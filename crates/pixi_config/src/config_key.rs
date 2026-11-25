use std::{iter::once, sync::Arc};

use itertools::Itertools;
use rattler_azure::AzureUrlError;
use rattler_conda_types::ChannelUrl;
use toml_edit::Key;
use url::Url;

use crate::{AzureEndpointKey, ContainerName, SetConfigError, consent_origin, invalid_value};

const AZURE_OPTIONS: &str = "azure-options";
const S3_OPTIONS: &str = "s3-options";
const AZURE_AUTH: &str = "auth";
const AZURE_SCHEME: &str = "scheme";
const VIRTUAL_PACKAGE_DETECTORS: &str = "virtual-package-detectors";
const CONSENT: &str = "consent";

/// A configuration key, parsed into the entry it addresses.
#[derive(Debug, Clone)]
pub struct ConfigKey {
    raw: String,
    kind: ConfigKeyKind,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfigKeyKind {
    Azure(AzureKey),
    S3(S3Key),
    Other(OtherKey),
}

/// A key outside `azure-options` and `s3-options`, as the segments it was
/// written with.
#[derive(Debug, Clone)]
pub(crate) struct OtherKey {
    parents: Vec<String>,
    target: String,
}

/// A key below `azure-options`, with the endpoint read from every segment
/// between the table and a trailing field.
#[derive(Debug, Clone)]
pub(crate) enum AzureKey {
    Table,
    Endpoint(AzureName<AzureEndpointKey>),
    /// `<endpoint>.auth`: the table of every container grant on the endpoint.
    AuthTable(AzureName<AzureEndpointKey>),
    Field {
        endpoint: AzureName<AzureEndpointKey>,
        field: AzureField,
    },
}

/// An endpoint or container as an `azure-options` key writes it, which need
/// not be valid for the key to name an entry in a file.
#[derive(Debug, Clone)]
pub(crate) enum AzureName<T> {
    Valid(T),
    Invalid {
        written: String,
        error: Arc<AzureUrlError>,
    },
}

/// A settable field of an `azure-options` entry; a bare `auth` is not one.
#[derive(Debug, Clone)]
pub(crate) enum AzureField {
    /// `<endpoint>.auth.<container>`: whether that container may be sent credentials.
    Auth(AzureName<ContainerName>),
    /// `<endpoint>.scheme`: the scheme `az://` is rewritten to.
    Scheme,
}

/// A key below `s3-options`, with the bucket read from every segment between
/// the table and a trailing field.
#[derive(Debug, Clone)]
pub(crate) enum S3Key {
    Table,
    Bucket {
        bucket: S3Bucket,
        spelling: BucketSpelling,
    },
    Field {
        bucket: S3Bucket,
        field: S3Field,
    },
}

/// The name of an s3 bucket, which is never empty.
#[derive(Debug, Clone)]
pub(crate) struct S3Bucket(String);

/// How a bucket's name was written in a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BucketSpelling {
    /// One segment, as in `s3-options."my.bucket"`.
    Segment,
    /// Several bare segments joined by their dots, as in `s3-options.my.bucket`.
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum S3Field {
    EndpointUrl,
    Region,
    AddressingStyle,
}

/// The path of the entry a [`ConfigKey`] addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPath {
    parents: Vec<KeySegment>,
    target: KeySegment,
}

/// One segment of a [`ConfigPath`], typed by what a file key must parse to in
/// order to spell it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySegment {
    Name(String),
    AzureEndpoint(AzureEndpointKey),
    ConsentOrigin(ChannelUrl),
}

/// A segment of a key as written, which only names a field when it is bare.
enum Segment {
    Bare(String),
    Quoted(String),
}

impl ConfigKey {
    pub fn parse(raw: &str) -> Result<Self, SetConfigError> {
        let segments = toml_segments(raw)?;
        let kind = ConfigKeyKind::from_segments(raw, segments)?;
        Ok(Self {
            raw: raw.to_string(),
            kind,
        })
    }

