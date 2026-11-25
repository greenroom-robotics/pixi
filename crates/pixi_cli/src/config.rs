use crate::cli_config::WorkspaceConfig;
use clap::Parser;
use itertools::Itertools;
use miette::{IntoDiagnostic, WrapErr};
use pixi_config;
use pixi_config::{
    AzureEndpointKey, Config, ConfigError, ConfigKey, ConfigPath, GlobalConfigSource, KeySegment,
};
use pixi_consts::consts;
use pixi_core::WorkspaceLocator;
use pixi_core::workspace::WorkspaceLocatorError;
use pixi_manifest::toml::TomlDocument;
use pixi_toml_edit::{insert_array_element, push_array_element, remove_entry, upsert_entry};
use rattler_conda_types::{ChannelUrl, NamedChannelOrUrl};
use serde_json::Value as JsonValue;
use std::{
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
};
use toml_edit::{Array, DocumentMut, Item};

const DEFAULT_CHANNELS: &str = "default-channels";
const EXTRA_INDEX_URLS: &str = "pypi-config.extra-index-urls";
const EXCLUDE_NEWER: &str = "exclude-newer";
const VIRTUAL_PACKAGE_DETECTORS: &str = "virtual-package-detectors";
const CONSENT: &str = "consent";

#[derive(Parser, Debug)]
enum Subcommand {
    /// Edit the configuration file
    #[clap(alias = "e")]
    Edit(EditArgs),

    /// List configuration values
    ///
    /// Example: `pixi config list default-channels`
    #[clap(visible_alias = "ls", alias = "l")]
    List(ListArgs),

    /// Prepend a value to a list configuration key
    ///
    /// Example: `pixi config prepend default-channels bioconda`
    Prepend(PendArgs),

    /// Append a value to a list configuration key
    ///
    /// Example: `pixi config append default-channels bioconda`
    Append(PendArgs),

    /// Set a configuration value
    ///
    /// Example: `pixi config set default-channels '["conda-forge", "bioconda"]'`
    Set(SetArgs),

    /// Unset a configuration value
    ///
    /// Example: `pixi config unset default-channels`
    Unset(UnsetArgs),
}

#[derive(Parser, Debug, Clone)]
struct CommonArgs {
    /// Operation on project-local configuration
    #[arg(long, short, conflicts_with_all = &["global", "system", "shared", "path"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    local: bool,

    /// Operation on global configuration
    #[arg(long, short, conflicts_with_all = &["local", "system", "shared", "path"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    global: bool,

    /// Operation on system configuration
    #[arg(long, short, conflicts_with_all = &["local", "global", "shared", "path"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    system: bool,

    /// Operation on the user configuration shared with other rattler-based tools,
    /// which only accepts the keys every such tool understands
    #[arg(long, conflicts_with_all = &["local", "global", "system", "path"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    shared: bool,

    /// Path to a local configuration file
    #[arg(long, short, conflicts_with_all = &["local", "global", "system", "shared"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    path: Option<PathBuf>,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,
}

#[derive(Parser, Debug, Clone)]
struct EditArgs {
    #[clap(flatten)]
    common: CommonArgs,

    /// The editor to use, defaults to `EDITOR` environment variable or `nano` on Unix and `notepad` on Windows
    #[arg(env = "EDITOR")]
    pub editor: Option<String>,
}

#[derive(Parser, Debug, Clone)]
struct ListArgs {
    /// Configuration key to show (all if not provided)
    key: Option<String>,

    /// Output in JSON format
    #[arg(long)]
    json: bool,

    #[clap(flatten)]
    config_source: pixi_config::ConfigSourceCli,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct PendArgs {
    /// Configuration key to set
    key: String,

    /// Configuration value to (pre|ap)pend
    value: String,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct SetArgs {
    /// Configuration key to set
    key: String,

    /// Configuration value to set (key will be unset if value not provided)
    value: Option<String>,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct UnsetArgs {
    /// Configuration key to unset
    key: String,

    #[clap(flatten)]
    common: CommonArgs,
}

enum AlterMode {
    Prepend,
    Append,
    Set,
    Unset,
}

/// Manage configuration
#[derive(Parser, Debug)]
#[clap(arg_required_else_help = true)]
pub struct Args {
    #[clap(subcommand)]
    subcommand: Subcommand,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    match args.subcommand {
        Subcommand::Edit(args) => {
            let config_path = determine_config_write_path(&args.common).await?;

            let editor = args.editor.unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });

            let mut child = if cfg!(windows) {
                std::process::Command::new("cmd")
                    .arg("/C")
                    .arg(editor.as_str())
                    .arg(&config_path)
                    .spawn()
                    .into_diagnostic()?
            } else {
                std::process::Command::new(editor.as_str())
                    .arg(&config_path)
                    .spawn()
                    .into_diagnostic()?
            };
            child.wait().into_diagnostic()?;
        }
        Subcommand::List(args) => {
            let config = load_config(&args.common, &args.config_source.source()).await?;

            let out = if let Some(key) = args.key {
                let partial = partial_config(&config, &key)?;
                if args.json {
                    serde_json::to_string_pretty(&partial).into_diagnostic()?
                } else {
                    toml_edit::ser::to_string_pretty(&partial).into_diagnostic()?
                }
            } else if args.json {
                serde_json::to_string_pretty(&config).into_diagnostic()?
            } else {
                toml_edit::ser::to_string_pretty(&config).into_diagnostic()?
            };

            if out.is_empty() {
                eprintln!("Configuration not set");
            }
            pixi_utils::io::ignore_broken_pipe(writeln!(std::io::stdout(), "{out}"))
                .into_diagnostic()?;
        }
        Subcommand::Prepend(args) => {
            alter_config(
                &args.common,
                &args.key,
                Some(args.value),
                AlterMode::Prepend,
            )
            .await?
        }
        Subcommand::Append(args) => {
            alter_config(&args.common, &args.key, Some(args.value), AlterMode::Append).await?
        }
        Subcommand::Set(args) => {
            alter_config(&args.common, &args.key, args.value, AlterMode::Set).await?
        }
        Subcommand::Unset(args) => {
            alter_config(&args.common, &args.key, None, AlterMode::Unset).await?
        }
    };
    Ok(())
}

async fn determine_project_root(common_args: &CommonArgs) -> miette::Result<Option<PathBuf>> {
    let workspace = WorkspaceLocator::default()
        .with_closest_package(false) // Dont care about the package
        .with_consider_environment(true)
        .with_search_start(common_args.workspace_config.workspace_locator_start())
        .with_ignore_pixi_version_check(true)
        .with_lenient_config(true)
        .locate()
        .await;
    match workspace {
        Err(WorkspaceLocatorError::WorkspaceNotFound(_)) => {
            if common_args.local {
                return Err(miette::miette!(
                    "--local flag can only be used inside a pixi workspace but no workspace could be found",
                ));
            }
            Ok(None)
        }
        Err(e) => {
            if common_args.local {
                return Err(e).into_diagnostic().context("--local flag can only be used inside a pixi workspace but loading the workspace failed",);
            }
            Ok(None)
        }
        Ok(project) => Ok(Some(project.root().to_path_buf())),
    }
}

/// Load the configuration the given arguments select, taking the global layer
/// from `source`.
async fn load_config(
    common_args: &CommonArgs,
    source: &GlobalConfigSource,
) -> miette::Result<Config> {
    if common_args.system {
        return Ok(Config::load_system());
    }

    if common_args.shared {
        return Ok(Config::load_shared());
    }

    if common_args.global {
        return Ok(Config::load_global_with(source));
    }

    // If an explicit --path was given, load and merge that specific config file.
    if let Some(path) = &common_args.path {
        let base_config = Config::load_global_with(source);
        if global_source_contains_path(source, path) {
            return Ok(base_config);
        }

        let local_config = match Config::from_path(path) {
            Ok(config) => config,
            Err(ConfigError::FileNotFound(_)) => Config::default(),
            Err(error) => return Err(error.into()),
        };
        return Ok(base_config.merge_config(local_config));
    }

    // Otherwise, check if we are in a project/workspace root
    if let Some(root) = determine_project_root(common_args).await? {
        return Ok(Config::load_with(&root, source));
    }

    Ok(Config::load_global_with(source))
}

fn global_source_contains_path(source: &GlobalConfigSource, path: &Path) -> bool {
    match source {
        GlobalConfigSource::Search => pixi_config::config_search_locations()
            .iter()
            .any(|location| same_config_path(&location.path, path)),
        GlobalConfigSource::File(source_path) => same_config_path(source_path, path),
        GlobalConfigSource::None => false,
    }
}

fn same_config_path(left: &Path, right: &Path) -> bool {
    left == right
        || fs_err::canonicalize(left)
            .ok()
            .zip(fs_err::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn resolved_config_write_path(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut resolved) = fs_err::canonicalize(existing) {
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
        if existing.as_os_str().is_empty() {
            existing = Path::new(".");
        }
    }
}

async fn determine_config_write_path(common_args: &CommonArgs) -> miette::Result<PathBuf> {
    Ok(determine_config_write_destination(common_args).await?.0)
}

async fn determine_config_write_destination(
    common_args: &CommonArgs,
) -> miette::Result<(PathBuf, Option<PathBuf>)> {
    if let Some(path) = &common_args.path {
        let repository = determine_project_root(common_args).await?.filter(|root| {
            let local = root.join(consts::PIXI_DIR).join(consts::CONFIG_FILE);
            same_config_path(path, &local)
                || resolved_config_write_path(path)
                    .zip(resolved_config_write_path(&local))
                    .is_some_and(|(path, local)| path == local)
        });
        return Ok((path.clone(), repository));
    }

    if common_args.system {
        return Ok((pixi_config::config_path_system(), None));
    }

    if common_args.shared {
        return Ok((pixi_config::shared_user_config_write_path(), None));
    }

    if !common_args.global
        && let Some(root) = determine_project_root(common_args).await?
    {
        return Ok((
            root.join(consts::PIXI_DIR).join(consts::CONFIG_FILE),
            Some(root),
        ));
    }

    Ok((pixi_config::user_config_write_path(), None))
}

/// Alters a specific key in the user configuration file according to the given `mode`.
///
/// Handles reading the existing TOML document (or initializing a new one),
/// updating key values (including list modifications like `Prepend` and `Append`),
/// and persisting the formatted result back to the disk.
///
/// # Errors
///
/// - `key` is not a valid key.
/// - The target config path cannot be determined or created.
/// - The existing config file cannot be read or parsed as valid TOML.
/// - A list-only operation (`Prepend`/`Append`) is attempted on a non-list key.
/// - Persisting the updated content disk fails.
async fn alter_config(
    common_args: &CommonArgs,
    key: &str,
    value: Option<String>,
    mode: AlterMode,
) -> miette::Result<()> {
    let key = ConfigKey::parse(key)?;
    let (mut to, repository) = determine_config_write_destination(common_args).await?;
    let content = match fs_err::read_to_string(&to) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e)
                .into_diagnostic()
                .context("failed to read config file");
        }
    };

    let doc_mut = content
        .parse::<toml_edit::DocumentMut>()
        .into_diagnostic()
        .context("failed to parse TOML")?;

    let mut toml_doc = TomlDocument::new(doc_mut);

    // Edit only the file that is about to be written. Starting from the
    // merged config would bake every inherited setting into it, so the user
    // would silently stop following the layers they inherit from.
    let loaded = if common_args.shared {
        Config::from_shared_path(&to)
    } else {
        Config::from_path(&to)
    };
    let (mut config, loaded) = match loaded {
        Ok(config) => (config, true),
        Err(ConfigError::FileNotFound(_)) => (Config::default(), true),
        // `unset` only edits the document, so it can remove an invalid value
        Err(_) if matches!(mode, AlterMode::Unset) => (Config::default(), false),
        Err(e) => return Err(e.into()),
    };

    let repository_consent = repository.is_some() && edits_detector_consent(&key.path());
    if repository_consent && let Some(root) = &repository {
        to = pixi_config::repository_detector_config_path(root)?;
    }
    match mode {
        AlterMode::Prepend => {
            let input = value.expect("value must be provided");
            pend_config_key(
                common_args,
                &mut config,
                &mut toml_doc,
                &key,
                &input,
                ListEdit::Prepend,
            )
            .await?
        }
        AlterMode::Append => {
            let input = value.expect("value must be provided");
            pend_config_key(
                common_args,
                &mut config,
                &mut toml_doc,
                &key,
                &input,
                ListEdit::Append,
            )
            .await?
        }
        // An invalid file is not loaded, so the edit can't be validated and
        // the key is removed from the document like any other key.
        AlterMode::Set | AlterMode::Unset if loaded && edits_exclude_newer_entry(&key.path()) => {
            edit_exclude_newer(&mut config, &mut toml_doc, &key, value, &mode)?
        }
        AlterMode::Set => set_config_key(
            &mut config,
            &mut toml_doc,
            &key,
            SetValue::from(value),
            common_args.shared,
        )?,
        AlterMode::Unset => unset(&mut toml_doc, &key, common_args.shared)?,
    }

    let contents = toml_doc.to_string();
    let parent = to.parent().expect("config path should have a parent");
    fs_err::create_dir_all(parent)
        .into_diagnostic()
        .wrap_err(format!(
            "failed to create directories in '{}'",
            parent.display()
        ))?;
    fs_err::write(&to, contents)
        .into_diagnostic()
        .wrap_err(format!("failed to write config to '{}'", to.display()))?;

    eprintln!("✅ Updated config at {}", to.display());
    Ok(())
}

