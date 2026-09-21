//! Build script template selection and variable substitution.

use std::path::Path;

use miette::Diagnostic;
use rattler_conda_types::Platform;
use thiserror::Error;

/// Errors that can occur during build script generation.
#[derive(Debug, Error, Diagnostic)]
pub enum BuildScriptError {
    #[error("unsupported ROS build type: '{build_type}'")]
    #[diagnostic(help(
        "Supported build types are: ament_cmake, ament_python, ament_cargo (linux only), ament_idl, cmake, catkin"
    ))]
    UnsupportedBuildType { build_type: String },
}

/// How Python modules end up in the install prefix.
///
/// Symlinked modules are served from the source tree, so the same value drives
/// the build script and the build input globs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonInstall {
    Copied,
    Symlinked,
}

impl PythonInstall {
    pub fn resolve(build_type: &str, editable: bool) -> Self {
        if editable && build_type == "ament_python" {
            Self::Symlinked
        } else {
            Self::Copied
        }
    }
}

/// Render a build script from the appropriate template.
///
/// Selects the template based on `build_type` and platform, then performs
/// variable substitution.
///
/// `package_xml` is consulted for every `ament_*` build type. The template
/// substitutes it into a heredoc that writes the file alongside a staged
/// source copy at build time. `cmake`/`catkin` templates ignore it.
pub fn render_build_script(
    build_type: &str,
    distro: &str,
    source_dir: &Path,
    package_xml: Option<&str>,
    python_install: PythonInstall,
) -> Result<String, BuildScriptError> {
    // Use the current (build) platform, not the host/target platform.
    // The build script runs on the build machine.
    let is_windows = Platform::current().is_windows();
    let template = select_template(build_type, is_windows)?;

    let src_dir_str = source_dir.display().to_string();
    let mut rendered = template
        .replace("@SRC_DIR@", &src_dir_str)
        .replace("@DISTRO@", distro)
        .replace("@BUILD_DIR@", "build")
        .replace("@BUILD_TYPE@", "Release")
        .replace(
            "@SYMLINK_INSTALL@",
            match python_install {
                PythonInstall::Symlinked => "1",
                PythonInstall::Copied => "0",
            },
        );

    if let Some(xml) = package_xml {
        rendered = rendered.replace("@PACKAGE_XML_CONTENT@", xml);
    }

    Ok(rendered)
}

fn select_template(build_type: &str, is_windows: bool) -> Result<&'static str, BuildScriptError> {
    match (build_type, is_windows) {
        ("ament_cmake", false) => Ok(include_str!("../templates/build_ament_cmake.sh")),
        ("ament_cmake", true) => Ok(include_str!("../templates/bld_ament_cmake.bat")),
        ("ament_python", false) => Ok(include_str!("../templates/build_ament_python.sh")),
        ("ament_python", true) => Ok(include_str!("../templates/bld_ament_python.bat")),
        ("ament_cargo", false) => Ok(include_str!("../templates/build_ament_cargo.sh")),
        // ament_idl uses the same template as ament_cmake; the synthesized
        // package.xml differs (it carries the rosidl_interface_packages
        // member_of_group declaration), but the build flow is identical.
        ("ament_idl", false) => Ok(include_str!("../templates/build_ament_cmake.sh")),
        ("cmake" | "catkin", false) => Ok(include_str!("../templates/build_catkin.sh")),
        ("cmake" | "catkin", true) => Ok(include_str!("../templates/bld_catkin.bat")),
        _ => Err(BuildScriptError::UnsupportedBuildType {
            build_type: build_type.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_render_ament_cmake() {
        let script =
            render_build_script("ament_cmake", "humble", &PathBuf::from("/my/source"), None, PythonInstall::Copied)
                .unwrap();

        assert!(script.contains("/my/source"));
        assert!(script.contains("Release"));
        assert!(!script.contains("@SRC_DIR@"));
        assert!(!script.contains("@BUILD_TYPE@"));
    }

    #[test]
    fn test_render_ament_python() {
        let script =
            render_build_script("ament_python", "jazzy", &PathBuf::from("/src"), None, PythonInstall::Copied).unwrap();

        assert!(script.contains("/src"));
        assert!(!script.contains("@SRC_DIR@"));
    }

    #[test]
    fn test_render_catkin() {
        let script = render_build_script("catkin", "noetic", &PathBuf::from("/pkg"), None, PythonInstall::Copied).unwrap();

        assert!(script.contains("/pkg"));
        assert!(script.contains("noetic"));
    }

    #[test]
    fn test_render_ament_cargo() {
        let script =
            render_build_script("ament_cargo", "kilted", &PathBuf::from("/work"), None, PythonInstall::Copied).unwrap();

        assert!(script.contains("cargo ament-build"));
        assert!(script.contains("/work"));
        assert!(script.contains("kilted"));
        assert!(!script.contains("@SRC_DIR@"));
        assert!(!script.contains("@DISTRO@"));
    }

    #[test]
    fn test_python_install_resolve() {
        assert_eq!(
            PythonInstall::resolve("ament_python", true),
            PythonInstall::Symlinked
        );
        assert_eq!(
            PythonInstall::resolve("ament_python", false),
            PythonInstall::Copied
        );
        assert_eq!(
            PythonInstall::resolve("ament_cmake", true),
            PythonInstall::Copied
        );
    }

    #[test]
    fn test_symlink_install_toggle() {
        let src = PathBuf::from("/src");
        let symlinked =
            render_build_script("ament_python", "jazzy", &src, None, PythonInstall::Symlinked)
                .unwrap();
        let copied =
            render_build_script("ament_python", "jazzy", &src, None, PythonInstall::Copied)
                .unwrap();

        assert!(symlinked.contains(r#"if [ "1" = "1" ]"#));
        assert!(copied.contains(r#"if [ "0" = "1" ]"#));
        assert!(!symlinked.contains("@SYMLINK_INSTALL@"));
    }

    #[test]
    fn test_unsupported_build_type() {
        let result = render_build_script("unknown_type", "jazzy", &PathBuf::from("/src"), None, PythonInstall::Copied);
        assert!(matches!(
            result,
            Err(BuildScriptError::UnsupportedBuildType { .. })
        ));
    }
}
