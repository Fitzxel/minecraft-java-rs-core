use crate::error::LaunchError;

/// Resolved filesystem path and name for a Maven library coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPath {
    /// Relative directory path, e.g. `"net/minecraftforge/forge/1.19-41.0.63"`.
    pub path: String,
    /// File name with extension, e.g. `"forge-1.19-41.0.63.jar"`.
    pub name: String,
    /// Raw version string from the coordinate.
    pub version: String,
}

/// Maven mirrors tried in order when downloading Minecraft libraries.
pub const MIRRORS: &[&str] = &[
    "https://maven.minecraftforge.net",
    "https://maven.neoforged.net/releases",
    "https://maven.creeperhost.net",
    "https://libraries.minecraft.net",
    "https://repo1.maven.org/maven2",
];

/// Converts a Gradle/Maven coordinate into a relative filesystem path and filename.
///
/// Format: `groupId:artifactId:version` or `groupId:artifactId:version:classifier`
/// Optional `@ext` suffix in version or classifier overrides the file extension.
///
/// # Examples
/// ```
/// use minecraft_java_rs_core::utils::paths::get_path_libraries;
///
/// let r = get_path_libraries("net.minecraftforge:forge:1.19-41.0.63", None, None).unwrap();
/// assert_eq!(r.path, "net/minecraftforge/forge/1.19-41.0.63");
/// assert_eq!(r.name, "forge-1.19-41.0.63.jar");
///
/// let r = get_path_libraries("net.java.dev.jna:jna:5.10.0", Some("-natives-linux"), None).unwrap();
/// assert_eq!(r.name, "jna-5.10.0-natives-linux.jar");
/// ```
pub fn get_path_libraries(
    main: &str,
    native_string: Option<&str>,
    force_ext: Option<&str>,
) -> Result<LibraryPath, LaunchError> {
    let parts: Vec<&str> = main.splitn(5, ':').collect();
    if parts.len() < 3 {
        return Err(LaunchError::InvalidData(format!(
            "invalid library coordinate (need group:artifact:version): '{main}'"
        )));
    }

    let group = parts[0];
    let artifact = parts[1];
    let version = parts[2];
    let classifier = parts.get(3).copied();

    // Base file name: "version" or "version-classifier"
    let file_name = match classifier {
        Some(c) => format!("{version}-{c}"),
        None => version.to_string(),
    };

    // If the file_name contains '@', treat the part after '@' as the extension.
    // Otherwise, append native_string (empty if None) and the extension (.jar by default).
    let final_file_name = if let Some(at) = file_name.find('@') {
        // "1.0@beta" → "1.0.beta"
        let mut s = file_name.clone();
        s.replace_range(at..=at, ".");
        s
    } else {
        let native = native_string.unwrap_or("");
        let ext = force_ext.unwrap_or(".jar");
        format!("{file_name}{native}{ext}")
    };

    // Directory path: group dots → slashes, artifact, version (without @suffix)
    let version_dir = version.split('@').next().unwrap_or(version);
    let path_lib = format!("{}/{artifact}/{version_dir}", group.replace('.', "/"));

    Ok(LibraryPath {
        path: path_lib,
        name: format!("{artifact}-{final_file_name}"),
        version: version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_coordinate() {
        let r = get_path_libraries("net.minecraftforge:forge:1.19-41.0.63", None, None).unwrap();
        assert_eq!(r.path, "net/minecraftforge/forge/1.19-41.0.63");
        assert_eq!(r.name, "forge-1.19-41.0.63.jar");
        assert_eq!(r.version, "1.19-41.0.63");
    }

    #[test]
    fn with_native_string() {
        let r =
            get_path_libraries("net.java.dev.jna:jna:5.10.0", Some("-natives-linux"), None)
                .unwrap();
        assert_eq!(r.path, "net/java/dev/jna/jna/5.10.0");
        assert_eq!(r.name, "jna-5.10.0-natives-linux.jar");
    }

    #[test]
    fn with_classifier() {
        let r =
            get_path_libraries("some.group:artifact:1.0:natives-win", None, None).unwrap();
        assert_eq!(r.name, "artifact-1.0-natives-win.jar");
    }

    #[test]
    fn with_at_extension() {
        let r = get_path_libraries("com.example:lib:1.0@zip", None, None).unwrap();
        assert_eq!(r.name, "lib-1.0.zip");
        // Directory uses the version without the @suffix
        assert_eq!(r.path, "com/example/lib/1.0");
    }

    #[test]
    fn with_force_ext() {
        let r = get_path_libraries("com.example:lib:2.0", None, Some(".zip")).unwrap();
        assert_eq!(r.name, "lib-2.0.zip");
    }

    #[test]
    fn malformed_coordinate_returns_error() {
        assert!(get_path_libraries("no-colons-here", None, None).is_err());
        assert!(get_path_libraries("only:one", None, None).is_err());
    }
}
