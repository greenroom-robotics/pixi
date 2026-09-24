use std::cmp::Ordering;
use std::io::{Seek, Write};

use flate2::read::GzDecoder;

use miette::IntoDiagnostic;
use pixi_config::Config;
use pixi_consts::consts;
use pixi_utils::reqwest::{build_reqwest_clients, reqwest_client_builder};
use reqwest::redirect::Policy;

use tempfile::NamedTempFile;
use url::Url;

use rattler_conda_types::Version;
use std::str::FromStr;

use crate::GlobalOptions;
use pixi_reporters::format_release_notes;

/// Update pixi to the latest version or a specific version.
#[derive(Debug, clap::Parser)]
pub struct Args {
    #[clap(flatten)]
    config_source: pixi_config::ConfigSourceCli,

    /// Run without network access. Updating always requires the network, so
    /// this makes `pixi self-update` fail fast instead of attempting to
    /// connect.
    #[arg(
        long,
        env = "PIXI_OFFLINE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    offline: Option<bool>,

    /// The desired version (to downgrade or upgrade to).
    #[clap(long)]
    version: Option<Version>,

    /// Only show release notes, do not modify the binary.
    #[clap(long)]
    dry_run: bool,

    /// Force download the desired version when not exactly same with the current. If no desired
    /// version, always replace with the latest version.
    #[clap(long, default_value_t = false)]
    force: bool,

    /// Skip printing the release notes.
    #[clap(long, default_value_t = false)]
    no_release_note: bool,

    /// The github releases URL, useful when behind a proxy, or using custom Pixi release
    #[clap(long)]
    from_url: Option<String>,
}

/// Response from the Github API when fetching a release by tag.
/// <https://docs.github.com/de/rest/releases/releases?apiVersion=2022-11-28#get-a-release-by-tag-name>
#[derive(Debug, serde::Deserialize)]
struct ReleaseResponse {
    /// Markdown body of the release as seen on the Github release page.
    body: String,

    /// The time and date when the release was published. (seems to be ISO 8601)
    published_at: String,

    /// The tag name of the release
    tag_name: String,
}

fn user_agent() -> String {
    format!("pixi {}", consts::PIXI_VERSION)
}

fn release_asset_name() -> miette::Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("pixi-linux-64.gz"),
        ("linux", "aarch64") => Ok("pixi-linux-aarch64.gz"),
        (os, arch) => miette::bail!(
            "`pixi self-update` is unsupported on {os}-{arch}: pixi-gr only publishes linux x86_64 and aarch64 builds."
        ),
    }
}

async fn latest_version() -> miette::Result<Version> {
    // Uses the public Github Releases /latest endpoint to get the latest tag from the URL
    let url = format!("{}/latest", consts::RELEASES_URL);

    // Load global config to respect TLS settings (e.g., tls-root-certs = "native")
    let config = Config::load_global();

    // Create a client with a redirect policy
    let no_redirect_client = reqwest_client_builder(Some(&config))?
        .redirect(Policy::none())
        .build()
        .into_diagnostic()?; // Prevent automatic redirects

    let version: String = match no_redirect_client
        .head(&url)
        .header("User-Agent", user_agent())
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_redirection() {
                match response.headers().get("Location") {
                    Some(location) => Url::parse(location.to_str().into_diagnostic()?)
                        .into_diagnostic()?
                        .path_segments()
                        .ok_or_else(|| {
                            miette::miette!("Could not get segments from Location header")
                        })?
                        .next_back()
                        .ok_or_else(|| {
                            miette::miette!("Could not get version from Location header")
                        })?
                        .to_string(),
                    None => miette::bail!(
                        "URL: {}. Redirect detected, but no 'Location' header found.",
                        url
                    ),
                }
            } else {
                miette::bail!(
                    "URL: {}. Request failed or did not redirect: {}.",
                    url,
                    response.status()
                )
            }
        }
        Err(err) => miette::bail!("URL: {}. Request failed: {}", url, err),
    };
    if version == "releases" {
        // /latest redirect took us back to /releases instead of /<tag>
        miette::bail!("URL '{}' does not seem to contain any releases.", url)
    }
    match version.strip_prefix(consts::RELEASE_TAG_PREFIX) {
        Some(version) => Ok(Version::from_str(version).into_diagnostic()?),
        None => miette::bail!(
            "Tag name '{}' must start with {}.",
            version,
            consts::RELEASE_TAG_PREFIX
        ),
    }
}

