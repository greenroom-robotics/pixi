from pathlib import Path

from .common import CURRENT_PLATFORM, verify_cli_command


def test_why_not_reports_the_conflict(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    manifest_path = tmp_pixi_workspace / "pixi.toml"
    manifest_path.write_text(
        f"""
[workspace]
name = "test-why-not"
channels = ["{multiple_versions_channel_1}"]
platforms = ["{CURRENT_PLATFORM}"]

[dependencies]
package = "==0.1.0"
"""
    )
    verify_cli_command([pixi, "lock", "--manifest-path", manifest_path])
    verify_cli_command(
        [pixi, "why-not", "package>=0.2.0", "--manifest-path", manifest_path],
        stdout_contains=f"Cannot solve default for {CURRENT_PLATFORM} with package >=0.2.0:",
    )
