//! Shared input-glob list. Both the package.xml and pixi-native code paths
//! reference it so changes to "what counts as a source change" stay in
//! lockstep.

/// Globs that should always invalidate the build cache for any ROS package.
///
/// Python sources are included unconditionally: every `ament_*` template
/// copies them into the built package, so a `.py` edit changes the artifact
/// even when the frontend asked for an editable build.
pub(crate) const ROS_SOURCE_GLOBS: &[&str] = &[
    "**/*.c",
    "**/*.cpp",
    "**/*.h",
    "**/*.hpp",
    "**/*.rs",
    "**/*.sh",
    "**/*.py",
    "**/*.pyx",
    "package.xml",
    "setup.cfg",
    "pyproject.toml",
    "Makefile",
    "CMakeLists.txt",
    "MANIFEST.in",
    "Cargo.toml",
    "Cargo.lock",
    "docs/**/*.rst",
    "docs/**/*.md",
    "config/*.yaml",
    "msg/**/*.msg",
    "srv/**/*.srv",
    "action/**/*.action",
];