    /// The key as it was written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub(crate) fn kind(&self) -> &ConfigKeyKind {
        &self.kind
    }

    /// The path of the entry this key addresses, with each endpoint and
    /// consent origin in the form the configuration serializes it.
    pub fn path(&self) -> ConfigPath {
        self.kind.path()
    }

    /// The key with each segment of its path rendered as a TOML key.
    pub fn canonical(&self) -> String {
        self.path().to_string()
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl ConfigPath {
    fn new(root: &str, rest: Vec<KeySegment>) -> Self {
        let mut segments = once(KeySegment::Name(root.to_string()))
            .chain(rest)
            .collect_vec();
        let target = segments.pop().expect("a path has its root");
        Self {
            parents: segments,
            target,
        }
    }

    pub fn parents(&self) -> &[KeySegment] {
        &self.parents
    }

    pub fn target(&self) -> &KeySegment {
        &self.target
    }

    /// The first segment of the path.
    pub fn root(&self) -> &KeySegment {
        self.parents.first().unwrap_or(&self.target)
    }

    pub fn segments(&self) -> impl Iterator<Item = &KeySegment> {
        self.parents.iter().chain(once(&self.target))
    }
}

impl std::fmt::Display for ConfigPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rendered = self
            .segments()
            .map(|segment| Key::new(segment.to_string()).display_repr().into_owned())
            .join(".");
        f.write_str(&rendered)
    }
}

impl KeySegment {
    /// The segment's name, when it is spelled by name alone.
    pub fn as_name(&self) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name),
            Self::AzureEndpoint(_) | Self::ConsentOrigin(_) => None,
        }
    }
}

impl std::fmt::Display for KeySegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            Self::AzureEndpoint(endpoint) => write!(f, "{endpoint}"),
            Self::ConsentOrigin(origin) => f.write_str(origin.as_str()),
        }
    }
}

fn toml_segments(raw: &str) -> Result<Vec<Segment>, SetConfigError> {
    let keys = Key::parse(raw).map_err(|source| SetConfigError::InvalidKey {
        key: raw.to_string(),
        source: Box::new(source),
    })?;
    Ok(keys.iter().map(Segment::from_key).collect())
}

impl Segment {
    fn from_key(key: &Key) -> Self {
        let name = key.get().to_string();
        if key.display_repr().starts_with(['"', '\'']) {
            Self::Quoted(name)
        } else {
            Self::Bare(name)
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Bare(name) | Self::Quoted(name) => name,
        }
    }

    fn is_bare(&self, field: &str) -> bool {
        matches!(self, Self::Bare(name) if name == field)
    }
}

fn joined(segments: &[Segment]) -> String {
    segments.iter().map(Segment::name).join(".")
}

impl ConfigKeyKind {
    fn from_segments(raw: &str, segments: Vec<Segment>) -> Result<Self, SetConfigError> {
        match segments.split_first() {
            Some((family, rest)) if family.name() == AZURE_OPTIONS => {
                Ok(Self::Azure(AzureKey::parse(rest)))
            }
            Some((family, rest)) if family.name() == S3_OPTIONS => {
                S3Key::parse(raw, rest).map(Self::S3)
            }
            _ => OtherKey::parse(raw, segments).map(Self::Other),
        }
    }

    fn path(&self) -> ConfigPath {
        match self {
            Self::Azure(key) => ConfigPath::new(AZURE_OPTIONS, key.path()),
            Self::S3(key) => ConfigPath::new(S3_OPTIONS, key.path()),
            Self::Other(key) => key.path(),
        }
    }
}

impl OtherKey {
    fn parse(raw: &str, segments: Vec<Segment>) -> Result<Self, SetConfigError> {
        let mut names = segments
            .iter()
            .map(|segment| segment.name().to_string())
            .collect_vec();
        let target = names.pop().ok_or_else(|| SetConfigError::EmptyKey {
            key: raw.to_string(),
        })?;
        Ok(Self {
            parents: names,
            target,
        })
    }

    fn path(&self) -> ConfigPath {
        ConfigPath {
            parents: self.parents.iter().map(|parent| name(parent)).collect(),
            target: self.target_segment(),
        }
    }

