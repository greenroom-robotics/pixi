from pathlib import Path

import pytest

from .common import ExitCode, copytree_with_local_backend, verify_cli_command


def test_log_working_quiet(pixi: Path, build_data: Path, tmp_pixi_workspace: Path) -> None:
    test_data = build_data.joinpath("log-example", "working")

    copytree_with_local_backend(test_data, tmp_pixi_workspace, dirs_exist_ok=True)

    verify_cli_command(
        [
            pixi,
            "install",
            "--quiet",
            "--manifest-path",
            tmp_pixi_workspace,
        ],
        stderr_excludes="Building package simple-app",
    )


def test_log_working_default(pixi: Path, build_data: Path, tmp_pixi_workspace: Path) -> None:
    test_data = build_data.joinpath("log-example", "working")

    copytree_with_local_backend(test_data, tmp_pixi_workspace, dirs_exist_ok=True)

    verify_cli_command(
        [
            pixi,
            "install",
            "--manifest-path",
            tmp_pixi_workspace,
        ],
        stderr_excludes="Building package simple-app",
    )


def test_log_working_verbose(pixi: Path, build_data: Path, tmp_pixi_workspace: Path) -> None:
    test_data = build_data.joinpath("log-example", "working")

    copytree_with_local_backend(test_data, tmp_pixi_workspace, dirs_exist_ok=True)

    # An isolated build cache forces the source build to actually run, so
    # there is backend output for `-v` to stream.
    env = {"PIXI_CACHE_DIR": str(tmp_pixi_workspace / "pixi-cache")}
    verify_cli_command(
        [
            pixi,
            "install",
            "-v",
            "--manifest-path",
            tmp_pixi_workspace,
        ],
        env=env,
        stderr_contains=["[simple-app] ", "Building package simple-app"],
    )


@pytest.mark.slow
def test_log_failing(pixi: Path, build_data: Path, tmp_pixi_workspace: Path) -> None:
    test_data = build_data.joinpath("log-example", "failing")

    copytree_with_local_backend(test_data, tmp_pixi_workspace, dirs_exist_ok=True)

    verify_cli_command(
        [
            pixi,
            "install",
            "--quiet",
            "--manifest-path",
            tmp_pixi_workspace,
        ],
        ExitCode.FAILURE,
        stderr_contains="failed to build 'simple-app'",
    )
