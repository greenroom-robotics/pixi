//! Shared input-glob lists: what counts as a source change for a ROS package.

/// Globs that always invalidate the build cache for any ROS package.
///
/// `setup.py` bakes entry points and `data_files` in at install time, so it
/// forces a rebuild even when module sources don't.
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

/// Python module sources, tracked separately: a [`PythonInstall::Symlinked`]
/// build serves them from the source tree, so they must not trigger a rebuild.
///
/// [`PythonInstall::Symlinked`]: crate::build_script::PythonInstall::Symlinked
pub(crate) const ROS_PYTHON_SOURCE_GLOBS: &[&str] = &["**/*.py", "**/*.pyx"];
