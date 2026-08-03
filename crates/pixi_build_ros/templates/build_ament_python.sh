set -eo pipefail

# Stage source into a work-dir-local copy and drop a synthesized package.xml
# next to setup.py. setup.py declares package.xml as a data_files entry, so
# the file must exist in the source tree at install time.
#
# In pixi-native mode @SRC_DIR@ is the user's manifest dir and $PWD is
# <SRC_DIR>/.pixi/build/work/.../work — STAGE_DIR therefore sits inside
# @SRC_DIR@ and a plain `cp -a SRC_DIR/. STAGE_DIR/` recurses into itself.
# Tar with excludes side-steps that and skips junk we don't want in the build.
STAGE_DIR="$PWD/src_stage"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"
tar -C "@SRC_DIR@" --exclude=./.pixi --exclude=./.git -cf - . | tar -C "$STAGE_DIR" -xf -

cat > "$STAGE_DIR/package.xml" <<'__PIXI_NATIVE_PACKAGE_XML__'
@PACKAGE_XML_CONTENT@
__PIXI_NATIVE_PACKAGE_XML__

export SRC_DIR="$STAGE_DIR"

pushd $SRC_DIR

# If there is a setup.cfg that contains install-scripts then we should not set it here
if [ -f setup.cfg ] && grep -q "install[-_]scripts" setup.cfg; then
    # Remove e.g. ros-humble- from PKG_NAME
    PKG_NAME_SHORT=${PKG_NAME#*ros-@DISTRO@-}
    # Substitute "-" with "_"
    PKG_NAME_SHORT=${PKG_NAME_SHORT//-/_}
    INSTALL_SCRIPTS_ARG="--install-scripts=$PREFIX/lib/$PKG_NAME_SHORT"
    echo "WARNING: setup.cfg not set, will set INSTALL_SCRIPTS_ARG to: $INSTALL_SCRIPTS_ARG"
    # The prefix is reused, so record what this build installed instead of
    # letting rattler-build package the previous build's leftovers as well.
    $PYTHON setup.py install --force --prefix="$PREFIX" --install-lib="$SP_DIR" $INSTALL_SCRIPTS_ARG --single-version-externally-managed --record="$RATTLER_BUILD_PACKAGE_FILES"

    # setuptools lists byte-compiled files in --record whether or not it wrote
    # them, and bytecode generation is off in the build environment. Any missing
    # path makes rattler-build abort while packaging, so keep only what landed.
    _kept_files="$(mktemp)"
    while IFS= read -r _recorded; do
        if [ -e "$_recorded" ]; then
            printf '%s\n' "$_recorded"
        fi
    done < "$RATTLER_BUILD_PACKAGE_FILES" > "$_kept_files"
    mv "$_kept_files" "$RATTLER_BUILD_PACKAGE_FILES"

    # Remove build artifacts from setup.py install
    rm -rf *.egg-info 2>/dev/null || true
    rm -rf build/ 2>/dev/null || true
else
    # pip uninstalls the previous install before reinstalling, so nothing is left
    # behind and rattler-build can keep deriving the contents from the prefix.
    $PYTHON -m pip install . --no-deps --force-reinstall -vvv
fi