/// Whether `path` addresses the detector consent, which a repository keeps
/// in a file of its own.
fn edits_detector_consent(path: &ConfigPath) -> bool {
    let mut names = path.segments().map(KeySegment::as_name);
    names.next() == Some(Some(VIRTUAL_PACKAGE_DETECTORS))
        && matches!(names.next(), None | Some(Some(CONSENT)))
}

fn edits_exclude_newer_entry(path: &ConfigPath) -> bool {
    path.parents()
        .first()
        .is_some_and(|root| root.as_name() == Some(EXCLUDE_NEWER))
}

/// Edits a key inside `exclude-newer` through `rattler_config`, which matches
/// exemptions by their normalized package name and keeps the comments and
/// layout around the edited key.
///
/// # Errors
///
/// - `value` is not valid for `key`.
/// - `mode` is `Unset` and the document does not hold `key`.
fn edit_exclude_newer(
    config: &mut Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    value: Option<String>,
    mode: &AlterMode,
) -> miette::Result<()> {
    let canonical = key.canonical();
    let changed = rattler_config::edit::set_exclude_newer_in_document(
        toml_doc.as_table_mut(),
        &mut config.exclude_newer,
        &canonical,
        value,
    )
    .map_err(|error| pixi_config::SetConfigError::from_exclude_newer_edit(&canonical, error))?;
    if matches!(mode, AlterMode::Unset) && !changed {
        miette::bail!("key `{key}` not found in configuration file");
    }
    Ok(())
}

/// Which end of a list a value is added to.
#[derive(Debug, Clone, Copy)]
enum ListEdit {
    Prepend,
    Append,
}

impl ListEdit {
    fn insert<T>(self, list: &mut Vec<T>, item: T) {
        match self {
            Self::Prepend => list.insert(0, item),
            Self::Append => list.push(item),
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Prepend => "prepend",
            Self::Append => "append",
        }
    }
}

const LIST_KEYS: [&str; 2] = [DEFAULT_CHANNELS, EXTRA_INDEX_URLS];

/// Adds `input` to the list `key` addresses, in `config` and in the document.
///
/// # Errors
///
/// - `key` is not a list key, or `input` is not valid for it.
/// - Writing the list into the document fails.
async fn pend_config_key(
    common_args: &CommonArgs,
    config: &mut Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    input: &str,
    edit: ListEdit,
) -> miette::Result<()> {
    let transplant = match key.canonical().as_str() {
        DEFAULT_CHANNELS => {
            pend_default_channel(common_args, config, toml_doc, key, input, edit).await?
        }
        EXTRA_INDEX_URLS => {
            pend_extra_index_url(config, toml_doc, key, input, edit, common_args.shared)?
        }
        _ => miette::bail!(
            "{} is only supported for list keys: {}",
            edit.command(),
            LIST_KEYS.join(", ")
        ),
    };
    ensure_written(key, transplant)
}

/// `default-channels` replaces the lower layers rather than concatenating
/// them, so a file without its own list starts from the merged one; otherwise
/// lower layers would be silenced.
async fn pend_default_channel(
    common_args: &CommonArgs,
    config: &mut Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    input: &str,
    edit: ListEdit,
) -> miette::Result<Transplant> {
    let channel = NamedChannelOrUrl::from_str(input)
        .into_diagnostic()
        .context("invalid channel name")?;
    if !spellings_in(toml_doc.as_item(), key.path().target()).is_empty() {
        edit.insert(&mut config.default_channels, channel);
        return transplant_config_key(
            config,
            toml_doc,
            key,
            TransplantMode::List(edit),
            common_args.shared,
        );
    }
    let mut channels = load_config(common_args, &GlobalConfigSource::Search)
        .await?
        .default_channels;
    edit.insert(&mut channels, channel);
    config.default_channels = channels;
    transplant_config_key(
        config,
        toml_doc,
        key,
        TransplantMode::Replace,
        common_args.shared,
    )
}

/// `extra-index-urls` is concatenated across layers, so only this file's own
/// share of the list is edited, and a prepend lands after the lower layers.
fn pend_extra_index_url(
    config: &mut Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    input: &str,
    edit: ListEdit,
    shared: bool,
) -> miette::Result<Transplant> {
    let url = url::Url::parse(input).map_err(|e| miette::miette!("invalid URL: {}", e))?;
    edit.insert(&mut config.pypi_config.extra_index_urls, url);
    transplant_config_key(config, toml_doc, key, TransplantMode::List(edit), shared)
}

fn ensure_written(key: &ConfigKey, transplant: Transplant) -> miette::Result<()> {
    match transplant {
        Transplant::Written => Ok(()),
        Transplant::Omitted(_) => miette::bail!("nothing was written for `{key}`"),
    }
}

/// Whether an edit removed an entry from the document.
enum Removal {
    Removed,
    Absent,
}

/// What writing a key from a validated `Config` did to the document.
#[must_use]
enum Transplant {
    Written,
    /// The configuration does not serialize the key, so any entry is removed.
    Omitted(Removal),
}

/// How a transplanted entry is written into the document.
#[derive(Debug, Clone, Copy)]
enum TransplantMode {
    Replace,
    /// Adds the list's first or last element to the document's list.
    List(ListEdit),
}

/// The value `pixi config set` was given for a key.
enum SetValue {
    Value(String),
    Clear,
}

impl From<Option<String>> for SetValue {
    fn from(value: Option<String>) -> Self {
        value.map_or(Self::Clear, Self::Value)
    }
}

impl SetValue {
    fn to_option(&self) -> Option<String> {
        match self {
            Self::Value(value) => Some(value.clone()),
            Self::Clear => None,
        }
    }
}

/// Sets `key` to `value` in `config` and writes the entry it addresses into
/// the document.
///
/// # Errors
///
/// - `value` is not valid for `key`.
/// - Writing the entry into the document fails.
/// - `value` is not the default for `key`, yet the configuration does not
///   store it.
fn set_config_key(
    config: &mut Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    value: SetValue,
    shared: bool,
) -> miette::Result<()> {
    config.set_key(key, value.to_option())?;
    let transplant = transplant_config_key(config, toml_doc, key, TransplantMode::Replace, shared)?;
    match unstored_value(&value, &transplant) {
        Some(value) => ensure_default(key, value),
        None => Ok(()),
    }
}

/// The value of a set that left the document without an entry for its key.
fn unstored_value<'a>(value: &'a SetValue, transplant: &Transplant) -> Option<&'a str> {
    match (value, transplant) {
        (SetValue::Value(value), Transplant::Omitted(Removal::Absent)) => Some(value),
        _ => None,
    }
}

/// Succeeds when `value` is the default for `key`, which the configuration
/// does not store.
fn ensure_default(key: &ConfigKey, value: &str) -> miette::Result<()> {
    let mut default = Config::default();
    default.set_key(key, Some(value.to_string()))?;
    if find_entry(serialize(&default)?.as_item(), &key.path()).is_some() {
        miette::bail!(
            "nothing was written for `{key}`, as the configuration does not store `{value}` \
             there; edit the file with `pixi config edit` instead"
        );
    }
    Ok(())
}

/// Unset a key from the TOML document, preserving existing formatting and comments.
///
/// # Errors
///
/// - The specified key does not exist in the document.
/// - The unset operation leaves the config as invalid.
fn unset(toml_doc: &mut TomlDocument, key: &ConfigKey, shared: bool) -> miette::Result<()> {
    match remove_key(toml_doc, key, shared)? {
        Removal::Removed => Ok(()),
        Removal::Absent => miette::bail!("key `{key}` not found in configuration file"),
    }
}

