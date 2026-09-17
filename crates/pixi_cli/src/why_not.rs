use crate::cli_config::WorkspaceConfig;
use clap::Parser;
use fancy_display::FancyDisplay;
use miette::{Context, IntoDiagnostic};
use pixi_api::workspace::platforms::resolve_platforms;
use pixi_core::{
    WorkspaceLocator,
    lock_file::{ProbeOutcome, probe_conda_solve, resolve_lock_platform_for},
};
use pixi_manifest::{HasWorkspaceManifest as _, PixiPlatform, PixiPlatformName};
use pixi_spec::PixiSpec;
use rattler_conda_types::{
    MatchSpec, PackageName, PackageNameMatcher, ParseStrictness, Version, VersionSpec,
    version_spec::RangeOperator,
};
use rattler_lock::LockFile;

/// Explain why the solver cannot give you a version of a package
#[derive(Debug, Parser)]
pub struct Args {
    #[clap(flatten)]
    pub config_source: pixi_config::ConfigSourceCli,

    /// The MatchSpec to probe for, e.g. `python>=3.13`. Without a version
    /// constraint the currently locked version is used as a lower bound.
    #[arg()]
    pub spec: String,

    /// The platform to solve for. Defaults to the platform best matching this
    /// machine. Accepts a workspace platform name; a bare conda subdir (e.g.
    /// `linux-64`) is also accepted.
    #[arg(long, short)]
    pub platform: Option<PixiPlatformName>,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    /// The environment to solve. Defaults to the default environment.
    #[arg(short, long)]
    pub environment: Option<String>,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let workspace = WorkspaceLocator::for_cli()
        .with_global_config_source(args.config_source.source())
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?;

    let environment = workspace
        .environment_from_name_or_env_var(args.environment)
        .wrap_err("Environment not found")?;

    let workspace_platforms = (&workspace)
        .workspace_manifest()
        .workspace
        .platforms
        .clone();
    let platform = match args.platform {
        Some(name) => resolve_platforms(&workspace_platforms, std::slice::from_ref(&name))?
            .into_iter()
            .next()
            .expect("resolve_platforms preserves length"),
        None => environment
            .best_declared_platform()
            .cloned()
            .ok_or_else(|| {
                miette::miette!(
                    "no platform supported by environment '{}' matches the current system",
                    environment.name()
                )
            })?,
    };

    let mut spec = MatchSpec::from_str(&args.spec, ParseStrictness::Lenient).into_diagnostic()?;
    let PackageNameMatcher::Exact(name) = spec.name.clone() else {
        return Err(miette::miette!(
            "`pixi why-not` needs a match spec that names one package"
        ));
    };

    if spec.version.is_none() {
        let lock_file = workspace
            .load_lock_file()
            .await?
            .into_lock_file_or_empty_with_warning();
        let version = locked_version(&lock_file, environment.name().as_str(), &platform, &name)
            .ok_or_else(|| {
                miette::miette!(
                    help = format!("try `pixi why-not '{}>=<version>'`", name.as_source()),
                    "'{}' is not locked for environment '{}' on '{}', so there is no version to compare against",
                    name.as_source(),
                    environment.name(),
                    platform.name()
                )
            })?;
        spec.version = Some(VersionSpec::Range(RangeOperator::Greater, version));
    }

    let (_, nameless) = spec.clone().into_nameless();
    let extra = (
        name.clone(),
        PixiSpec::from_nameless_matchspec(nameless, &workspace.channel_config()),
    );

    let progress = pixi_reporters::TopLevelProgress::from_global();
    let outcome = {
        let _clear = pixi_reporters::TopLevelProgress::clear_when_done(Some(&progress));
        probe_conda_solve(
            &workspace,
            &environment,
            platform.name(),
            extra,
            Some(&progress),
        )
        .await?
    };

    match outcome {
        ProbeOutcome::Unsolvable(explanation) => {
            println!(
                "Cannot solve {} for {} with {}:",
                environment.name().fancy_display(),
                platform.name(),
                spec
            );
            println!("{explanation}");
        }
        ProbeOutcome::Solvable => println!(
            "Nothing prevents {} in {} ({}); the lock is just stale. Run: pixi update {}",
            spec,
            environment.name().fancy_display(),
            platform.name(),
            name.as_source()
        ),
    }

    Ok(())
}

fn locked_version(
    lock_file: &LockFile,
    environment: &str,
    platform: &PixiPlatform,
    name: &PackageName,
) -> Option<Version> {
    let env = lock_file.environment(environment)?;
    let locked_platform = resolve_lock_platform_for(lock_file, platform)?;
    env.packages(locked_platform)?
        .find_map(|p| p.as_conda().filter(|c| c.name() == name))
        .and_then(|c| c.record())
        .map(|record| record.version.version().clone())
}
