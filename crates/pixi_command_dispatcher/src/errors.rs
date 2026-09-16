use std::{borrow::Borrow, collections::BTreeMap, path::PathBuf, sync::Arc};

use miette::Diagnostic;
use pixi_record::VariantValue;
use pixi_spec::{SourceLocationSpec, SpecConversionError};
use rattler_conda_types::{
    ChannelUrl, ConvertSubdirError, InvalidPackageNameError, PackageName, ParseChannelError,
};
use rattler_repodata_gateway::RunExportExtractorError;
use thiserror::Error;

use crate::{
    BackendSourceBuildError, BuildBackendMetadataError, InstallPixiEnvironmentError,
    InstantiateBackendError, PackageNotProvidedError,
    build::{DependenciesError, pin_compatible::PinCompatibleError},
    cycle::Cycle,
    solve_conda::SolveCondaEnvironmentError,
};
use pixi_compute_sources::SourceCheckoutError;

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SourceBuildError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceCheckout(#[from] SourceCheckoutError),

    #[error(transparent)]
    CreateWorkDirectory(Arc<std::io::Error>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Discovery(Arc<pixi_build_discovery::DiscoveryError>),

    #[error("could not initialize the build-backend")]
    Initialize(
        #[diagnostic_source]
        #[source]
        InstantiateBackendError,
    ),

    #[error("failed to create the build environment directory")]
    CreateBuildEnvironmentDirectory(#[source] Arc<std::io::Error>),

    #[error("failed to install the build environment")]
    InstallBuildEnvironment(#[source] Arc<InstallPixiEnvironmentError>),

    #[error("failed to install the host environment")]
    InstallHostEnvironment(#[source] Arc<InstallPixiEnvironmentError>),

    #[error(
        "The build backend does not provide an output matching '{name}' with variants: {}.",
        format_variants(variants)
    )]
    MissingOutput {
        name: String,
        variants: BTreeMap<String, VariantValue>,
    },

    #[error(
        "The build backend returned a path for the build package ({0}), but the path does not exist."
    )]
    MissingOutputFile(PathBuf),

    #[error("backend returned a dependency on an invalid package name")]
    InvalidPackageName(#[source] Arc<InvalidPackageNameError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PinCompatibleError(#[from] PinCompatibleError),

    #[error(
        "the build backend returned an unresolved `pin-subpackage` spec for '{}'",
        .0.as_normalized()
    )]
    UnresolvedPinSubpackage(PackageName),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BackendBuildError(#[from] BackendSourceBuildError),

    #[error("failed to amend run exports for {0} environment")]
    RunExportsExtraction(String, #[source] Arc<RunExportExtractorError>),

    #[error("failed to read metadata from the output package")]
    ReadIndexJson(#[source] Arc<rattler_package_streaming::ExtractError>),

    #[error("failed to calculate sha256 hash of {}", .0.display())]
    CalculateSha256(std::path::PathBuf, #[source] Arc<std::io::Error>),

    #[error("the package does not contain a valid subdir")]
    ConvertSubdir(#[source] Arc<ConvertSubdirError>),

    #[error(transparent)]
    GlobSet(Arc<pixi_glob::GlobSetError>),

    #[error("not built: {} failed to build", format_dependencies(dependencies))]
    DependencyFailed {
        package: PackageName,
        dependencies: Vec<PackageName>,
    },
}

/// One source package that could not be built, with the message and hint
/// that apply to it.
#[derive(Debug, Clone, Error, Diagnostic)]
#[error("failed to build '{}' from '{}'", package.as_source(), manifest_source)]
pub struct SourceBuildFailure {
    pub package: PackageName,
    pub manifest_source: Box<pixi_record::PinnedSourceSpec>,
    #[diagnostic_source]
    #[source]
    pub error: SourceBuildError,
    #[help]
    pub help: Option<String>,
}

impl SourceBuildFailure {
    /// The dependencies whose failure is the only reason this package was
    /// not built.
    fn failed_dependencies(&self) -> Option<&[PackageName]> {
        match &self.error {
            SourceBuildError::DependencyFailed { dependencies, .. } => Some(dependencies),
            _ => None,
        }
    }
}

/// Every source package that failed while building a set of them together.
/// Holding the first failure apart from the rest keeps the collection
/// non-empty by construction.
#[derive(Debug, Clone, Error)]
pub struct SourceBuildFailures {
    first: SourceBuildFailure,
    rest: Vec<SourceBuildFailure>,
}

impl SourceBuildFailures {
    pub fn new(first: SourceBuildFailure, rest: Vec<SourceBuildFailure>) -> Self {
        Self { first, rest }
    }

    /// `None` when `failures` is empty and there is nothing to report.
    pub fn from_vec(failures: Vec<SourceBuildFailure>) -> Option<Self> {
        let mut failures = failures.into_iter();
        let first = failures.next()?;
        Some(Self::new(first, failures.collect()))
    }

    fn all(&self) -> impl Iterator<Item = &SourceBuildFailure> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// Packages that failed on their own account.
    fn failed(&self) -> impl Iterator<Item = &SourceBuildFailure> {
        self.all().filter(|f| f.failed_dependencies().is_none())
    }

    /// Packages left unbuilt because something they depend on failed.
    fn skipped(&self) -> impl Iterator<Item = (&PackageName, &[PackageName])> {
        self.all()
            .filter_map(|f| Some((&f.package, f.failed_dependencies()?)))
    }
}

impl std::fmt::Display for SourceBuildFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failed = self.failed().count();
        let skipped = self.skipped().count();
        if failed == 0 {
            return write!(
                f,
                "{skipped} source {} skipped because their dependencies failed to build",
                plural_packages(skipped)
            );
        }
        write!(
            f,
            "{failed} source {} failed to build",
            plural_packages(failed)
        )?;
        if skipped > 0 {
            write!(f, ", {skipped} skipped")?;
        }
        Ok(())
    }
}

impl Diagnostic for SourceBuildFailures {
    fn related(&self) -> Option<Box<dyn Iterator<Item = &dyn Diagnostic> + '_>> {
        Some(Box::new(self.failed().map(|f| f as &dyn Diagnostic)))
    }

    fn help(&self) -> Option<Box<dyn std::fmt::Display + '_>> {
        let lines = self
            .skipped()
            .map(|(package, dependencies)| {
                format!(
                    "skipped {}: depends on failed {}",
                    package.as_source(),
                    format_dependencies(dependencies)
                )
            })
            .collect::<Vec<_>>();
        (!lines.is_empty()).then(|| Box::new(lines.join("\n")) as Box<dyn std::fmt::Display>)
    }
}