/// Removes every spelling of the entry `key` addresses from the document,
/// along with the parent tables that leaves empty.
///
/// # Errors
///
/// - Navigating the document's tables fails.
/// - The removal leaves the config as invalid.
fn remove_key(
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    shared: bool,
) -> miette::Result<Removal> {
    let path = key.path();
    let parents = resolve_parent_keys(toml_doc, path.parents());
    let parent_table = nested_item_mut(toml_doc, &parents)?;
    let spellings = spellings_in(parent_table, path.target());
    for spelling in &spellings {
        remove_entry(parent_table, spelling).into_diagnostic()?;
    }
    prune_empty_parents(toml_doc, as_strs(&parents))?;
    if spellings.is_empty() {
        return Ok(Removal::Absent);
    }
    validate_document(toml_doc, shared)
        .wrap_err_with(|| format!("unsetting `{key}` would leave the config file invalid"))?;
    Ok(Removal::Removed)
}

fn validate_document(toml_doc: &TomlDocument, shared: bool) -> miette::Result<()> {
    let contents = toml_doc.to_string();
    if shared {
        Config::keys_not_shared(&contents)?;
    } else {
        Config::from_toml(&contents, None)?;
    }
    Ok(())
}

fn prune_empty_parents(toml_doc: &mut TomlDocument, mut path: Vec<&str>) -> miette::Result<()> {
    if path.is_empty() {
        return Ok(());
    }

    let is_empty = toml_doc
        .get_nested_table(&path)
        .map(|t| t.is_empty())
        .unwrap_or(false);

    if is_empty {
        let Some(key_target_to_remove) = path.pop() else {
            return Ok(());
        };

        let parent_of_target = if path.is_empty() {
            toml_doc.as_item_mut()
        } else {
            toml_doc
                .get_or_insert_nested_item(&path)
                .into_diagnostic()?
        };

        remove_entry(parent_of_target, key_target_to_remove).into_diagnostic()?;

        prune_empty_parents(toml_doc, path)?;
    }

    Ok(())
}

/// Transplants a single key from a validated `Config` into an editable TOML document.
/// Empty values omitted by serialization remove the corresponding entry.
///
/// We serialize the entire Config and parse it into a temporary document because:
/// 1. The input value undergoes strict type validation via Serde.
/// 2. We extract only the specific target leaf node, preventing unrequested default values.
///
/// # Errors
///
/// - Serializing `config` or parsing the temporary TOML document fails.
/// - The entry is not shared by all rattler-based tools while `shared` is set.
/// - Navigating or creating nested parent tables in `toml_doc` fails.
/// - Removing an omitted entry fails, as [`remove_key`] does.
fn transplant_config_key(
    config: &Config,
    toml_doc: &mut TomlDocument,
    key: &ConfigKey,
    mode: TransplantMode,
    shared: bool,
) -> miette::Result<Transplant> {
    let path = key.path();
    let serialized = serialize(config)?;
    let Some(entry) = find_entry(serialized.as_item(), &path) else {
        return remove_key(toml_doc, key, shared).map(Transplant::Omitted);
    };
    if shared {
        ensure_shared(&serialized, path.root())?;
    }
    let parents = resolve_parent_keys(toml_doc, path.parents());
    match (mode, entry.as_value().and_then(|value| value.as_array())) {
        (TransplantMode::List(edit), Some(list)) => {
            edit_document_list(toml_doc, &parents, path.target(), edit, list)?
        }
        _ => write_entry(toml_doc, &parents, path.target(), entry)?,
    }
    Ok(Transplant::Written)
}

fn serialize(config: &Config) -> miette::Result<DocumentMut> {
    toml_edit::ser::to_string(config)
        .into_diagnostic()?
        .parse::<DocumentMut>()
        .into_diagnostic()
}

/// The entry at `path` in `item`, under whichever spellings `item` uses.
fn find_entry<'a>(item: &'a Item, path: &ConfigPath) -> Option<&'a Item> {
    path.segments().try_fold(item, |item, segment| {
        let spelling = spellings_in(item, segment).into_iter().next()?;
        item.get(&spelling)
    })
}

/// Validates the serialized section being edited, not unrelated foreign keys
/// on disk, keeping required siblings such as an S3 bucket's region.
fn ensure_shared(serialized: &DocumentMut, root: &KeySegment) -> miette::Result<()> {
    let spelling = spellings_in(serialized.as_item(), root)
        .into_iter()
        .next()
        .expect("the serialized key has a root");
    let mut selected = DocumentMut::new();
    selected[&spelling] = serialized[&spelling].clone();
    let not_shared = Config::keys_not_shared(&selected.to_string())?;
    if !not_shared.is_empty() {
        return Err(miette::miette!(
            "'{}' is not a key shared by all rattler-based tools, so it cannot be written to \
             the shared configuration. Use `--global` to set it for pixi alone.",
            not_shared.join("', '")
        ));
    }
    Ok(())
}

/// Adds the first or last element of `list` to the document's list at
/// `target`, keeping the formatting of the list already there.
fn edit_document_list(
    toml_doc: &mut TomlDocument,
    parents: &[String],
    target: &KeySegment,
    edit: ListEdit,
    list: &Array,
) -> miette::Result<()> {
    let list_key = spellings_in(nested_item_mut(toml_doc, parents)?, target)
        .into_iter()
        .next()
        .unwrap_or_else(|| new_target_spelling(target));
    let document_list = toml_doc
        .get_or_insert_toml_array_mut(&as_strs(parents), &list_key)
        .into_diagnostic()?;
    match edit {
        ListEdit::Prepend => {
            if let Some(element) = list.get(0) {
                insert_array_element(document_list, 0, element.clone());
            }
        }
        ListEdit::Append => {
            if let Some(element) = list.iter().last() {
                push_array_element(document_list, element.clone());
            }
        }
    }
    Ok(())
}

/// Writes `entry` at `target` below `parents`, replacing every other spelling
/// of `target` so that exactly one remains.
fn write_entry(
    toml_doc: &mut TomlDocument,
    parents: &[String],
    target: &KeySegment,
    entry: &Item,
) -> miette::Result<()> {
    let target_table = nested_item_mut(toml_doc, parents)?;
    let spellings = spellings_in(target_table, target);
    let target_key =
        reusable_spelling(&spellings, target).unwrap_or_else(|| new_target_spelling(target));
    for spelling in spellings.iter().filter(|spelling| **spelling != target_key) {
        remove_entry(target_table, spelling).into_diagnostic()?;
    }
    insert_entry(target_table, &target_key, entry)
}

fn insert_entry(table: &mut Item, key: &str, entry: &Item) -> miette::Result<()> {
    if let Some(value) = entry.as_value() {
        upsert_entry(table, key, value.clone()).into_diagnostic()?;
    } else if let Some(entry_table) = entry.as_table()
        && let Some(table_like) = table.as_table_like_mut()
    {
        table_like.insert(key, Item::Table(entry_table.clone()));
    }
    Ok(())
}

/// The spelling already in the document that an entry may keep. A consent
/// origin keeps only a spelling without credentials.
fn reusable_spelling(spellings: &[String], target: &KeySegment) -> Option<String> {
    match target {
        KeySegment::ConsentOrigin(origin) => spellings
            .iter()
            .find(|spelling| channel_url(spelling).is_some_and(|channel| channel == *origin))
            .cloned(),
        KeySegment::Name(name) => spellings
            .iter()
            .find(|spelling| Some(spelling.as_str()) != legacy_alias(name).as_deref())
            .cloned(),
        KeySegment::AzureEndpoint(_) => spellings.first().cloned(),
    }
}

fn nested_item_mut<'a>(
    toml_doc: &'a mut TomlDocument,
    parents: &[String],
) -> miette::Result<&'a mut Item> {
    if parents.is_empty() {
        return Ok(toml_doc.as_item_mut());
    }
    toml_doc
        .get_or_insert_nested_item(&as_strs(parents))
        .into_diagnostic()
}

fn as_strs(strings: &[String]) -> Vec<&str> {
    strings.iter().map(String::as_str).collect()
}

/// The keys of `item` that spell `segment`, the preferred spelling first.
fn spellings_in(item: &Item, segment: &KeySegment) -> Vec<String> {
    let written = item
        .as_table_like()
        .into_iter()
        .flat_map(|table| table.iter())
        .map(|(key, _)| key)
        .collect_vec();
    spellings_among(&written, segment)
}

/// The keys of `written` that spell `segment`, the preferred spelling first.
fn spellings_among(written: &[&str], segment: &KeySegment) -> Vec<String> {
    match segment {
        KeySegment::Name(name) => key_spellings(name)
            .into_iter()
            .filter(|spelling| written.contains(&spelling.as_str()))
            .collect(),
        KeySegment::AzureEndpoint(endpoint) => written_matching(written, |key| {
            AzureEndpointKey::parse(key).is_ok_and(|key| key == *endpoint)
        }),
        KeySegment::ConsentOrigin(origin) => written_matching(written, |key| {
            channel_url(key).is_some_and(|channel| pixi_config::consent_origin(&channel) == *origin)
        }),
    }
}

fn written_matching(written: &[&str], matches: impl Fn(&str) -> bool) -> Vec<String> {
    written
        .iter()
        .filter(|key| matches(key))
        .map(|key| key.to_string())
        .collect()
}

fn channel_url(written: &str) -> Option<ChannelUrl> {
    url::Url::parse(written)
        .ok()
        .filter(|url| !url.cannot_be_a_base())
        .map(ChannelUrl::from)
}

/// The spellings a key segment may have in a configuration file: as given,
/// its legacy snake_case alias, and for a channel URL its canonical form,
/// which is how the configuration serializes it.
fn key_spellings(segment: &str) -> Vec<String> {
    let mut spellings = vec![segment.to_string()];
    spellings.extend(legacy_alias(segment));
    spellings.extend(canonical_channel_url(segment));
    spellings.extend(slashed_channel_url(segment));
    spellings
}

/// The spelling the configuration serializes a channel URL segment with,
/// when it differs from `segment`.
fn canonical_channel_url(segment: &str) -> Option<String> {
    let canonical = channel_url(segment)?
        .as_str()
        .trim_end_matches('/')
        .to_string();
    (canonical != segment).then_some(canonical)
}

