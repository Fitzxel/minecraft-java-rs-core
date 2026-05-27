use crate::models::loader::LoaderType;
use crate::models::minecraft::AssetItem;

// ── Input / Output types for the ModLoader trait ──────────────────────────────

/// Context provided by the orchestrator before loader installation begins.
#[derive(Debug, Clone)]
pub struct LoaderInstallInput {
    /// Resolved Minecraft version string (e.g. `"1.20.4"`, never an alias).
    pub mc_version: String,
    /// Path to the Java binary used to run installer processors.
    pub java_path: String,
    /// Absolute path to the Minecraft client JAR.
    pub mc_jar: String,
    /// Absolute path to the Minecraft version JSON file on disk.
    pub mc_json: String,
}

/// Returned by a successful loader installation.
#[derive(Debug)]
pub struct LoaderResult {
    /// Additional library JARs to include in the classpath.
    pub libraries: Vec<AssetItem>,
    /// Main class override; `None` means use the vanilla main class.
    pub main_class: Option<String>,
    /// Installed loader version string (e.g. `"neoforge-21.1.0"`).
    pub loader_version: String,
    /// Which loader type was installed.
    pub loader_type: LoaderType,
    /// Extra plain-string game arguments from the loader JSON
    /// (`minecraftArguments` string or `arguments.game` array).
    pub extra_game_args: Vec<String>,
    /// Extra JVM arguments from the loader version JSON (`arguments.jvm`),
    /// already resolved (placeholders replaced with real paths).
    pub extra_jvm_args: Vec<String>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_install_input_constructs() {
        let input = LoaderInstallInput {
            mc_version: "1.20.4".into(),
            java_path: "/usr/bin/java".into(),
            mc_jar: "/mc/versions/1.20.4/1.20.4.jar".into(),
            mc_json: "/mc/versions/1.20.4/1.20.4.json".into(),
        };
        assert_eq!(input.mc_version, "1.20.4");
        assert_eq!(input.java_path, "/usr/bin/java");
    }

    #[test]
    fn loader_result_constructs() {
        let result = LoaderResult {
            libraries: vec![],
            main_class: Some("net.fabricmc.loader.impl.launch.knot.KnotClient".into()),
            loader_version: "fabric-loader-0.15.6-1.20.4".into(),
            loader_type: LoaderType::Fabric,
            extra_game_args: vec![],
            extra_jvm_args: vec![],
        };
        assert_eq!(result.loader_type, LoaderType::Fabric);
        assert!(result.main_class.is_some());
        assert!(result.libraries.is_empty());
    }

    #[test]
    fn loader_result_no_main_class() {
        let result = LoaderResult {
            libraries: vec![],
            main_class: None,
            loader_version: "forge-47.1.0".into(),
            loader_type: LoaderType::Forge,
            extra_game_args: vec!["--launchTarget".into(), "fmlclient".into()],
            extra_jvm_args: vec![],
        };
        assert!(result.main_class.is_none());
        assert_eq!(result.loader_type, LoaderType::Forge);
        assert_eq!(result.extra_game_args.len(), 2);
    }
}
