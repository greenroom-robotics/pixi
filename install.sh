#!/bin/sh
# Install the GR pixi fork (native az:// Azure Blob channel support) as `pixi`.
#   curl -fsSL https://github.com/greenroom-robotics/pixi/releases/download/pixi-gr@0.75.1/install.sh | sh
# Override version/dest with PIXI_GR_VERSION / PIXI_GR_BIN_DIR.
set -eu

VERSION="${PIXI_GR_VERSION:-0.75.1}"
DEST="${PIXI_GR_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -m)" in
  x86_64 | amd64) arch=linux-64 ;;
  aarch64 | arm64) arch=linux-aarch64 ;;
  *) echo "pixi-gr: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

# Note any pixi already on PATH before we install — the GR build is meant to
# replace it, so a different one left ahead of $DEST would silently win.
existing="$(command -v pixi || true)"

url="https://github.com/greenroom-robotics/pixi/releases/download/pixi-gr@${VERSION}/pixi-${arch}.gz"
mkdir -p "$DEST"
echo "pixi-gr ${VERSION} (${arch}) -> ${DEST}/pixi"
curl -fsSL "$url" | gunzip > "$DEST/pixi"
chmod +x "$DEST/pixi"

if [ -n "$existing" ] && [ "$existing" != "$DEST/pixi" ]; then
  echo "warning: another pixi is on PATH at $existing" >&2
  echo "         remove it (or put $DEST first) so this GR build is the one that runs." >&2
fi

# $DEST holds the `pixi` binary; $PIXI_HOME/bin holds the trampolines that
# `pixi global install` writes, so both have to be on PATH.
for dir in "$DEST" "${PIXI_HOME:-$HOME/.pixi}/bin"; do
  case ":$PATH:" in *":$dir:"*) continue ;; esac
  shell="${SHELL:-}"
  case "${shell##*/}" in
    bash) rc="$HOME/.bashrc"; line="export PATH=\"$dir:\$PATH\"" ;;
    zsh) rc="$HOME/.zshrc"; line="export PATH=\"$dir:\$PATH\"" ;;
    fish) rc="$HOME/.config/fish/config.fish"; line="set -gx PATH \"$dir\" \$PATH" ;;
    *) echo "note: add $dir to your PATH" >&2; continue ;;
  esac
  grep -Fxq "$line" "$rc" 2>/dev/null && continue
  mkdir -p "$(dirname "$rc")"
  printf '\n%s\n' "$line" >>"$rc"
  echo "added $dir to PATH in $rc — restart or source your shell"
done