fn slashed_channel_url(segment: &str) -> Option<String> {
    let channel = channel_url(segment)?;
    Some(format!("{}/", channel.as_str().trim_end_matches('/')))
}

/// The spelling a new parent table takes for `segment`.
fn new_parent_spelling(segment: &KeySegment) -> String {
    match segment {
        KeySegment::Name(name) => canonical_channel_url(name)
            .and_then(|_| slashed_channel_url(name))
            .unwrap_or_else(|| name.clone()),
        KeySegment::AzureEndpoint(_) | KeySegment::ConsentOrigin(_) => new_target_spelling(segment),
    }
}

/// The spelling a new entry takes for `segment`.
fn new_target_spelling(segment: &KeySegment) -> String {
    match segment {
        KeySegment::Name(name) => canonical_channel_url(name).unwrap_or_else(|| name.clone()),
        KeySegment::AzureEndpoint(endpoint) => endpoint.to_string(),
        KeySegment::ConsentOrigin(origin) => origin.as_str().trim_end_matches('/').to_string(),
    }
}

/// Returns the legacy `snake_case` alias for a canonical `kebab-case` key or parent table
/// if the one exists in the Serde schema
fn legacy_alias(key: &str) -> Option<String> {
    match key {
        "default-channels"
        | "authentication-override-file"
        | "tls-no-verify"
        | "repodata-config"
        | "change-ps1"
        | "disable-bzip2"
        | "disable-sharded"
        | "disable-zstd" => Some(key.replace('-', "_")),
        _ => None,
    }
}

/// Resolve parent key paths against the TOML document, taking the spelling
/// already on disk when there is one.
fn resolve_parent_keys(doc: &TomlDocument, parents: &[KeySegment]) -> Vec<String> {
    let mut current_item = doc.as_item();
    parents
        .iter()
        .map(|parent| {
            let chosen = spellings_in(current_item, parent)
                .into_iter()
                .next()
                .unwrap_or_else(|| new_parent_spelling(parent));
            current_item = current_item.get(&chosen).unwrap_or(&Item::None);
            chosen
        })
        .collect()
}

/// Extract the selected value, wrapped in its parent tables.
fn partial_config(config: &Config, key: &str) -> miette::Result<JsonValue> {
    let path = ConfigKey::parse(key)?.path();
    if !is_known_key(config.get_keys(), &path) {
        return Err(miette::miette!(
            "unknown key: {}\nSupported keys:\n\t{}",
            console::style(key).red(),
            config.get_keys().join(",\n\t")
        ));
    }

    // TOML omits unset fields instead of serializing them as null.
    let value: JsonValue =
        toml_edit::de::from_str(&toml_edit::ser::to_string(config).into_diagnostic()?)
            .into_diagnostic()?;
    Ok(select_json(value, &path).unwrap_or_else(|| JsonValue::Object(Default::default())))
}

/// The entry of `value` at `path`, wrapped in its parent tables as `value`
/// spells them.
fn select_json(mut value: JsonValue, path: &ConfigPath) -> Option<JsonValue> {
    let mut spelled = Vec::new();
    for segment in path.segments() {
        let written = value
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
            .map(String::as_str)
            .collect_vec();
        let spelling = spellings_among(&written, segment).into_iter().next()?;
        value = value.get_mut(&spelling)?.take();
        spelled.push(spelling);
    }
    Some(spelled.into_iter().rev().fold(value, |value, spelling| {
        JsonValue::Object([(spelling, value)].into_iter().collect())
    }))
}

