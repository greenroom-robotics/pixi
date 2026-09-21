//! Shared input-glob lists. Both the package.xml and pixi-native code paths
//! reference them so changes to "what counts as a source change" stay in
//! lockstep.

/// Globs that always invalidate the build cache for any ROS package.
///
/// `setup.py` is listed explicitly rather than left to
/// [`ROS_PYTHON_SOURCE_GLOBS`]: entry points and `data_files` are baked in at
/// install time, so it must force a rebuild even when module sources don't.
pub(crate) const ROS_SOURCE_GLOBS: &[&str] = &[
    "**/*.c",
    "**/*.cpp",
    "**/*.h",
    "**/*.hpp",
    "**/*.rs",
    "**/*.sh",
    "package.xml",
    "setup.py",
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

/// Python module sources, tracked separately because a
/// [`PythonInstall::Symlinked`] build serves them straight from the source
/// tree and so must not rebuild when they change.
///
/// [`PythonInstall::Symlinked`]: crate::build_script::PythonInstall::Symlinked
pub(crate) const ROS_PYTHON_SOURCE_GLOBS: &[&str] = &["**/*.py", "**/*.pyx"];