    fn target_segment(&self) -> KeySegment {
        let names_consent = self.parents == [VIRTUAL_PACKAGE_DETECTORS, CONSENT];
        names_consent
            .then(|| channel_url(&self.target))
            .flatten()
            .map(|origin| KeySegment::ConsentOrigin(consent_origin(&origin)))
            .unwrap_or_else(|| name(&self.target))
    }
}

fn channel_url(written: &str) -> Option<ChannelUrl> {
    Url::parse(written)
        .ok()
        .filter(|url| !url.cannot_be_a_base())
        .map(ChannelUrl::from)
}

impl AzureKey {
    fn parse(segments: &[Segment]) -> Self {
        match segments {
            [] => Self::Table,
            [endpoint @ .., auth, container]
                if !endpoint.is_empty() && auth.is_bare(AZURE_AUTH) =>
            {
                Self::Field {
                    endpoint: AzureName::endpoint(endpoint),
                    field: AzureField::Auth(AzureName::parse(container.name(), ContainerName::new)),
                }
            }
            [endpoint @ .., auth] if !endpoint.is_empty() && auth.is_bare(AZURE_AUTH) => {
                Self::AuthTable(AzureName::endpoint(endpoint))
            }
            [endpoint @ .., scheme] if !endpoint.is_empty() && scheme.is_bare(AZURE_SCHEME) => {
                Self::Field {
                    endpoint: AzureName::endpoint(endpoint),
                    field: AzureField::Scheme,
                }
            }
            endpoint => Self::Endpoint(AzureName::endpoint(endpoint)),
        }
    }

    fn path(&self) -> Vec<KeySegment> {
        match self {
            Self::Table => Vec::new(),
            Self::Endpoint(endpoint) => vec![endpoint.endpoint_segment()],
            Self::AuthTable(endpoint) => vec![endpoint.endpoint_segment(), name(AZURE_AUTH)],
            Self::Field { endpoint, field } => once(endpoint.endpoint_segment())
                .chain(field.path())
                .collect(),
        }
    }
}

impl<T> AzureName<T> {
    fn parse(written: &str, parse: impl FnOnce(&str) -> Result<T, AzureUrlError>) -> Self {
        match parse(written) {
            Ok(parsed) => Self::Valid(parsed),
            Err(error) => Self::Invalid {
                written: written.to_string(),
                error: Arc::new(error),
            },
        }
    }

    /// The parsed name, if it is one a value can be set for.
    pub(crate) fn settable(&self, key: &str) -> Result<&T, SetConfigError> {
        match self {
            Self::Valid(parsed) => Ok(parsed),
            Self::Invalid { error, .. } => Err(invalid_value(key)(error.clone())),
        }
    }

    fn segment(&self, valid: impl FnOnce(&T) -> KeySegment) -> KeySegment {
        match self {
            Self::Valid(parsed) => valid(parsed),
            Self::Invalid { written, .. } => name(written),
        }
    }
}

impl AzureName<AzureEndpointKey> {
    fn endpoint(segments: &[Segment]) -> Self {
        Self::parse(&joined(segments), AzureEndpointKey::parse)
    }

    fn endpoint_segment(&self) -> KeySegment {
        self.segment(|endpoint| KeySegment::AzureEndpoint(endpoint.clone()))
    }
}

fn name(name: &str) -> KeySegment {
    KeySegment::Name(name.to_string())
}

impl AzureField {
    fn path(&self) -> Vec<KeySegment> {
        match self {
            Self::Auth(container) => vec![
                name(AZURE_AUTH),
                container.segment(|container| name(&container.to_string())),
            ],
            Self::Scheme => vec![name(AZURE_SCHEME)],
        }
    }
}