/// Match supported keys, with each `<...>` placeholder matching one segment.
fn is_known_key(known_keys: &[&str], path: &ConfigPath) -> bool {
    known_keys.iter().any(|known| {
        let known_segments = known.split('.').collect_vec();
        known_segments.len() == path.segments().count()
            && known_segments
                .iter()
                .zip(path.segments())
                .all(|(known, segment)| known.starts_with('<') || segment.as_name() == Some(known))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixi_config::DetectorDecision;

    struct TestContext {
        pub config_path: PathBuf,
        pub common_args: CommonArgs,
        pub _temp_dir: tempfile::TempDir,
    }

    impl TestContext {
        fn read_config(&self) -> String {
            let config_read_result = fs_err::read_to_string(&self.config_path);

            config_read_result.expect("Should be able to read the config file after update")
        }

        fn setup(config_content: Option<&str>) -> Self {
            let temp_dir = tempfile::tempdir().unwrap();
            let project_root = temp_dir.path();

            let config_path = project_root.join("config.toml");
            fs_err::write(&config_path, config_content.unwrap_or("")).unwrap();

            let common_args = CommonArgs {
                local: false,
                global: false,
                system: false,
                shared: false,
                path: Some(config_path.clone()),
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(temp_dir.path().to_path_buf()),
                    ..Default::default()
                },
            };

            Self {
                _temp_dir: temp_dir,
                common_args,
                config_path,
            }
        }
    }

    async fn execute_subcommand(subcommand: Subcommand) {
        let args = Args { subcommand };
        let result = execute(args).await;

        result.expect("The subcommand execution failed");
    }

    #[tokio::test]
    async fn consent_revocation_removes_equivalent_origins() {
        let original = concat!(
            "[virtual-package-detectors.consent]\n",
            "\"https://prefix.dev/channel\" = \"allow\"\n",
            "\"https://prefix.dev/channel/\" = \"allow\"\n",
            "\"https://user:secret@prefix.dev/t/token/channel\" = \"allow\"\n",
            "\"https://prefix.dev/other\" = \"allow\"\n",
        );
        let origin = ChannelUrl::from(url::Url::parse("https://prefix.dev/channel").unwrap());
        for mode in [AlterMode::Set, AlterMode::Unset] {
            let context = TestContext::setup(Some(original));
            let expected = matches!(mode, AlterMode::Set).then_some(DetectorDecision::Deny);
            alter_config(
                &context.common_args,
                "virtual-package-detectors.consent.\"https://prefix.dev/channel\"",
                expected.map(|_| "deny".to_string()),
                mode,
            )
            .await
            .unwrap();
            let config = Config::from_path(&context.config_path).unwrap();
            assert_eq!(config.virtual_package_detectors.consent(&origin), expected);
            let other = ChannelUrl::from(url::Url::parse("https://prefix.dev/other").unwrap());
            assert_eq!(
                config.virtual_package_detectors.consent(&other),
                Some(DetectorDecision::Allow)
            );
            assert!(!context.read_config().contains("secret"));
        }
    }

    #[tokio::test]
    async fn exclude_newer_keys_are_edited_inside_the_value() {
        let context = TestContext::setup(Some("exclude-newer = \"7d\"\n"));
        let edit = |key: &'static str, value: Option<&'static str>| {
            let common_args = context.common_args.clone();
            async move {
                let mode = if value.is_some() {
                    AlterMode::Set
                } else {
                    AlterMode::Unset
                };
                alter_config(&common_args, key, value.map(str::to_string), mode).await
            }
        };

        // An exemption is added to a bare cutoff.
        edit("exclude-newer.exemptions.ruff", Some("*"))
            .await
            .unwrap();
        assert_eq!(
            context.read_config(),
            "exclude-newer = { cutoff = \"7d\", exemptions = { ruff = \"*\" } }\n"
        );

        // An exemption written in a different case replaces it, keeping
        // its name as written.
        edit("exclude-newer.exemptions.Ruff", Some("==1"))
            .await
            .unwrap();
        assert_eq!(
            context.read_config(),
            "exclude-newer = { cutoff = \"7d\", exemptions = { ruff = \"==1\" } }\n"
        );

        // The cutoff is kept when the last exemption is removed.
        edit("exclude-newer.cutoff", Some("14d")).await.unwrap();
        edit("exclude-newer.exemptions.ruff", None).await.unwrap();
        assert_eq!(context.read_config(), "exclude-newer = \"14d\"\n");

        // Invalid values are rejected and leave the file unchanged.
        edit("exclude-newer.cutoff", Some("yesterday"))
            .await
            .unwrap_err();
        edit("exclude-newer.exemptions.ruff.bogus", Some("*"))
            .await
            .unwrap_err();
        assert_eq!(context.read_config(), "exclude-newer = \"14d\"\n");

        // Unsetting an exemption that isn't there fails like any missing key.
        edit("exclude-newer.exemptions.numpy", None)
            .await
            .unwrap_err();
        assert_eq!(context.read_config(), "exclude-newer = \"14d\"\n");
    }

    #[tokio::test]
    async fn exclude_newer_edits_keep_comments_and_layout() {
        let original = concat!(
            "[exclude-newer]\n",
            "cutoff = \"7d\" # a week\n",
            "[exclude-newer.exemptions]\n",
            "# keep ruff current\n",
            "ruff = \"*\"\n",
        );
        let context = TestContext::setup(Some(original));
        alter_config(
            &context.common_args,
            "exclude-newer.exemptions.numpy",
            Some(">=2".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        assert_eq!(
            context.read_config(),
            format!("{original}numpy = \">=2\"\n")
        );
        alter_config(
            &context.common_args,
            "exclude-newer.exemptions.numpy",
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();
        assert_eq!(context.read_config(), original);

        // Without exemptions, the table is kept with its comments.
        alter_config(
            &context.common_args,
            "exclude-newer.exemptions.ruff",
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();
        assert_eq!(
            context.read_config(),
            "[exclude-newer]\ncutoff = \"7d\" # a week\n"
        );
    }

    #[tokio::test]
    async fn invalid_exclude_newer_exemption_is_unset_alone() {
        let context = TestContext::setup(Some(
            "exclude-newer = { cutoff = \"7d\", exemptions = { ruff = \"*\", bad = \"\" } }\n",
        ));
        alter_config(
            &context.common_args,
            "exclude-newer.exemptions.bad",
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();
        assert_eq!(
            context.read_config(),
            "exclude-newer = { cutoff = \"7d\", exemptions = { ruff = \"*\"} }\n"
        );
    }

    #[tokio::test]
    async fn authenticated_consent_keys_are_saved_without_credentials() {
        let context = TestContext::setup(None);
        alter_config(
            &context.common_args,
            "virtual-package-detectors.consent.\"https://user:secret@prefix.dev/t/token/channel\"",
            Some("deny".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let config = Config::from_path(&context.config_path).unwrap();
        let origin = ChannelUrl::from(url::Url::parse("https://prefix.dev/channel").unwrap());
        assert_eq!(
            config.virtual_package_detectors.consent(&origin),
            Some(DetectorDecision::Deny)
        );
        assert!(!context.read_config().contains("secret"));
        assert!(!context.read_config().contains("/t/token/"));
    }

    #[tokio::test]
    async fn shared_edits_ignore_foreign_value_types() {
        let mut context = TestContext::setup(Some(
            "shell = \"foreign-tool-setting\"\ntls-no-verify = false\n",
        ));
        context.common_args.shared = true;
        alter_config(
            &context.common_args,
            "tls-no-verify",
            Some("true".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        assert_eq!(
            Config::from_shared_path(&context.config_path)
                .unwrap()
                .tls_no_verify,
            Some(true)
        );
        alter_config(
            &context.common_args,
            "tls-no-verify",
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();
        let document = context.read_config().parse::<DocumentMut>().unwrap();
        assert_eq!(document["shell"].as_str(), Some("foreign-tool-setting"));
        assert!(document.get("tls-no-verify").is_none());
    }

    #[tokio::test]
    async fn channel_consent_round_trips_across_url_spellings() {
        let context = TestContext::setup(Some("default-channels = [\"conda-forge\"]\n"));
        let origin = ChannelUrl::from(url::Url::parse("https://prefix.dev/conda-forge").unwrap());
        let typed = "virtual-package-detectors.consent.\"https://prefix.dev/conda-forge/\"";
        alter_config(
            &context.common_args,
            typed,
            Some("allow".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let config = Config::from_path(&context.config_path).unwrap();
        assert_eq!(
            config.virtual_package_detectors.consent(&origin),
            Some(DetectorDecision::Allow)
        );
        assert_eq!(
            partial_config(&config, typed).unwrap(),
            serde_json::json!({
                "virtual-package-detectors": {
                    "consent": {"https://prefix.dev/conda-forge": "allow"}
                }
            })
        );
        alter_config(
            &context.common_args,
            "virtual-package-detectors.consent.\"https://prefix.dev/conda-forge\"",
            Some("deny".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let config = Config::from_path(&context.config_path).unwrap();
        assert_eq!(
            config.virtual_package_detectors.consent(&origin),
            Some(DetectorDecision::Deny)
        );
        assert_eq!(config.virtual_package_detectors.consent.len(), 1);
        alter_config(&context.common_args, typed, None, AlterMode::Unset)
            .await
            .unwrap();
        let config = Config::from_path(&context.config_path).unwrap();
        assert_eq!(config.virtual_package_detectors.consent(&origin), None);
        assert_eq!(
            config.default_channels,
            vec![NamedChannelOrUrl::from_str("conda-forge").unwrap()]
        );
    }

    #[tokio::test]
    async fn empty_detector_config_revokes_consent() {
        let origin = ChannelUrl::from(url::Url::parse("https://prefix.dev/conda-forge").unwrap());
        for key in [
            "virtual-package-detectors.consent",
            "virtual-package-detectors",
        ] {
            let context = TestContext::setup(Some(
                r#"default-channels = ["conda-forge"]
[virtual-package-detectors.consent]
"https://prefix.dev/conda-forge" = "allow"
"#,
            ));
            alter_config(
                &context.common_args,
                key,
                Some("{}".to_string()),
                AlterMode::Set,
            )
            .await
            .unwrap();
            let config = Config::from_path(&context.config_path).unwrap();
            assert_eq!(config.virtual_package_detectors.consent(&origin), None);
            assert_eq!(
                config.default_channels,
                vec![NamedChannelOrUrl::from_str("conda-forge").unwrap()]
            );
        }
    }

    #[tokio::test]
    async fn repository_consent_edits_follow_the_destination() {
        for (local_scope, explicit_path) in [(true, false), (false, false), (false, true)] {
            local_consent_edits_preserve_unselected_channels(local_scope, explicit_path).await;
        }
    }

    async fn local_consent_edits_preserve_unselected_channels(
        local_scope: bool,
        explicit_path: bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        let config_home = directory.path().join("user-config");
        fs_err::create_dir_all(&repository).unwrap();
        fs_err::write(
            repository.join("pixi.toml"),
            "[workspace]\nname = \"consent-test\"\nchannels = []\n",
        )
        .unwrap();
        let args = CommonArgs {
            local: local_scope,
            global: false,
            system: false,
            shared: false,
            path: explicit_path
                .then(|| repository.join(consts::PIXI_DIR).join(consts::CONFIG_FILE)),
            workspace_config: WorkspaceConfig {
                manifest_path: Some(repository.join("pixi.toml")),
                ..Default::default()
            },
        };
        let first = ChannelUrl::from(url::Url::parse("https://prefix.dev/first").unwrap());
        let second = ChannelUrl::from(url::Url::parse("https://prefix.dev/second").unwrap());
        let local = repository.join(consts::PIXI_DIR).join(consts::CONFIG_FILE);
        temp_env::async_with_vars(
            [
                ("XDG_CONFIG_HOME", Some(config_home.as_os_str())),
                ("APPDATA", Some(config_home.as_os_str())),
            ],
            async {
                pixi_config::write_detector_decision(&local, &first, DetectorDecision::Allow)
                    .unwrap();
                pixi_config::write_detector_decision(&local, &second, DetectorDecision::Allow)
                    .unwrap();
                alter_config(
                    &args,
                    "virtual-package-detectors.timeout-seconds",
                    Some("10".to_string()),
                    AlterMode::Set,
                )
                .await
                .unwrap();
                let source = GlobalConfigSource::None;
                let config = Config::load_with(&repository, &source);
                assert_eq!(
                    config.virtual_package_detectors.consent(&first),
                    Some(DetectorDecision::Allow)
                );
                assert_eq!(
                    config.virtual_package_detectors.consent(&second),
                    Some(DetectorDecision::Allow)
                );
                let key = "virtual-package-detectors.consent.\"https://prefix.dev/first/\"";
                alter_config(&args, key, Some("deny".to_string()), AlterMode::Set)
                    .await
                    .unwrap();
                let config = Config::load_with(&repository, &source);
                assert_eq!(
                    config.virtual_package_detectors.consent(&first),
                    Some(DetectorDecision::Deny)
                );
                assert_eq!(
                    config.virtual_package_detectors.consent(&second),
                    Some(DetectorDecision::Allow)
                );
                alter_config(&args, key, None, AlterMode::Unset)
                    .await
                    .unwrap();
                assert_eq!(
                    Config::load_with(&repository, &source)
                        .virtual_package_detectors
                        .consent(&first),
                    None
                );
                pixi_config::write_detector_decision(&local, &first, DetectorDecision::Allow)
                    .unwrap();
                assert_eq!(
                    Config::load_with(&repository, &source)
                        .virtual_package_detectors
                        .consent(&first),
                    Some(DetectorDecision::Allow)
                );
                alter_config(
                    &args,
                    "virtual-package-detectors.consent",
                    Some(r#"{"https://prefix.dev/second/": "allow"}"#.to_string()),
                    AlterMode::Set,
                )
                .await
                .unwrap();
                let config = Config::load_with(&repository, &source);
                assert_eq!(config.virtual_package_detectors.consent(&first), None);
                assert_eq!(
                    config.virtual_package_detectors.consent(&second),
                    Some(DetectorDecision::Allow)
                );
                alter_config(
                    &args,
                    "virtual-package-detectors",
                    Some(r#"{"timeout-seconds": 20}"#.to_string()),
                    AlterMode::Set,
                )
                .await
                .unwrap();
                assert_eq!(
                    Config::load_with(&repository, &source)
                        .virtual_package_detectors
                        .consent(&second),
                    None
                );
            },
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_consent_edits_cannot_modify_linked_external_configs() {
        for link_kind in ["directory", "file", "hardlink"] {
            for (local_scope, explicit_path) in [(true, false), (false, false), (false, true)] {
                let directory = tempfile::tempdir().unwrap();
                let repository = directory.path().join("repository");
                let external = directory.path().join("external");
                let config_home = directory.path().join("user-config");
                fs_err::create_dir_all(&repository).unwrap();
                fs_err::create_dir_all(&external).unwrap();
                fs_err::write(
                    repository.join("pixi.toml"),
                    "[workspace]\nname = \"consent-test\"\nchannels = []\n",
                )
                .unwrap();
                let target = external.join(consts::CONFIG_FILE);
                let original = "offline = true\n";
                fs_err::write(&target, original).unwrap();
                let local_directory = repository.join(consts::PIXI_DIR);
                let local = local_directory.join(consts::CONFIG_FILE);
                if link_kind == "directory" {
                    std::os::unix::fs::symlink(&external, &local_directory).unwrap();
                } else {
                    fs_err::create_dir(&local_directory).unwrap();
                    if link_kind == "file" {
                        std::os::unix::fs::symlink(&target, &local).unwrap();
                    } else {
                        fs_err::hard_link(&target, &local).unwrap();
                    }
                }
                let args = CommonArgs {
                    local: local_scope,
                    global: false,
                    system: false,
                    shared: false,
                    path: explicit_path.then_some(local),
                    workspace_config: WorkspaceConfig {
                        manifest_path: Some(repository.join("pixi.toml")),
                        ..Default::default()
                    },
                };
                temp_env::async_with_vars(
                    [
                        ("XDG_CONFIG_HOME", Some(config_home.as_os_str())),
                        ("APPDATA", Some(config_home.as_os_str())),
                    ],
                    async {
                        assert!(
                            alter_config(
                                &args,
                                "virtual-package-detectors.consent.\"https://prefix.dev/first\"",
                                Some("allow".to_string()),
                                AlterMode::Set,
                            )
                            .await
                            .is_err(),
                            "{link_kind}, local={local_scope}, explicit={explicit_path}"
                        );
                        assert_eq!(fs_err::read_to_string(&target).unwrap(), original);
                    },
                )
                .await;
            }
        }
    }

    #[tokio::test]
    async fn test_determine_config_write_path() {
        let test_context = TestContext::setup(None);
        let mut config_path = test_context.config_path.clone();

        let mut config_write_path = determine_config_write_path(&test_context.common_args)
            .await
            .expect("Determine config write path should have succeeded");

        if cfg!(target_os = "macos") {
            config_path = config_path
                .canonicalize()
                .expect("Failed to canonicalize temp directory path");

            config_write_path = config_write_path
                .canonicalize()
                .expect("Failed to canonicalize temp directory path");
        }

        assert_eq!(config_write_path, config_path);
    }

    fn shared_args() -> CommonArgs {
        CommonArgs {
            local: false,
            global: false,
            system: false,
            shared: true,
            path: None,
            workspace_config: WorkspaceConfig::default(),
        }
    }

    /// `--shared` writes the user-level rattler file and only accepts keys
    /// every rattler-based tool understands.
    #[tokio::test]
    async fn shared_writes_the_rattler_file_and_refuses_pixi_keys() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_home = temp_dir.path().join("config-home");
        let rattler_home = config_home.join("rattler");
        fs_err::create_dir_all(&rattler_home).unwrap();
        fs_err::write(rattler_home.join("config.toml"), "").unwrap();
        temp_env::async_with_vars(
            [
                ("XDG_CONFIG_HOME", Some(config_home.to_str().unwrap())),
                ("RATTLER_HOME", Some(rattler_home.to_str().unwrap())),
            ],
            async {
                let args = shared_args();
                let path = determine_config_write_path(&args).await.unwrap();
                assert_eq!(path, rattler_home.join("config.toml"));

                alter_config(
                    &args,
                    "virtual-package-detectors.consent.\"https://conda.anaconda.org/conda-forge\"",
                    Some("allow".to_string()),
                    AlterMode::Set,
                )
                .await
                .unwrap();
                let origin = ChannelUrl::from(
                    url::Url::parse("https://conda.anaconda.org/conda-forge").unwrap(),
                );
                assert_eq!(
                    Config::from_shared_path(&path)
                        .unwrap()
                        .virtual_package_detectors
                        .consent(&origin),
                    Some(DetectorDecision::Allow)
                );

                assert!(
                    alter_config(
                        &args,
                        "shell.change-ps1",
                        Some("false".to_string()),
                        AlterMode::Set,
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    Config::from_shared_path(&path)
                        .unwrap()
                        .virtual_package_detectors
                        .consent(&origin),
                    Some(DetectorDecision::Allow)
                );

                alter_config(
                    &args,
                    "virtual-package-detectors.consent.\"https://conda.anaconda.org/conda-forge\"",
                    None,
                    AlterMode::Unset,
                )
                .await
                .unwrap();
                assert_eq!(
                    Config::from_shared_path(&path)
                        .unwrap()
                        .virtual_package_detectors
                        .consent(&origin),
                    None
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn shared_rejects_writes_to_existing_foreign_tables() {
        let original = "# keep this comment\n[shell]\nchange-ps1 = false\n";
        for (key, value) in [
            ("shell.force-activate", "true"),
            ("shell.change-ps1", "true"),
            (r#""shell"."force-activate""#, "true"),
            ("shell", r#"{"force-activate": true}"#),
            (r#""shell""#, r#"{"force-activate": true}"#),
        ] {
            let mut context = TestContext::setup(Some(original));
            context.common_args.shared = true;
            let result = alter_config(
                &context.common_args,
                key,
                Some(value.to_string()),
                AlterMode::Set,
            )
            .await;
            assert!(result.is_err(), "{key}");
            assert_eq!(context.read_config(), original, "{key}");
        }
    }

    #[tokio::test]
    async fn shared_writes_preserve_unrelated_foreign_keys() {
        let original = concat!(
            "\"tls-no-verify.extra\" = false\n",
            "\"repodata-config.disable-zstd\" = false\n",
            "# keep this comment\n[shell]\nchange-ps1 = false\nforeign-option = \"keep\"\n",
            "\n[repodata-config]\nforeign-option = \"also keep\"\n",
        );
        let mut context = TestContext::setup(Some(original));
        context.common_args.shared = true;
        alter_config(
            &context.common_args,
            "tls-no-verify",
            Some("true".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let contents = context.read_config();
        assert!(contents.contains("# keep this comment"));
        let document = contents.parse::<DocumentMut>().unwrap();
        assert_eq!(document["shell"]["change-ps1"].as_bool(), Some(false));
        assert_eq!(document["shell"]["foreign-option"].as_str(), Some("keep"));
        assert_eq!(
            Config::from_shared_path(&context.config_path)
                .unwrap()
                .tls_no_verify,
            Some(true)
        );
        alter_config(
            &context.common_args,
            "repodata-config.disable-zstd",
            Some("true".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let document = context.read_config().parse::<DocumentMut>().unwrap();
        assert_eq!(document["tls-no-verify.extra"].as_bool(), Some(false));
        assert_eq!(
            document["repodata-config.disable-zstd"].as_bool(),
            Some(false)
        );
        assert_eq!(
            document["repodata-config"]["foreign-option"].as_str(),
            Some("also keep")
        );
        assert_eq!(
            document["repodata-config"]["disable-zstd"].as_bool(),
            Some(true)
        );

        alter_config(
            &context.common_args,
            r#""shell"."change-ps1""#,
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();
        let contents = context.read_config();
        let document = contents.parse::<DocumentMut>().unwrap();
        assert!(document["shell"].get("change-ps1").is_none());
        assert_eq!(document["shell"]["foreign-option"].as_str(), Some("keep"));
        assert_eq!(document["tls-no-verify"].as_bool(), Some(true));

        alter_config(&context.common_args, "shell", None, AlterMode::Set)
            .await
            .unwrap();
        let document = context.read_config().parse::<DocumentMut>().unwrap();
        assert!(document.get("shell").is_none());
        assert_eq!(document["tls-no-verify"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn shared_leaf_writes_keep_required_siblings() {
        let mut context = TestContext::setup(Some(
            "[s3-options.bucket]\nendpoint-url = \"https://old.example.com\"\nregion = \"eu-west-1\"\n",
        ));
        context.common_args.shared = true;
        alter_config(
            &context.common_args,
            "s3-options.bucket.endpoint-url",
            Some("https://new.example.com".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();
        let config = Config::from_shared_path(&context.config_path).unwrap();
        let bucket = &config.s3_options.0["bucket"];
        assert_eq!(bucket.endpoint_url.as_str(), "https://new.example.com/");
        assert_eq!(bucket.region, "eu-west-1");
    }

    #[tokio::test]
    async fn set_creates_missing_pixi_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let project_root = temp_dir.path();

        fs_err::write(
            project_root.join("pixi.toml"),
            r#"[workspace]
            name = "test-workspace"
            channels = []"#,
        )
        .unwrap();

        let common_args = CommonArgs {
            local: false,
            global: false,
            system: false,
            shared: false,
            path: None,
            workspace_config: WorkspaceConfig {
                manifest_path: Some(temp_dir.path().to_path_buf()),
                ..Default::default()
            },
        };

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "pinning-strategy".to_owned(),
            value: Some("semver".to_owned()),
            common: common_args,
        }))
        .await;

        // Assert that the file AND the directory now exist
        let config_path = project_root.join(".pixi/config.toml");
        assert!(config_path.exists(), "Config file should have been created");
        assert!(
            config_path.parent().unwrap().exists(),
            "Parent directory should have been created"
        );
    }

    #[tokio::test]
    async fn list_empty_config() {
        let test_context = TestContext::setup(None);

        execute_subcommand(Subcommand::List(ListArgs {
            key: None,
            json: false,
            common: test_context.common_args,
            config_source: pixi_config::ConfigSourceCli::default(),
        }))
        .await;
    }

    #[tokio::test]
    async fn set_valid_key() {
        let test_context = TestContext::setup(None);

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "pinning-strategy".to_owned(),
            value: Some("semver".to_owned()),
            common: test_context.common_args,
        }))
        .await;
    }

    #[tokio::test]
    async fn set_preserves_comments() {
        let test_context = TestContext::setup(Some(
            "# some comment which should be kept\nallow-symbolic-links = true",
        ));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "tls-no-verify".to_owned(),
            value: Some("false".to_owned()),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @"
        # some comment which should be kept
        allow-symbolic-links = true
        tls-no-verify = false
        "
        );
    }

    #[tokio::test]
    async fn set_non_existent_key_to_none() {
        let test_context = TestContext::setup(Some("allow-symbolic-links = true"));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "tls-no-verify".to_owned(),
            value: None,
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @"allow-symbolic-links = true"
        );
    }

    #[tokio::test]
    async fn set_existing_key_to_none_removes_key() {
        let test_context = TestContext::setup(Some("allow-symbolic-links = true"));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "allow-symbolic-links".to_owned(),
            value: None,
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @""
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn set_table_creation() {
        let test_context = TestContext::setup(Some("allow-symbolic-links = true"));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "cache.root".to_owned(),
            value: Some("/tmp/pixi-cache".to_owned()),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        allow-symbolic-links = true

        [cache]
        root = "/tmp/pixi-cache"
        "#
        );
    }

    #[tokio::test]
    async fn unset_dotted_key() {
        let test_context = TestContext::setup(Some(
            r#"[repodata-config."https://prefix.dev"]
disable-sharded = false"#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "repodata-config.\"https://prefix.dev\".disable-sharded".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @""
        );
    }

    #[tokio::test]
    async fn unset_missing_key() {
        let test_context = TestContext::setup(None);

        let args = Args {
            subcommand: Subcommand::Unset(UnsetArgs {
                key: "pinning-strategy".to_owned(),
                common: test_context.common_args,
            }),
        };
        let result = execute(args).await;

        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found in configuration file"));
    }

    #[tokio::test]
    async fn unset_on_existing_stale_key() {
        let test_context = TestContext::setup(Some(
            r#"[shell]
stale-key = "some_value"
not-stale-key = "another_value"
            "#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "shell.stale-key".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        [shell]
        not-stale-key = "another_value"
        "#,
        );
    }

    #[tokio::test]
    async fn unset_on_stale_key_removes_empty_parent_table() {
        let test_context = TestContext::setup(Some(
            r#"[shell]
            stale-key = "some_value"
            "#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "shell.stale-key".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @""
        );
    }

    #[tokio::test]
    async fn unset_key_with_sibling_kept() {
        let test_context = TestContext::setup(Some(
            r#"[repodata-config."https://backup.example.com"]
disable-zstd = true

[repodata-config."https://primary.example.com"]
disable-sharded = false
"#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: r#"repodata-config."https://backup.example.com".disable-zstd"#.to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"

        [repodata-config."https://primary.example.com"]
        disable-sharded = false
        "#
        );
    }

    #[tokio::test]
    async fn unset_key_removes_nested_empty_parent_table() {
        let test_context = TestContext::setup(Some(
            r#"[pypi-options]
index-url = "https://pypi.org/simple"
"#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "pypi-options.index-url".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @""
        );
    }

    #[tokio::test]
    async fn unset_config_key_keeps_its_comment() {
        let test_context = TestContext::setup(Some(
            r#"# some comment that is being kept from the deleted key
allow-symbolic-links = true
stale-key = "some-value"
stale-key2 = "some-other-value"
        "#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "allow-symbolic-links".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        # some comment that is being kept from the deleted key
        stale-key = "some-value"
        stale-key2 = "some-other-value"
        "#
        );
    }

    #[tokio::test]
    async fn append_single_line() {
        let test_context = TestContext::setup(Some(
            r#"allow-symbolic-links = true
default-channels = ["conda-forge"]
"#,
        ));

        execute_subcommand(Subcommand::Append(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        allow-symbolic-links = true
        default-channels = ["conda-forge", "new-channel"]
        "#
        );
    }

    #[tokio::test]
    async fn append_multi_line() {
        let test_context = TestContext::setup(Some(
            r#"allow-symbolic-links = true
default-channels = [
    "conda-forge",
]
        "#,
        ));

        execute_subcommand(Subcommand::Append(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        allow-symbolic-links = true
        default-channels = [
            "conda-forge",
            "new-channel",
        ]
        "#
        );
    }

    #[tokio::test]
    async fn append_from_scratch() {
        let test_context = TestContext::setup(None);

        execute_subcommand(Subcommand::Append(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"default-channels = ["new-channel"]"#
        );
    }

    #[tokio::test]
    async fn prepend_single_line() {
        let test_context = TestContext::setup(Some(
            r#"allow-symbolic-links = true
default-channels = ["conda-forge"]
"#,
        ));

        execute_subcommand(Subcommand::Prepend(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        allow-symbolic-links = true
        default-channels = ["new-channel", "conda-forge"]
        "#
        );
    }

    #[tokio::test]
    async fn prepend_multi_line() {
        let test_context = TestContext::setup(Some(
            r#"allow-symbolic-links = true
default-channels = [
    "conda-forge",
]
        "#,
        ));

        execute_subcommand(Subcommand::Prepend(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"
        allow-symbolic-links = true
        default-channels = [
            "new-channel",
            "conda-forge",
        ]
        "#
        );
    }

    #[tokio::test]
    async fn prepend_from_scratch() {
        let test_context = TestContext::setup(None);

        execute_subcommand(Subcommand::Prepend(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"default-channels = ["new-channel"]"#
        );
    }

    #[tokio::test]
    async fn set_kebab_case_overwrites_legacy_snake_case_key() {
        let test_context =
            TestContext::setup(Some("other = 1\n# keep this comment\ntls_no_verify = true"));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "tls-no-verify".to_owned(),
            value: Some("false".to_owned()),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @"
        other = 1
        # keep this comment
        tls-no-verify = false
        "
        );

        // Verify subsequent modifications on the newly canonicalized key work cleanly
        execute_subcommand(Subcommand::Set(SetArgs {
            key: "tls-no-verify".to_owned(),
            value: Some("true".to_owned()),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @"
        other = 1
        # keep this comment
        tls-no-verify = true
        "
        );
    }

    #[tokio::test]
    async fn set_nested_key_reuses_snake_case_parent_table() {
        let test_context = TestContext::setup(Some(
            r#"
[repodata_config]
disable-sharded = true
"#,
        ));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "repodata-config.disable-sharded".to_owned(),
            common: test_context.common_args.clone(),
            value: Some("false".to_owned()),
        }))
        .await;

        insta::assert_snapshot!(
                    test_context.read_config(),
                    @"

        [repodata_config]
        disable-sharded = false
        ",
        );
    }

    #[tokio::test]
    async fn set_nested_key_overwrites_legacy_snake_case_child() {
        let test_context = TestContext::setup(Some(
            r#"
[repodata-config]
disable_bzip2 = true
"#,
        ));

        execute_subcommand(Subcommand::Set(SetArgs {
            key: "repodata-config.disable-bzip2".to_owned(),
            common: test_context.common_args.clone(),
            value: Some("false".to_owned()),
        }))
        .await;

        insta::assert_snapshot!(
                    test_context.read_config(),
                    @"

        [repodata-config]
        disable-bzip2 = false
        ",
        );
    }

    #[tokio::test]
    async fn append_preserves_snake_case_key() {
        let test_context = TestContext::setup(Some(
            r#"
default_channels = ["conda-forge"]
"#,
        ));

        execute_subcommand(Subcommand::Append(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"

        default_channels = ["conda-forge", "new-channel"]
        "#,
        );
    }

    #[tokio::test]
    async fn prepend_preserves_snake_case_key() {
        let test_context = TestContext::setup(Some(
            r#"
default_channels = ["conda-forge"]
"#,
        ));

        execute_subcommand(Subcommand::Prepend(PendArgs {
            key: "default-channels".to_owned(),
            value: "new-channel".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
            test_context.read_config(),
            @r#"

        default_channels = ["new-channel", "conda-forge"]
        "#,
        );
    }

    #[tokio::test]
    async fn unset_snake_case_target_key() {
        let test_context = TestContext::setup(Some(
            r#"
tls_no_verify = true
"#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "tls-no-verify".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
                    test_context.read_config(),
                    @"",
        );
    }

    #[tokio::test]
    async fn unset_nested_snake_case_child_key() {
        let test_context = TestContext::setup(Some(
            r#"
[repodata_config]
disable_sharded = true
disable-shared = true
"#,
        ));

        execute_subcommand(Subcommand::Unset(UnsetArgs {
            key: "repodata-config.disable-sharded".to_owned(),
            common: test_context.common_args.clone(),
        }))
        .await;

        insta::assert_snapshot!(
                    test_context.read_config(),
                    @ "

        [repodata_config]
        disable-shared = true
        ",
        );
    }

    #[tokio::test]
    async fn unset_invalidating_config_fails() {
        let test_context = TestContext::setup(Some(
            r#"
[s3-options.bucket]
endpoint-url = "https://my-s3-compatible-host.com"
addressing-style = "path"
region = "us-east-1"
"#,
        ));

        let args = Args {
            subcommand: Subcommand::Unset(UnsetArgs {
                key: "s3-options.bucket.region".to_owned(),
                common: test_context.common_args.clone(),
            }),
        };

        let result = execute(args).await;
        let err = result
            .expect_err("expected unset on required field to return an error, but it succeeded ");
        assert!(
            err.to_string()
                .contains("would leave the config file invalid")
                || err.to_string().contains("missing field `region`")
        );
    }

    async fn set_keys(keys: &[(&str, &str)]) -> String {
        let context = TestContext::setup(None);
        for (key, value) in keys {
            alter_config(
                &context.common_args,
                key,
                Some(value.to_string()),
                AlterMode::Set,
            )
            .await
            .unwrap();
        }
        context.read_config()
    }

    #[tokio::test]
    async fn set_azure_options_with_unquoted_and_quoted_endpoint() {
        let unquoted = set_keys(&[
            (
                "azure-options.mycompany.blob.core.windows.net.auth.releases",
                "true",
            ),
            (
                "azure-options.mycompany.blob.core.windows.net.scheme",
                "http",
            ),
        ])
        .await;
        let quoted = set_keys(&[
            (
                "azure-options.\"mycompany.blob.core.windows.net\".auth.releases",
                "true",
            ),
            (
                "azure-options.\"mycompany.blob.core.windows.net\".scheme",
                "http",
            ),
        ])
        .await;

        assert_eq!(unquoted, quoted);
        insta::assert_snapshot!(unquoted, @r#"
        [azure-options."mycompany.blob.core.windows.net"]
        scheme = "http"
        [azure-options."mycompany.blob.core.windows.net".auth]
        releases = true
        "#);
    }

    #[tokio::test]
    async fn set_whole_azure_entry_with_unquoted_and_quoted_endpoint() {
        let entry = r#"{"auth": {"releases": true}}"#;
        let unquoted = set_keys(&[("azure-options.mycompany.blob.core.windows.net", entry)]).await;
        let quoted =
            set_keys(&[("azure-options.\"mycompany.blob.core.windows.net\"", entry)]).await;

        assert_eq!(unquoted, quoted);
        insta::assert_snapshot!(unquoted, @r#"
        [azure-options]
        "mycompany.blob.core.windows.net" = { scheme = "https", auth = { releases = true } }
        "#);
    }

    #[tokio::test]
    async fn set_s3_options_with_unquoted_and_quoted_dotted_bucket() {
        let options = r#"{"endpoint-url": "http://localhost:9000", "region": "auto", "addressing-style": "path"}"#;
        let unquoted = set_keys(&[
            ("s3-options.my.bucket", options),
            ("s3-options.my.bucket.region", "eu-west-1"),
        ])
        .await;
        let quoted = set_keys(&[
            ("s3-options.\"my.bucket\"", options),
            ("s3-options.\"my.bucket\".region", "eu-west-1"),
        ])
        .await;

        assert_eq!(unquoted, quoted);
        insta::assert_snapshot!(unquoted, @r#"
        [s3-options]
        "my.bucket" = { endpoint-url = "http://localhost:9000/", region = "eu-west-1", addressing-style = "path" }
        "#);
    }

    #[tokio::test]
    async fn set_s3_field_on_undefined_bucket_fails() {
        let context = TestContext::setup(None);

        let err = alter_config(
            &context.common_args,
            "s3-options.missing.region",
            Some("eu-west-1".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap_err();

        insta::assert_snapshot!(err, @"s3 bucket `missing` is not defined in this file; set `s3-options.missing` to its full JSON first");
        insta::assert_snapshot!(context.read_config(), @"");
    }

    #[tokio::test]
    async fn set_default_value_on_empty_file_succeeds() {
        let written =
            set_keys(&[("default-channels", "[]"), ("concurrency.downloads", "50")]).await;

        insta::assert_snapshot!(written, @"");
    }

    #[tokio::test]
    async fn set_and_pend_quoted_default_channels() {
        let context = TestContext::setup(None);
        let key = "\"default-channels\"";
        for (value, mode) in [
            ("[\"conda-forge\"]", AlterMode::Set),
            ("bioconda", AlterMode::Append),
            ("pytorch", AlterMode::Prepend),
        ] {
            alter_config(&context.common_args, key, Some(value.to_string()), mode)
                .await
                .unwrap();
        }

        insta::assert_snapshot!(context.read_config(), @r#"default-channels = ["pytorch", "conda-forge", "bioconda"]"#);
    }

    async fn alter_file(original: &str, key: &str, value: Option<&str>, mode: AlterMode) -> String {
        let context = TestContext::setup(Some(original));
        alter_config(&context.common_args, key, value.map(str::to_string), mode)
            .await
            .unwrap();
        context.read_config()
    }

    #[tokio::test]
    async fn set_azure_grant_under_non_canonical_endpoint_spelling() {
        for spelling in [
            "mycompany.blob.core.windows.net.",
            "MyCompany.Blob.Core.Windows.Net",
        ] {
            let original = format!("[azure-options.\"{spelling}\".auth]\nreleases = true\n");
            let key = format!("azure-options.\"{spelling}\".auth.releases");
            let revoked = alter_file(&original, &key, Some("false"), AlterMode::Set).await;
            assert_eq!(
                revoked,
                format!("[azure-options.\"{spelling}\".auth]\nreleases = false\n")
            );

            let unset = alter_file(
                &original,
                "azure-options.mycompany.blob.core.windows.net.auth.releases",
                None,
                AlterMode::Unset,
            )
            .await;
            assert_eq!(unset, "");
        }
    }

    #[tokio::test]
    async fn set_azure_entry_replaces_non_canonical_endpoint_spelling() {
        let written = alter_file(
            "[azure-options.\"mycompany.blob.core.windows.net.\".auth]\nreleases = true\n",
            "azure-options.mycompany.blob.core.windows.net",
            Some(r#"{"auth": {"staging": true}}"#),
            AlterMode::Set,
        )
        .await;

        insta::assert_snapshot!(written, @r#"
        [azure-options]
        "mycompany.blob.core.windows.net." = { scheme = "https", auth = { staging = true } }
        "#);
    }

    #[tokio::test]
    async fn set_azure_containers_named_like_fields() {
        let scheme = set_keys(&[(
            "azure-options.mycompany.blob.core.windows.net.auth.scheme",
            "true",
        )])
        .await;
        insta::assert_snapshot!(scheme, @r#"
        [azure-options."mycompany.blob.core.windows.net".auth]
        scheme = true
        "#);

        let auth = set_keys(&[(
            "azure-options.mycompany.blob.core.windows.net.auth.auth",
            "true",
        )])
        .await;
        insta::assert_snapshot!(auth, @r#"
        [azure-options."mycompany.blob.core.windows.net".auth]
        auth = true
        "#);
    }

    #[tokio::test]
    async fn unset_azure_auth_table_with_unquoted_endpoint() {
        let original = concat!(
            "[azure-options.\"mycompany.blob.core.windows.net\"]\n",
            "scheme = \"http\"\n",
            "[azure-options.\"mycompany.blob.core.windows.net\".auth]\n",
            "releases = true\n",
        );
        let written = alter_file(
            original,
            "azure-options.mycompany.blob.core.windows.net.auth",
            None,
            AlterMode::Unset,
        )
        .await;

        insta::assert_snapshot!(written, @r#"
        [azure-options."mycompany.blob.core.windows.net"]
        scheme = "http"
        "#);
    }

    #[tokio::test]
    async fn unset_invalid_azure_endpoint() {
        let written = alter_file(
            "[azure-options.not-an-endpoint.auth]\nreleases = true\n",
            "azure-options.not-an-endpoint",
            None,
            AlterMode::Unset,
        )
        .await;

        insta::assert_snapshot!(written, @"");
    }

    #[tokio::test]
    async fn set_and_unset_s3_bucket_named_like_a_field() {
        let options = r#"{"endpoint-url": "http://localhost:9000", "region": "auto"}"#;
        let written = set_keys(&[("s3-options.\"region\"", options)]).await;
        insta::assert_snapshot!(written, @r#"
        [s3-options]
        region = { endpoint-url = "http://localhost:9000/", region = "auto", addressing-style = "virtual-host" }
        "#);

        let unset = alter_file(&written, "s3-options.region", None, AlterMode::Unset).await;
        insta::assert_snapshot!(unset, @"");
    }

    #[tokio::test]
    async fn set_omitted_value_removes_existing_entry() {
        let context = TestContext::setup(Some("default-channels = [\"conda-forge\"]\n"));

        alter_config(
            &context.common_args,
            "default-channels",
            Some("[]".to_string()),
            AlterMode::Set,
        )
        .await
        .unwrap();

        insta::assert_snapshot!(context.read_config(), @"");
    }

    #[tokio::test]
    async fn unset_azure_grant_with_unquoted_endpoint() {
        let context = TestContext::setup(Some(
            "[azure-options.\"mycompany.blob.core.windows.net\".auth]\nreleases = true\nstaging = true\n",
        ));

        alter_config(
            &context.common_args,
            "azure-options.mycompany.blob.core.windows.net.auth.releases",
            None,
            AlterMode::Unset,
        )
        .await
        .unwrap();

        insta::assert_snapshot!(context.read_config(), @r#"
        [azure-options."mycompany.blob.core.windows.net".auth]
        staging = true
        "#);
    }

    #[tokio::test]
    async fn list_accepts_every_supported_key() {
        let test_context = TestContext::setup(None);

        let mut rejected = Vec::new();
        for key in Config::default().get_keys() {
            let key = key
                .replace("<bucket>", "my-bucket")
                .replace("<endpoint>", r#""mycompany.blob.core.windows.net""#)
                .replace("<container>", "releases")
                .replace("<origin>", r#""https://prefix.dev/channel""#)
                .replace("<package>", "numpy")
                .replace("<field>", "version");
            let result = execute(Args {
                subcommand: Subcommand::List(ListArgs {
                    key: Some(key.clone()),
                    json: false,
                    common: test_context.common_args.clone(),
                    config_source: pixi_config::ConfigSourceCli {
                        no_config: true,
                        ..Default::default()
                    },
                }),
            })
            .await;
            if result.is_err() {
                rejected.push(key);
            }
        }

        assert!(
            rejected.is_empty(),
            "`pixi config list <key>` rejects supported keys: {rejected:?}"
        );
    }

    #[test]
    fn list_selected_values_serialize_as_toml_and_json() {
        let (config, _) = Config::from_toml(
            "default-channels = [\"conda-forge\"]\n[shell]\nchange-ps1 = false\nforce-activate = true\n",
            None,
        )
        .unwrap();
        for (key, expected) in [
            (
                "default-channels",
                serde_json::json!({"default-channels": ["conda-forge"]}),
            ),
            (
                "shell.change-ps1",
                serde_json::json!({"shell": {"change-ps1": false}}),
            ),
            (
                "shell",
                serde_json::json!({"shell": {"change-ps1": false, "force-activate": true}}),
            ),
        ] {
            let partial = partial_config(&config, key).unwrap();
            let toml = toml_edit::ser::to_string_pretty(&partial).unwrap();
            let json = serde_json::to_string_pretty(&partial).unwrap();
            assert_eq!(
                toml_edit::de::from_str::<JsonValue>(&toml).unwrap(),
                expected,
                "{key}"
            );
            assert_eq!(
                serde_json::from_str::<JsonValue>(&json).unwrap(),
                expected,
                "{key}"
            );
        }
    }

    #[test]
    fn test_partial_config_nested_keys() {
        let (config, _) = Config::from_toml(
            r#"
default-channels = ["conda-forge"]

[pypi-config]
index-url = "https://pypi.example.com/simple"
extra-index-urls = ["https://extra.example.com/simple"]

[s3-options."my.bucket"]
endpoint-url = "https://s3.example.com"
region = "eu-west-1"
addressing-style = "path"

[virtual-package-detectors]
timeout-seconds = 7
"#,
            None,
        )
        .unwrap();
        assert_eq!(
            partial_config(&config, "pypi-config.index-url").unwrap(),
            serde_json::json!({
                "pypi-config": {"index-url": "https://pypi.example.com/simple"}
            })
        );
        assert_eq!(
            partial_config(&config, r#"s3-options."my.bucket".region"#).unwrap(),
            serde_json::json!({"s3-options": {"my.bucket": {"region": "eu-west-1"}}})
        );
        assert_eq!(
            partial_config(&config, "virtual-package-detectors.timeout-seconds").unwrap(),
            serde_json::json!({"virtual-package-detectors": {"timeout-seconds": 7}})
        );
    }

    #[test]
    fn test_partial_config_unset_and_unknown_keys() {
        let (config, _) = Config::from_toml("default-channels = [\"conda-forge\"]", None).unwrap();
        assert_eq!(
            partial_config(&config, "s3-options.missing.region").unwrap(),
            serde_json::json!({})
        );
        let unset = partial_config(&config, "detached-environments").unwrap();
        assert_eq!(
            toml_edit::de::from_str::<JsonValue>(
                &toml_edit::ser::to_string_pretty(&unset).unwrap()
            )
            .unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::from_str::<JsonValue>(&serde_json::to_string_pretty(&unset).unwrap())
                .unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            partial_config(&config, "s3-options.missing.not-a-field").unwrap(),
            serde_json::json!({})
        );
        for key in ["not-a-key", "offline.nested"] {
            assert!(partial_config(&config, key).is_err(), "{key}");
        }
    }

    #[test]
    fn test_partial_config_with_unquoted_dotted_keys() {
        let (config, _) = Config::from_toml(
            r#"
[azure-options."mycompany.blob.core.windows.net".auth]
releases = true

[s3-options."my.bucket"]
endpoint-url = "https://s3.example.com"
region = "eu-west-1"
"#,
            None,
        )
        .unwrap();
        assert_eq!(
            partial_config(&config, "azure-options.mycompany.blob.core.windows.net").unwrap(),
            serde_json::json!({"azure-options": {"mycompany.blob.core.windows.net": {
                "scheme": "https",
                "auth": {"releases": true}
            }}})
        );
        assert_eq!(
            partial_config(
                &config,
                "azure-options.MyCompany.blob.core.windows.net.auth.releases"
            )
            .unwrap(),
            serde_json::json!({"azure-options": {"mycompany.blob.core.windows.net": {
                "auth": {"releases": true}
            }}})
        );
        assert_eq!(
            partial_config(&config, "s3-options.my.bucket.region").unwrap(),
            serde_json::json!({"s3-options": {"my.bucket": {"region": "eu-west-1"}}})
        );
        assert_eq!(
            partial_config(&config, "s3-options.my.bucket").unwrap(),
            serde_json::json!({"s3-options": {"my.bucket": {
                "endpoint-url": "https://s3.example.com/",
                "region": "eu-west-1",
                "addressing-style": "virtual-host"
            }}})
        );
    }
}
