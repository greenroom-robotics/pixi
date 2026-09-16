"""Cover keep-going source builds and the failure summary they produce.

A source build failure no longer aborts sibling builds: packages that do not
depend on the failed one still build, packages that do depend on it are
reported as skipped, and the final summary/log-tail wording matches
`SourceBuildFailures` (crates/pixi_command_dispatcher/src/errors.rs) and the
build-failure reporting in crates/pixi_reporters/src/sync_reporter.rs.
"""

from pathlib import Path

import pytest

from .common import ExitCode, copytree_with_local_backend, verify_cli_command


@pytest.mark.slow
def test_keep_going_reports_failure_and_skips_dependents(
    pixi: Path, build_data: Path, tmp_pixi_workspace: Path
) -> None:
    test_data = build_data.joinpath("rattler-build-backend")
    workspace_dir = tmp_pixi_workspace / "keep-going"
    copytree_with_local_backend(test_data / "keep-going", workspace_dir)

    cache_dir = tmp_pixi_workspace / "pixi-cache"
    env = {"PIXI_CACHE_DIR": str(cache_dir)}

    manifest_path = workspace_dir / "pixi.toml"
    verify_cli_command(
        [
            pixi,
            "config",
            "set",
            "--manifest-path",
            manifest_path,
            "--local",
            "concurrency.builds",
            "3",
        ],
        env=env,
    )

    output = verify_cli_command(
        [pixi, "install", "--quiet", "--manifest-path", manifest_path],
        expected_exit_code=ExitCode.FAILURE,
        env=env,
        stderr_contains=[
            "1 source package failed to build, 1 skipped",
            "skipped dependent: depends on failed `bad`",
            "build of bad failed:",
            "boom-marker-line",
            "full log:",
        ],
    )

    # `good` does not depend on the failed package, so it must still have
    # been built into a package artifact even though the overall install
    # fails before anything gets linked into the prefix.
    good_artifacts = list(workspace_dir.joinpath(".pixi", "bld", "good").rglob("*.conda"))
    assert good_artifacts, (
        f"expected 'good' to have been built despite 'bad' failing, stderr:\n{output.stderr}"
    )
    # `dependent` was skipped, not attempted: no build directory for it at all.
    assert not workspace_dir.joinpath(".pixi", "bld", "dependent").exists(), (
        "'dependent' should have been skipped, not built"
    )

    log_path = cache_dir / "build-logs" / "bad.log"
    assert log_path.is_file(), f"expected a build log at {log_path}"
    assert "boom-marker-line" in log_path.read_text()


@pytest.mark.slow
def test_keep_going_concurrent_builds_all_succeed(
    pixi: Path, build_data: Path, tmp_pixi_workspace: Path
) -> None:
    test_data = build_data.joinpath("rattler-build-backend")
    workspace_dir = tmp_pixi_workspace / "keep-going-success"
    copytree_with_local_backend(test_data / "keep-going-success", workspace_dir)

    cache_dir = tmp_pixi_workspace / "pixi-cache"
    env = {"PIXI_CACHE_DIR": str(cache_dir)}

    manifest_path = workspace_dir / "pixi.toml"
    verify_cli_command(
        [
            pixi,
            "config",
            "set",
            "--manifest-path",
            manifest_path,
            "--local",
            "concurrency.builds",
            "3",
        ],
        env=env,
    )

    verify_cli_command(
        [pixi, "install", "--manifest-path", manifest_path],
        expected_exit_code=ExitCode.SUCCESS,
        env=env,
    )

    conda_meta = workspace_dir / ".pixi" / "envs" / "default" / "conda-meta"
    for pkg in ("pkg-a", "pkg-b", "pkg-c"):
        assert list(conda_meta.glob(f"{pkg}-*.json")), f"expected {pkg} to be built and linked"