impl S3Key {
    fn parse(raw: &str, segments: &[Segment]) -> Result<Self, SetConfigError> {
        let Some((last, bucket)) = segments.split_last() else {
            return Ok(Self::Table);
        };
        if let Some(field) = S3Field::named_by(last).filter(|_| !bucket.is_empty()) {
            return Ok(Self::Field {
                bucket: S3Bucket::from_segments(raw, bucket)?,
                field,
            });
        }
        let spelling = if bucket.is_empty() {
            BucketSpelling::Segment
        } else {
            BucketSpelling::Dotted
        };
        Ok(Self::Bucket {
            bucket: S3Bucket::from_segments(raw, segments)?,
            spelling,
        })
    }

    fn path(&self) -> Vec<KeySegment> {
        match self {
            Self::Table => Vec::new(),
            Self::Bucket { bucket, .. } => vec![name(bucket.as_str())],
            Self::Field { bucket, field } => vec![name(bucket.as_str()), name(field.as_str())],
        }
    }
}

impl S3Bucket {
    fn from_segments(raw: &str, segments: &[Segment]) -> Result<Self, SetConfigError> {
        let bucket = joined(segments);
        if bucket.is_empty() {
            return Err(SetConfigError::EmptyS3Bucket {
                key: raw.to_string(),
            });
        }
        Ok(Self(bucket))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The names that end at one of this name's dots.
    pub(crate) fn dotted_prefixes(&self) -> impl Iterator<Item = &str> {
        self.0.match_indices('.').map(|(end, _)| &self.0[..end])
    }

    /// The field this name's last dotted label reads as, ignoring case and
    /// `_` in place of `-`.
    pub(crate) fn field_in_last_label(&self) -> Option<S3Field> {
        let (_, last) = self.0.rsplit_once('.')?;
        let normalized = last.to_lowercase().replace('_', "-");
        S3Field::ALL
            .into_iter()
            .find(|field| field.as_str() == normalized)
    }
}

impl std::fmt::Display for S3Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl S3Field {
    const ALL: [Self; 3] = [Self::EndpointUrl, Self::Region, Self::AddressingStyle];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EndpointUrl => "endpoint-url",
            Self::Region => "region",
            Self::AddressingStyle => "addressing-style",
        }
    }

    fn named_by(segment: &Segment) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| segment.is_bare(field.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> String {
        ConfigKey::parse(raw).unwrap().path().to_string()
    }

    fn error(raw: &str) -> String {
        ConfigKey::parse(raw).unwrap_err().to_string()
    }

    #[test]
    fn quoted_and_unquoted_azure_endpoints_address_the_same_entry() {
        for raw in [
            "azure-options.acct.blob.core.windows.net.auth.releases",
            "azure-options.\"acct.blob.core.windows.net\".auth.releases",
        ] {
            assert_eq!(
                path(raw),
                r#"azure-options."acct.blob.core.windows.net".auth.releases"#
            );
        }
        for raw in [
            "azure-options.acct.blob.core.windows.net.scheme",
            "azure-options.\"acct.blob.core.windows.net\".scheme",
        ] {
            assert_eq!(
                path(raw),
                r#"azure-options."acct.blob.core.windows.net".scheme"#
            );
        }
        for raw in [
            "azure-options.acct.blob.core.windows.net",
            "azure-options.\"acct.blob.core.windows.net\"",
        ] {
            assert_eq!(path(raw), r#"azure-options."acct.blob.core.windows.net""#);
        }
    }

    #[test]
    fn non_canonical_azure_endpoints_address_the_canonical_entry() {
        for raw in [
            "azure-options.\"acct.blob.core.windows.net.\".auth.releases",
            "azure-options.\"ACCT.Blob.Core.Windows.Net\".auth.releases",
        ] {
            assert_eq!(
                path(raw),
                r#"azure-options."acct.blob.core.windows.net".auth.releases"#
            );
        }
    }

    #[test]
    fn azure_containers_may_be_named_like_fields() {
        insta::assert_snapshot!(
            path("azure-options.acct.blob.core.windows.net.auth.scheme"),
            @r#"azure-options."acct.blob.core.windows.net".auth.scheme"#
        );
        insta::assert_snapshot!(
            path("azure-options.acct.blob.core.windows.net.auth.auth"),
            @r#"azure-options."acct.blob.core.windows.net".auth.auth"#
        );
        assert!(matches!(
            ConfigKey::parse("azure-options.acct.blob.core.windows.net.auth.scheme")
                .unwrap()
                .kind(),
            ConfigKeyKind::Azure(AzureKey::Field {
                field: AzureField::Auth(_),
                ..
            })
        ));
    }

    #[test]
    fn azure_auth_table_is_a_key_of_its_own() {
        assert!(matches!(
            ConfigKey::parse("azure-options.acct.blob.core.windows.net.auth")
                .unwrap()
                .kind(),
            ConfigKeyKind::Azure(AzureKey::AuthTable(AzureName::Valid(_)))
        ));
    }

    #[test]
    fn invalid_azure_endpoints_keep_their_spelling() {
        insta::assert_snapshot!(
            path("azure-options.not-an-endpoint.auth.releases"),
            @"azure-options.not-an-endpoint.auth.releases"
        );
        insta::assert_snapshot!(path("azure-options.auth"), @"azure-options.auth");
    }

    #[test]
    fn quoted_and_unquoted_s3_buckets_address_the_same_entry() {
        for raw in ["s3-options.my.bucket", "s3-options.\"my.bucket\""] {
            assert_eq!(path(raw), r#"s3-options."my.bucket""#);
        }
        for raw in [
            "s3-options.my.bucket.region",
            "s3-options.\"my.bucket\".region",
        ] {
            assert_eq!(path(raw), r#"s3-options."my.bucket".region"#);
        }
    }

    #[test]
    fn s3_buckets_may_be_named_like_fields() {
        for raw in ["s3-options.region", "s3-options.\"region\""] {
            assert!(matches!(
                ConfigKey::parse(raw).unwrap().kind(),
                ConfigKeyKind::S3(S3Key::Bucket {
                    spelling: BucketSpelling::Segment,
                    ..
                })
            ));
        }
        assert!(matches!(
            ConfigKey::parse("s3-options.my.\"region\"").unwrap().kind(),
            ConfigKeyKind::S3(S3Key::Bucket {
                spelling: BucketSpelling::Dotted,
                ..
            })
        ));
    }

    #[test]
    fn s3_bucket_names_read_their_dotted_prefixes_and_field_like_labels() {
        let bucket = S3Bucket("my.bucket.endpoint_url".to_string());
        assert_eq!(bucket.dotted_prefixes().collect_vec(), ["my", "my.bucket"]);
        assert_eq!(bucket.field_in_last_label(), Some(S3Field::EndpointUrl));
        assert_eq!(S3Bucket("bucket".to_string()).field_in_last_label(), None);
    }

    #[test]
    fn invalid_keys_are_rejected() {
        insta::assert_snapshot!(error("s3-options.\"\""), @r#"`s3-options.""` names an s3 bucket with an empty name"#);
        insta::assert_snapshot!(error("s3-options.\"\".region"), @r#"`s3-options."".region` names an s3 bucket with an empty name"#);
        insta::assert_snapshot!(error("not a key"), @"invalid key `not a key`");
        insta::assert_snapshot!(error(""), @"invalid key ``");
    }

    #[test]
    fn other_keys_keep_their_segments() {
        insta::assert_snapshot!(
            ConfigKey::parse("\"default-channels\"").unwrap().canonical(),
            @"default-channels"
        );
        insta::assert_snapshot!(
            ConfigKey::parse("repodata-config.\"https://prefix.dev/c\".disable-zstd")
                .unwrap()
                .canonical(),
            @r#"repodata-config."https://prefix.dev/c".disable-zstd"#
        );
    }

    #[test]
    fn consent_origins_address_their_normalized_entry() {
        assert_eq!(
            ConfigKey::parse(
                "virtual-package-detectors.consent.\"https://user:secret@prefix.dev/t/token/c\""
            )
            .unwrap()
            .path()
            .target(),
            &KeySegment::ConsentOrigin(consent_origin(
                &channel_url("https://prefix.dev/c").unwrap()
            ))
        );
    }
}
