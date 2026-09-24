"""Cut a `pixi-gr@<version>` release of the Greenroom pixi fork.

Upstream's release flow (release.py + the .github workflows) is disabled in
this fork: we don't publish every target, don't sign, and tag as
`pixi-gr@X.Y.Z` rather than `vX.Y.Z` so the fork's releases never collide with
upstream's. This script is the whole GR release process.

Steps:
    1. Take the version from --version, or bump the latest `pixi-gr@` tag from
       the conventional commits since it with git-cliff.
    2. Cross-build `pixi` for linux-64 and linux-aarch64 with cargo-zigbuild.
    3. gzip each binary to staging/pixi-<arch>.gz.
    4. Rewrite the default VERSION in install.sh to match, and commit if it moved.
    5. Tag `pixi-gr@<version>` at HEAD and push.
    6. Create the GitHub release with install.sh + both binaries.

Usage:
    pixi run -e release release-gr [--dry-run] [--version X.Y.Z]
"""

import argparse
import gzip
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGING = ROOT / "staging"
INSTALL_SH = ROOT / "install.sh"
REPO = "greenroom-robotics/pixi"

# Release asset name -> rust target triple. musl keeps the binary static so it
# runs on whatever glibc the fleet happens to have.
TARGETS = {
    "linux-64": "x86_64-unknown-linux-musl",
    "linux-aarch64": "aarch64-unknown-linux-musl",
}


def run(cmd: list[str], dry_run: bool = False, **kwargs: object) -> subprocess.CompletedProcess[str]:
    print(f"  -> {' '.join(cmd)}", flush=True)
    if dry_run:
        return subprocess.CompletedProcess(cmd, 0, "", "")
    return subprocess.run(cmd, check=True, text=True, **kwargs)  # type: ignore[arg-type]


def capture(cmd: list[str]) -> str:
    return subprocess.run(cmd, check=True, text=True, capture_output=True).stdout.strip()


def bumped_version() -> str:
    # On 0.x, feat and breaking commits bump patch rather than minor so the fork
    # never claims an upstream version number it didn't ship.
    env = {
        **os.environ,
        "GIT_CLIFF__BUMP__FEATURES_ALWAYS_BUMP_MINOR": "false",
        "GIT_CLIFF__BUMP__BREAKING_ALWAYS_BUMP_MAJOR": "false",
    }
    tag = subprocess.run(
        ["git-cliff", "--offline", "--bumped-version", "--tag-pattern", "^pixi-gr@"],
        check=True,
        text=True,
        capture_output=True,
        env=env,
    ).stdout.strip()
    return tag.removeprefix("pixi-gr@")


def semver(value: str) -> str:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", value):
        raise argparse.ArgumentTypeError(f"{value!r} is not X.Y.Z")
    return value


def build(target: str, version: str, dry_run: bool) -> Path:
    # `pixi --version` reads consts::PIXI_VERSION, whose fallback is a hardcoded
    # string that only upstream's tbump run keeps in step with Cargo.toml. Set
    # the compile-time override so the release tag is the single source of truth
    # and a binary can never self-report a stale version.
    env_extra = {"PIXI_VERSION": version}
    # aarch64 Linux hosts may use 64 KiB pages; jemalloc needs the page size at
    # compile time. Mirrors scripts/build_options.py.
    if target.startswith("aarch64-"):
        env_extra["JEMALLOC_SYS_WITH_LG_PAGE"] = "16"
    env = {**os.environ, **env_extra}
    print(f"  -> cargo zigbuild --release --target {target} --bin pixi", flush=True)
    if not dry_run:
        subprocess.run(
            ["cargo", "zigbuild", "--release", "--target", target, "--bin", "pixi"],
            check=True,
            env=env,
        )
    return ROOT / "target" / target / "release" / "pixi"


def package(binary: Path, arch: str, dry_run: bool) -> Path:
    dest = STAGING / f"pixi-{arch}.gz"
    print(f"  -> gzip {binary} -> {dest}", flush=True)
    if dry_run:
        return dest
    if not binary.is_file():
        sys.exit(f"error: {binary} not found")
    STAGING.mkdir(parents=True, exist_ok=True)
    with binary.open("rb") as src, gzip.open(dest, "wb") as out:
        shutil.copyfileobj(src, out)
    return dest


def sync_install_sh(version: str, dry_run: bool) -> bool:
    """Point install.sh's default VERSION at this release. True if it changed."""
    text = INSTALL_SH.read_text()
    updated = re.sub(
        r'^VERSION="\$\{PIXI_GR_VERSION:-[^}]*\}"$',
        f'VERSION="${{PIXI_GR_VERSION:-{version}}}"',
        text,
        count=1,
        flags=re.MULTILINE,
    )
    updated = re.sub(r"pixi-gr@[0-9]+\.[0-9]+\.[0-9]+", f"pixi-gr@{version}", updated)
    if updated == text:
        return False
    print(f"  -> install.sh VERSION -> {version}", flush=True)
    if not dry_run:
        INSTALL_SH.write_text(updated)
    return True


def main() -> None:
    parser = argparse.ArgumentParser(description="Cut a pixi-gr release")
    parser.add_argument("--dry-run", action="store_true", help="Build nothing, tag nothing, publish nothing")
    parser.add_argument(
        "--version",
        type=semver,
        help="Release this version instead of bumping from commits, e.g. to sync with upstream",
    )
    args = parser.parse_args()

    version = args.version or bumped_version()
    tag = f"pixi-gr@{version}"
    print(f"Releasing {tag}")

    if capture(["git", "status", "--porcelain"]):
        sys.exit("error: working tree is dirty; commit or stash first")

    if sync_install_sh(version, args.dry_run) and not args.dry_run:
        run(["git", "add", str(INSTALL_SH)])
        run(["git", "commit", "-m", f"chore: point install.sh at {tag}"])

    assets = [INSTALL_SH]
    for arch, target in TARGETS.items():
        assets.append(package(build(target, version, args.dry_run), arch, args.dry_run))

    existing = subprocess.run(
        ["git", "rev-parse", f"{tag}^{{commit}}"], capture_output=True, text=True
    )
    head = capture(["git", "rev-parse", "HEAD"])
    if existing.returncode == 0 and existing.stdout.strip() != head:
        sys.exit(f"error: {tag} already exists at another commit (nothing to release since it?)")
    if existing.returncode != 0:
        run(["git", "tag", tag], args.dry_run)
    run(["git", "push", "origin", "HEAD", tag], args.dry_run)

    notes = (
        f"GR pixi fork with native `az://` Azure Blob channel support.\n\n"
        f"Install (detects arch, installs as `pixi`):\n"
        f"```sh\ncurl -fsSL https://github.com/{REPO}/releases/download/{tag}/install.sh | sh\n```"
    )
    run(
        [
            "gh", "release", "create", tag,
            "--repo", REPO,
            "--title", tag,
            "--notes", notes,
            "--latest",
            *[str(a) for a in assets],
        ],
        args.dry_run,
    )
    print(f"\nDone: https://github.com/{REPO}/releases/tag/{tag}")


if __name__ == "__main__":
    main()
