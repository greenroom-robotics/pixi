use crate::cli_config::WorkspaceConfig;
use clap::Parser;
use fancy_display::FancyDisplay;
use miette::{Context, IntoDiagnostic};
use pixi_api::workspace::platforms::resolve_platforms;
use pixi_consts::consts;
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
    let outcome = probe_conda_solve(
        &workspace,
        &environment,
        platform.name(),
        extra,
        Some(&progress),
    )
    .await;
    progress.on_clear();
    let outcome = outcome?;

    match outcome {
        ProbeOutcome::Unsolvable(reasons) => {
            println!(
                "{} Cannot solve {} for {} with {}",
                console::style("×").red().bold(),
                environment.name().fancy_display(),
                consts::PLATFORM_STYLE.apply_to(platform.name()),
                console::style(&spec).bold(),
            );
            for reason in reasons {
                println!();
                println!("{}", collapse_repeats(&reason));
            }
        }
        ProbeOutcome::Solvable => {
            println!(
                "{} Nothing prevents {} in {} ({})",
                console::style("✔").green().bold(),
                console::style(&spec).bold(),
                environment.name().fancy_display(),
                consts::PLATFORM_STYLE.apply_to(platform.name()),
            );
            println!(
                "  The lock file is out of date. Run: {}",
                consts::TASK_STYLE.apply_to(format!("pixi update {}", name.as_source())),
            );
        }
    }

    Ok(())
}

const BRANCH: &str = "├─ ";
const LAST_BRANCH: &str = "└─ ";

/// Splits a line of a solver explanation into its tree indent, its branch
/// connector and the text that follows. `None` when the line is not a branch.
fn split_branch(line: &str) -> Option<(&str, &str, &str)> {
    [BRANCH, LAST_BRANCH].into_iter().find_map(|connector| {
        let at = line.find(connector)?;
        let (indent, rest) = line.split_at(at);
        indent.chars().all(|c| c == ' ' || c == '│').then_some((
            indent,
            connector,
            &rest[connector.len()..],
        ))
    })
}

/// The fixed phrases the solver appends after the package a line is about.
/// Everything before one is a package name and a version or version set.
const PHRASES: [&str; 7] = [
    ", for which no candidates were found.",
    ", which cannot be installed because there are no viable options:",
    ", which conflicts with the versions reported above.",
    ", which conflicts with any installable versions previously reported",
    " cannot be installed because there are no viable options:",
    " is excluded because ",
    " would require",
];

/// Splits the text of a line at the earliest phrase that follows the package
/// it names. The phrase is empty when the line names no package.
fn split_phrase(text: &str) -> (&str, &str) {
    PHRASES
        .iter()
        .filter_map(|phrase| text.find(phrase))
        .min()
        .map_or((text, ""), |at| text.split_at(at))
}

/// Renders one line: the tree and the solver's boilerplate recede, the package
/// it names stands out. A line naming no package is left alone.
fn render_line(indent: &str, connector: &str, text: &str, count: usize) -> String {
    let (subject, phrase) = split_phrase(text);
    if phrase.is_empty() {
        return format!("{indent}{connector}{text}");
    }
    let subject = match subject.split_once(' ') {
        Some((name, version)) => format!(
            "{} {}",
            consts::CONDA_PACKAGE_STYLE.apply_to(name),
            console::style(version).yellow()
        ),
        None => consts::CONDA_PACKAGE_STYLE.apply_to(subject).to_string(),
    };
    let badge = if count > 1 {
        format!(" ({count} builds)")
    } else {
        String::new()
    };
    let dim = |text: String| match text.is_empty() {
        true => text,
        false => console::style(text).dim().to_string(),
    };
    format!(
        "{}{subject}{}{}",
        dim(format!("{indent}{connector}")),
        dim(badge),
        dim(phrase.to_owned()),
    )
}

/// Joins adjacent branches that repeat the same text at the same depth into
/// one line carrying the repeat count. The solver reports a branch per build,
/// so a version with many builds otherwise repeats verbatim dozens of times.
fn collapse_repeats(explanation: &str) -> String {
    fn flush(out: &mut Vec<String>, run: Option<(&str, &str, &str, usize)>) {
        if let Some((indent, connector, text, count)) = run {
            out.push(render_line(indent, connector, text, count));
        }
    }

    let mut out = Vec::new();
    let mut run = None;
    for line in explanation.lines() {
        match split_branch(line) {
            Some((indent, connector, text)) => match run {
                Some((prev_indent, _, prev_text, count))
                    if (prev_indent, prev_text) == (indent, text) =>
                {
                    run = Some((indent, connector, text, count + 1));
                }
                previous => {
                    flush(&mut out, previous);
                    run = Some((indent, connector, text, 1));
                }
            },
            None => {
                flush(&mut out, run.take());
                out.push(render_line("", "", line, 1));
            }
        }
    }
    flush(&mut out, run);
    out.join("\n")
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

#[cfg(test)]
mod tests {
    use super::collapse_repeats;

    #[test]
    fn repeats_collapse_keeping_the_last_connector() {
        let explanation = "\
The following packages are incompatible
├─ python >=3.14,<3.15 cannot be installed because there are no viable options:
│  ├─ python 3.14.7, which conflicts with the versions reported above.
│  ├─ python 3.14.7, which conflicts with the versions reported above.
│  └─ python 3.14.0, which conflicts with the versions reported above.
└─ python ==3.12 cannot be installed because there are no viable options:
   └─ python 3.12.0, which conflicts with the versions reported above.";

        assert_eq!(
            collapse_repeats(explanation),
            "\
The following packages are incompatible
├─ python >=3.14,<3.15 cannot be installed because there are no viable options:
│  ├─ python 3.14.7 (2 builds), which conflicts with the versions reported above.
│  └─ python 3.14.0, which conflicts with the versions reported above.
└─ python ==3.12 cannot be installed because there are no viable options:
   └─ python 3.12.0, which conflicts with the versions reported above."
        );
    }

    #[test]
    fn a_line_naming_no_package_is_left_alone() {
        let explanation = "The following packages are incompatible";

        assert_eq!(collapse_repeats(explanation), explanation);
    }

    #[test]
    fn same_text_at_different_depths_does_not_merge() {
        let explanation = "\
├─ python 3.14.0, which conflicts.
│  ├─ python 3.14.0, which conflicts.";

        assert_eq!(collapse_repeats(explanation), explanation);
    }
}