async fn fetch_release_notes(version: &Option<Version>) -> miette::Result<String> {
    let url = if let Some(version) = version {
        format!(
            "{}/{}{}",
            consts::RELEASES_API_BY_TAG,
            consts::RELEASE_TAG_PREFIX,
            version
        )
    } else {
        consts::RELEASES_API_LATEST.to_string()
    };

    let client = build_reqwest_clients(None, None)?.1;
    let response = client
        .get(&url)
        .header("User-Agent", user_agent())
        .send()
        .await
        .into_diagnostic()?;

    if response.status().is_success() {
        let release_response: ReleaseResponse = response.json().await.into_diagnostic()?;

        // We only care for the date, not the time
        let date = release_response
            .published_at
            .split('T')
            .next()
            .unwrap_or("unknown");

        Ok(format!(
            "Release notes for version {} ({}):\n{}\n",
            version
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or(release_response.tag_name),
            date,
            release_response.body.trim()
        ))
    } else {
        miette::bail!("Status code {}", response.status())
    }
}

/// Executes the self-update command.
///
/// # Arguments
/// * `args` - The self-update specific arguments.
/// * `global_options` - Reference to the global CLI options.
pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    // Updating the binary always requires downloading a release from GitHub,
    // so bail out early with a clear error in offline mode.
    if args
        .offline
        .unwrap_or_else(|| Config::load_global_with(&args.config_source.source()).offline())
    {
        return Err(crate::offline::NetworkRequiredError {
            command: "pixi self-update",
        }
        .into());
    }

    let asset_name = release_asset_name()?;

    // Exam the validity of provided url
    if let Some(ref url) = args.from_url
        && let Err(err) = Url::parse(url)
    {
        miette::bail!("URL: {}. Url validation failed: {}", url, err)
    }

    let is_quiet = global_options.quiet > 0;
    // Get the target version, without 'v' prefix, None for force latest version
    let target_version = match &args.version {
        Some(version) => {
            // Remove leading 'v' if present and inform the user
            if version.to_string().starts_with('v') {
                if !is_quiet {
                    eprintln!(
                        "{}Warning: Leading 'v' removed from version {}",
                        console::style(console::Emoji("⚠️ ", "")).yellow(),
                        version
                    );
                }
                Some(Version::from_str(&version.to_string()[1..]).into_diagnostic()?)
            } else {
                Some(version.clone())
            }
        }
        None => {
            if args.force {
                None
            } else {
                Some(latest_version().await?)
            }
        }
    };

    // Get the current version of the pixi binary
    let current_version = Version::from_str(consts::PIXI_VERSION).into_diagnostic()?;

    let up_to_date = target_version
        .as_ref()
        .is_some_and(|t| *t == current_version);

    let fetch_release_warning = if args.no_release_note || up_to_date || is_quiet {
        None
    } else {
        match fetch_release_notes(&target_version).await {
            Ok(release_notes) => {
                // Print release notes
                eprintln!(
                    "{}{}",
                    console::style(console::Emoji("📝 ", "")).yellow(),
                    format_release_notes(&release_notes)
                );
                None
            }
            Err(err) => {
                // Failure to fetch release notes must not prevent self-update, especially if format changes
                let release_url = if let Some(ref target_version) = target_version {
                    format!(
                        "{}/tag/{}{}",
                        consts::RELEASES_URL,
                        consts::RELEASE_TAG_PREFIX,
                        target_version
                    )
                } else {
                    format!("{}/latest", consts::RELEASES_URL)
                };
                Some(format!(
                    "{}Failed to fetch release notes ({}). Check the release page for more information: {}",
                    console::style(console::Emoji("⚠️ ", "")).yellow(),
                    err,
                    release_url
                ))
            }
        }
    };

    // Don't actually update the binary if `--dry-run` is passed
    if args.dry_run {
        if !is_quiet {
            let target_version = match target_version {
                Some(target_version) => target_version,
                None => latest_version().await?,
            };
            eprintln!("{}", get_dry_run_message(&current_version, &target_version));
        }
        return Ok(());
    }

    // Stop here if the target version is the same as the current version
    if up_to_date {
        if !is_quiet {
            eprintln!(
                "{}pixi is already up-to-date (version {})",
                console::style(console::Emoji("✔ ", "")).green(),
                current_version
            );
        }
        return Ok(());
    }

    let action = if !args.force
        && target_version
            .as_ref()
            .is_some_and(|t| *t < current_version)
    {
        if args.version.is_none() {
            // Ask if --version was not passed
            let confirmation = dialoguer::Confirm::new()
                .with_prompt(format!(
                        "\nCurrent version ({}) is more recent than remote ({}). Do you want to downgrade?",
                        current_version, target_version.as_ref().expect("target_version is not resolved")
                ))
                .default(false)
                .show_default(true)
                .interact()
                .into_diagnostic()?;
            if !confirmation {
                return Ok(());
            };
        };
        "downgraded"
    } else {
        "updated"
    };

    if !args.force && !is_quiet {
        eprintln!(
            "{}Pixi will be {} from {} to {}",
            console::style(console::Emoji("✔ ", "")).green(),
            action,
            current_version,
            target_version
                .as_ref()
                .expect("target_version is not resolved")
        );
    }

    let pre_fix = if let Some(ref from_url) = args.from_url {
        from_url
    } else {
        consts::RELEASES_URL
    };

    let download_url = if let Some(ref target_version) = target_version {
        format!(
            "{}/download/{}{}/{}",
            pre_fix,
            consts::RELEASE_TAG_PREFIX,
            target_version,
            asset_name
        )
    } else {
        format!("{}/latest/download/{}", pre_fix, asset_name)
    };

    // Create a temp file to download the archive
    let mut archived_tempfile = NamedTempFile::new().into_diagnostic()?;

    let client = build_reqwest_clients(None, None)?.1;
    let mut res = client
        .get(&download_url)
        .header("User-Agent", user_agent())
        .send()
        .await
        .expect("Failed to download the archive");

    if res.status() != reqwest::StatusCode::OK {
        miette::bail!(format!("URL {} returned {}", download_url, res.status()));
    } else {
        // Download the archive
        while let Some(chunk) = res.chunk().await.into_diagnostic()? {
            archived_tempfile
                .as_file()
                .write_all(&chunk)
                .into_diagnostic()?;
        }
    }

    if !is_quiet {
        eprintln!(
            "{}Pixi archive downloaded.",
            console::style(console::Emoji("✔ ", "")).green(),
        );
    }

    // Seek to the beginning of the file before uncompressing it
    archived_tempfile
        .rewind()
        .expect("Failed to rewind the archive file");

    let new_binary = NamedTempFile::new().into_diagnostic()?;
    gunzip(archived_tempfile.as_file(), new_binary.as_file())?;

    if !is_quiet {
        eprintln!(
            "{}Pixi archive uncompressed.",
            console::style(console::Emoji("✔ ", "")).green(),
        );
    }

    self_replace::self_replace(new_binary.path()).into_diagnostic()?;

    if !is_quiet {
        if let Some(ref target_version) = target_version {
            eprintln!(
                "{}Pixi has been updated to version {}.",
                console::style(console::Emoji("✔ ", "")).green(),
                target_version
            );
        } else {
            eprintln!(
                "{}Pixi has been updated to latest release.",
                console::style(console::Emoji("✔ ", "")).green(),
            );
        }
    }

    if let Some(fetch_release_warning) = fetch_release_warning {
        tracing::warn!(fetch_release_warning);
    }

    Ok(())
}