fn plural_packages(count: usize) -> &'static str {
    if count == 1 { "package" } else { "packages" }
}

fn format_dependencies(dependencies: &[PackageName]) -> String {
    dependencies
        .iter()
        .map(|name| format!("`{}`", name.as_source()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<InvalidPackageNameError> for SourceBuildError {
    fn from(err: InvalidPackageNameError) -> Self {
        Self::InvalidPackageName(Arc::new(err))
    }
}

impl From<pixi_glob::GlobSetError> for SourceBuildError {
    fn from(err: pixi_glob::GlobSetError) -> Self {
        Self::GlobSet(Arc::new(err))
    }
}

impl From<pixi_build_discovery::DiscoveryError> for SourceBuildError {
    fn from(err: pixi_build_discovery::DiscoveryError) -> Self {
        Self::Discovery(Arc::new(err))
    }
}

impl From<DependenciesError> for SourceBuildError {
    fn from(value: DependenciesError) -> Self {
        match value {
            DependenciesError::InvalidPackageName(error) => {
                SourceBuildError::InvalidPackageName(error)
            }
            DependenciesError::PinCompatibleError(error) => {
                SourceBuildError::PinCompatibleError(error)
            }
            DependenciesError::UnresolvedPinSubpackage(name) => {
                SourceBuildError::UnresolvedPinSubpackage(name)
            }
        }
    }
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SourceMetadataError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    BuildBackendMetadata(#[from] BuildBackendMetadataError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceRecord(#[from] SourceRecordError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageNotProvided(#[from] PackageNotProvidedError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceCheckout(#[from] SourceCheckoutError),
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SourceRecordError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    BuildBackendMetadata(#[from] BuildBackendMetadataError),

    #[error("failed to amend run exports for {0} environment")]
    RunExportsExtraction(String, #[source] Arc<RunExportExtractorError>),

    #[error("failed to solve the build environment for package '{}'", package.as_source())]
    SolveBuildEnvironment {
        package: PackageName,
        #[diagnostic_source]
        #[source]
        error: Box<SolvePixiEnvironmentError>,
    },

    #[error("failed to solve the host environment for package '{}'", package.as_source())]
    SolveHostEnvironment {
        package: PackageName,
        #[diagnostic_source]
        #[source]
        error: Box<SolvePixiEnvironmentError>,
    },

    #[error(transparent)]
    SpecConversionError(Arc<SpecConversionError>),

    #[error(transparent)]
    InvalidPackageName(Arc<InvalidPackageNameError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PinCompatibleError(#[from] PinCompatibleError),

    #[error(
        "the build backend returned an unresolved `pin-subpackage` spec for '{}'",
        .0.as_normalized()
    )]
    UnresolvedPinSubpackage(PackageName),

    #[error("found two source dependencies for {} but for different sources ({source1} and {source2})", package.as_source()
    )]
    DuplicateSourceDependency {
        package: PackageName,
        source1: Box<SourceLocationSpec>,
        source2: Box<SourceLocationSpec>,
    },

    #[error("the dependencies of some packages in the environment form a cycle")]
    Cycle(Cycle),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageNotProvided(#[from] PackageNotProvidedError),

    #[error(
        "no output with matching variants found for package '{package}' at '{manifest_path}', available outputs: {available}"
    )]
    NoMatchingVariant {
        package: String,
        manifest_path: String,
        available: String,
    },

    /// Pinning or checking out the source failed.
    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceCheckout(#[from] SourceCheckoutError),
}

impl From<SpecConversionError> for SourceRecordError {
    fn from(err: SpecConversionError) -> Self {
        Self::SpecConversionError(Arc::new(err))
    }
}

impl From<InvalidPackageNameError> for SourceRecordError {
    fn from(err: InvalidPackageNameError) -> Self {
        Self::InvalidPackageName(Arc::new(err))
    }
}

impl From<DependenciesError> for SourceRecordError {
    fn from(value: DependenciesError) -> Self {
        match value {
            DependenciesError::InvalidPackageName(error) => {
                SourceRecordError::InvalidPackageName(error)
            }
            DependenciesError::PinCompatibleError(error) => {
                SourceRecordError::PinCompatibleError(error)
            }
            DependenciesError::UnresolvedPinSubpackage(name) => {
                SourceRecordError::UnresolvedPinSubpackage(name)
            }
        }
    }
}

/// An error that might be returned when solving a pixi environment.
#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SolvePixiEnvironmentError {
    #[error(transparent)]
    QueryError(Arc<rattler_repodata_gateway::GatewayError>),

    #[error("failed to solve the environment")]
    SolveError(#[source] Arc<rattler_solve::SolveError>),

    #[error("failed to read the package cache")]
    CacheIndexError(#[source] Arc<std::io::Error>),

    #[error(transparent)]
    SpecConversionError(Arc<SpecConversionError>),

    #[error("detected a cyclic dependency:\n\n{0}")]
    Cycle(Cycle),

    #[error(transparent)]
    ParseChannelError(Arc<ParseChannelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    MissingChannel(MissingChannelError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DevSourceMetadataError(crate::DevSourceMetadataError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceCheckoutError(SourceCheckoutError),

    /// [`SourceMetadata`](crate::SourceMetadata) error surfaced
    /// directly from [`SourceMetadataKey`](crate::keys::SourceMetadataKey).
    #[error(transparent)]
    #[diagnostic(transparent)]
    SourceMetadata(SourceMetadataError),

    /// Resolving a source package that is part of the environment failed.
    /// Names the package and its source location so failures deep inside
    /// nested build/host environments can be traced back to the package
    /// they belong to.
    #[error("failed to resolve source package '{}' (at '{source}')", name.as_source())]
    ResolveSourcePackage {
        name: PackageName,
        source: Box<SourceLocationSpec>,
        #[diagnostic_source]
        #[source]
        error: Box<SourceRecordError>,
    },
}

impl SolvePixiEnvironmentError {
    /// Returns the backend discovery failure this solve error ultimately
    /// stems from, if any. Walks the typed error chain, including the solve
    /// errors of nested build and host environments.
    pub fn discovery_error(&self) -> Option<&pixi_build_discovery::DiscoveryError> {
        match self {
            SolvePixiEnvironmentError::SourceMetadata(err) => err.discovery_error(),
            SolvePixiEnvironmentError::DevSourceMetadataError(err) => err.discovery_error(),
            SolvePixiEnvironmentError::ResolveSourcePackage { error, .. } => {
                error.discovery_error()
            }
            _ => None,
        }
    }
}

impl SourceMetadataError {
    /// Returns the backend discovery failure this error ultimately stems
    /// from, if any.
    pub fn discovery_error(&self) -> Option<&pixi_build_discovery::DiscoveryError> {
        match self {
            SourceMetadataError::BuildBackendMetadata(err) => err.discovery_error(),
            SourceMetadataError::SourceRecord(err) => err.discovery_error(),
            _ => None,
        }
    }
}

impl SourceRecordError {
    /// Returns the backend discovery failure this error ultimately stems
    /// from, if any.
    pub fn discovery_error(&self) -> Option<&pixi_build_discovery::DiscoveryError> {
        match self {
            SourceRecordError::BuildBackendMetadata(err) => err.discovery_error(),
            SourceRecordError::SolveBuildEnvironment { error, .. }
            | SourceRecordError::SolveHostEnvironment { error, .. } => error.discovery_error(),
            _ => None,
        }
    }
}

impl From<SourceMetadataError> for SolvePixiEnvironmentError {
    fn from(err: SourceMetadataError) -> Self {
        // Preserve cycle-error identity when the SourceMetadata error
        // ultimately wraps a SourceRecord cycle, so callers of the new
        // path still see `SolvePixiEnvironmentError::Cycle(..)` and
        // not a generic source-metadata error.
        match err {
            SourceMetadataError::SourceRecord(SourceRecordError::Cycle(cycle)) => {
                SolvePixiEnvironmentError::Cycle(cycle)
            }
            other => SolvePixiEnvironmentError::SourceMetadata(other),
        }
    }
}

impl From<rattler_repodata_gateway::GatewayError> for SolvePixiEnvironmentError {
    fn from(err: rattler_repodata_gateway::GatewayError) -> Self {
        Self::QueryError(Arc::new(err))
    }
}

impl From<rattler_solve::SolveError> for SolvePixiEnvironmentError {
    fn from(err: rattler_solve::SolveError) -> Self {
        Self::SolveError(Arc::new(err))
    }
}

impl From<SpecConversionError> for SolvePixiEnvironmentError {
    fn from(err: SpecConversionError) -> Self {
        Self::SpecConversionError(Arc::new(err))
    }
}

impl From<ParseChannelError> for SolvePixiEnvironmentError {
    fn from(err: ParseChannelError) -> Self {
        Self::ParseChannelError(Arc::new(err))
    }
}

/// An error for a missing channel in the solve request
#[derive(Debug, Clone, Diagnostic, Error)]
#[error("Package '{package}' requested unavailable channel '{channel}'")]
pub struct MissingChannelError {
    pub package: String,
    pub channel: ChannelUrl,
    #[help]
    pub advice: Option<String>,
}

impl Borrow<dyn Diagnostic> for Box<SolvePixiEnvironmentError> {
    fn borrow(&self) -> &(dyn Diagnostic + 'static) {
        self.as_ref()
    }
}

impl Borrow<dyn Diagnostic> for Box<SourceRecordError> {
    fn borrow(&self) -> &(dyn Diagnostic + 'static) {
        self.as_ref()
    }
}

impl From<SolveCondaEnvironmentError> for SolvePixiEnvironmentError {
    fn from(err: SolveCondaEnvironmentError) -> Self {
        match err {
            SolveCondaEnvironmentError::SolveError(err) => {
                SolvePixiEnvironmentError::SolveError(Arc::new(err))
            }
            SolveCondaEnvironmentError::SpecConversionError(err) => {
                SolvePixiEnvironmentError::SpecConversionError(Arc::new(err))
            }
            SolveCondaEnvironmentError::Gateway(err) => {
                SolvePixiEnvironmentError::QueryError(Arc::new(err))
            }
        }
    }
}

impl From<crate::DevSourceMetadataError> for SolvePixiEnvironmentError {
    fn from(err: crate::DevSourceMetadataError) -> Self {
        Self::DevSourceMetadataError(err)
    }
}

/// Formats a variant map as `key=value` pairs for error messages.
fn format_variants(variants: &BTreeMap<String, VariantValue>) -> String {
    if variants.is_empty() {
        return "none".to_string();
    }
    variants
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuildBackendMetadataError;

    fn discovery_failure() -> pixi_build_discovery::DiscoveryError {
        pixi_build_discovery::DiscoveryError::FailedToDiscover {
            path: "/some/source".to_string(),
            help: "help".to_string(),
        }
    }

    fn metadata_error() -> BuildBackendMetadataError {
        BuildBackendMetadataError::Discovery(Arc::new(discovery_failure()))
    }

    fn failure(package: &str, error: SourceBuildError) -> SourceBuildFailure {
        SourceBuildFailure {
            package: PackageName::new_unchecked(package),
            manifest_source: Box::new(
                pixi_record::PinnedPathSpec {
                    path: "some/path".into(),
                }
                .into(),
            ),
            error,
            help: None,
        }
    }

    fn dependency_failed(package: &str, dependency: &str) -> SourceBuildError {
        SourceBuildError::DependencyFailed {
            package: PackageName::new_unchecked(package),
            dependencies: vec![PackageName::new_unchecked(dependency)],
        }
    }

    #[test]
    fn failures_summarise_failed_and_skipped_separately() {
        let failures = SourceBuildFailures::from_vec(vec![
            failure("a", SourceBuildError::MissingOutputFile("out".into())),
            failure("b", dependency_failed("b", "a")),
            failure("c", dependency_failed("c", "a")),
        ])
        .expect("non-empty");

        assert_eq!(
            failures.to_string(),
            "1 source package failed to build, 2 skipped"
        );
        assert_eq!(failures.related().unwrap().count(), 1);
        assert_eq!(
            failures.help().unwrap().to_string(),
            "skipped b: depends on failed `a`\nskipped c: depends on failed `a`"
        );
    }

    #[test]
    fn no_failures_yields_nothing_to_report() {
        assert!(SourceBuildFailures::from_vec(Vec::new()).is_none());
    }

    #[test]
    fn discovery_error_is_found_behind_build_backend_metadata() {
        let err = SolvePixiEnvironmentError::SourceMetadata(
            SourceMetadataError::BuildBackendMetadata(metadata_error()),
        );
        assert!(matches!(
            err.discovery_error(),
            Some(pixi_build_discovery::DiscoveryError::FailedToDiscover { .. })
        ));
    }

    #[test]
    fn discovery_error_is_found_behind_source_record() {
        let err = SolvePixiEnvironmentError::SourceMetadata(SourceMetadataError::SourceRecord(
            SourceRecordError::BuildBackendMetadata(metadata_error()),
        ));
        assert!(err.discovery_error().is_some());
    }

    #[test]
    fn discovery_error_is_found_behind_nested_build_environment_solve() {
        // A discovery failure while solving the build environment of another
        // source dependency nests a full solve error inside the outer one.
        let inner = SolvePixiEnvironmentError::SourceMetadata(
            SourceMetadataError::BuildBackendMetadata(metadata_error()),
        );
        let err = SolvePixiEnvironmentError::SourceMetadata(SourceMetadataError::SourceRecord(
            SourceRecordError::SolveBuildEnvironment {
                package: PackageName::new_unchecked("some-package"),
                error: Box::new(inner),
            },
        ));
        assert!(err.discovery_error().is_some());
    }

    #[test]
    fn missing_output_error_prints_the_variants() {
        let mut variants = BTreeMap::new();
        variants.insert("python".to_string(), VariantValue::from("3.12".to_string()));
        let err = SourceBuildError::MissingOutput {
            name: "wusel".to_string(),
            variants,
        };

        insta::assert_snapshot!(
            err,
            @"The build backend does not provide an output matching 'wusel' with variants: python=3.12."
        );
    }

    #[test]
    fn discovery_error_is_found_behind_backend_initialization() {
        let err = SolvePixiEnvironmentError::SourceMetadata(
            SourceMetadataError::BuildBackendMetadata(BuildBackendMetadataError::Initialize(
                InstantiateBackendError::Discovery(Arc::new(discovery_failure())),
            )),
        );
        assert!(err.discovery_error().is_some());
    }

    #[test]
    fn unrelated_solve_error_has_no_discovery_error() {
        let err = SolvePixiEnvironmentError::Cycle(Cycle { stack: Vec::new() });
        assert!(err.discovery_error().is_none());
    }
}