/// Return the message that should be shown to users when executing with `--dry-run`.
fn get_dry_run_message(current: &Version, target: &Version) -> String {
    match target.cmp(current) {
        Ordering::Equal => format!(
            "{}Current pixi version already at latest version: {current}.",
            console::style(console::Emoji("✔ ", "")).green()
        ),
        Ordering::Greater => format!(
            "{}Pixi version would be updated from {current} to {target}, but `--dry-run` given.",
            console::style(console::Emoji("ℹ️ ", "")).yellow()
        ),
        Ordering::Less => format!(
            "{}Pixi version would be downgraded from {current} to {target}, but `--dry-run` given.",
            console::style(console::Emoji("ℹ️ ", "")).yellow()
        ),
    }
}

fn gunzip(src: &std::fs::File, mut dst: &std::fs::File) -> miette::Result<()> {
    std::io::copy(&mut GzDecoder::new(src), &mut dst).into_diagnostic()?;
    Ok(())
}

pub async fn execute_stub(_: Args, _: &GlobalOptions) -> miette::Result<()> {
    let message = option_env!("PIXI_SELF_UPDATE_DISABLED_MESSAGE");
    miette::bail!(
        message.unwrap_or("This version of pixi was built without self-update support. Please use your package manager to update pixi.")
    )
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, Write};

    use flate2::{Compression, write::GzEncoder};

    #[test]
    pub fn test_gunzip_round_trip() {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(b"pixi binary").unwrap();
        let mut src = tempfile::tempfile().unwrap();
        src.write_all(&gz.finish().unwrap()).unwrap();
        src.rewind().unwrap();

        let mut dst = tempfile::tempfile().unwrap();
        super::gunzip(&src, &dst).unwrap();

        let mut out = Vec::new();
        dst.rewind().unwrap();
        dst.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"pixi binary");
    }
}
