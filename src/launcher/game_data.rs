use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    error::LaunchError,
    models::{
        java::JavaFileItem,
        loader::LoaderType,
        minecraft::{AssetItem, MinecraftVersionJson},
    },
};

// ── GameData ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameData {
    pub minecraft_json: MinecraftVersionJson,
    /// Loader profile merged on top of the base version JSON (raw JSON value
    /// to avoid coupling game_data to a specific loader type).
    #[serde(default)]
    pub minecraft_loader: Option<serde_json::Value>,
    pub minecraft_version: String,
    pub minecraft_java: JavaInfo,
    /// Extra library JARs added by the mod loader; included in the classpath.
    #[serde(default)]
    pub loader_libraries: Vec<AssetItem>,
    /// Main class override set by the mod loader.
    #[serde(default)]
    pub loader_main_class: Option<String>,
    /// Loader version id, e.g. `"1.20.4-forge-47.4.20"`.
    #[serde(default)]
    pub loader_version_id: Option<String>,
    /// Which loader type was installed (used for Forge-specific JVM flags).
    #[serde(default)]
    pub loader_type: Option<LoaderType>,
    /// Extra plain-string game args contributed by the loader
    /// (from `minecraftArguments` or `arguments.game` in the loader JSON).
    #[serde(default)]
    pub loader_extra_game_args: Vec<String>,
    /// Extra JVM args from the loader version JSON (`arguments.jvm`), pre-resolved.
    #[serde(default)]
    pub loader_extra_jvm_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JavaInfo {
    pub files: Vec<JavaFileItem>,
    pub path: String,
}

// ── Persistence ───────────────────────────────────────────────────────────────

const GAME_DATA_FILE: &str = "gameData.json";

pub async fn load_game_data(dir: &Path) -> Result<GameData, LaunchError> {
    let path = dir.join(GAME_DATA_FILE);
    let raw = tokio::fs::read_to_string(&path).await?;
    let data = serde_json::from_str(&raw)?;
    Ok(data)
}

pub async fn save_game_data(dir: &Path, data: &GameData) -> Result<(), LaunchError> {
    tokio::fs::create_dir_all(dir).await?;
    let json = serde_json::to_string_pretty(data)?;
    tokio::fs::write(dir.join(GAME_DATA_FILE), json).await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_game_data() -> GameData {
        use crate::models::minecraft::{
            AssetIndexRef, DownloadArtifact, MinecraftVersionJson, VersionDownloads,
        };
        GameData {
            minecraft_version: "1.20.4".into(),
            minecraft_loader: None,
            minecraft_java: JavaInfo {
                files: vec![],
                path: "/usr/bin/java".into(),
            },
            loader_libraries: vec![],
            loader_main_class: None,
            loader_version_id: None,
            loader_type: None,
            loader_extra_game_args: vec![],
            loader_extra_jvm_args: vec![],
            minecraft_json: MinecraftVersionJson {
                id: "1.20.4".into(),
                version_type: "release".into(),
                assets: Some("16".into()),
                asset_index: Some(AssetIndexRef {
                    id: "16".into(),
                    url: "https://example.com/16.json".into(),
                    sha1: "abc123".into(),
                    size: 0,
                    total_size: None,
                }),
                downloads: Some(VersionDownloads {
                    client: DownloadArtifact {
                        url: "https://example.com/client.jar".into(),
                        sha1: "def456".into(),
                        size: 0,
                    },
                    server: None,
                    client_mappings: None,
                    server_mappings: None,
                }),
                libraries: vec![],
                arguments: None,
                minecraft_arguments: None,
                java_version: None,
                main_class: Some("net.minecraft.client.main.Main".into()),
                has_natives: false,
            },
        }
    }

    #[tokio::test]
    async fn round_trip_game_data() {
        let dir = TempDir::new().unwrap();
        let original = make_game_data();
        save_game_data(dir.path(), &original).await.unwrap();
        let loaded = load_game_data(dir.path()).await.unwrap();
        assert_eq!(loaded.minecraft_version, "1.20.4");
        assert_eq!(loaded.minecraft_java.path, "/usr/bin/java");
        assert!(loaded.minecraft_loader.is_none());
    }

    #[tokio::test]
    async fn load_missing_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = load_game_data(dir.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn save_creates_missing_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        let data = make_game_data();
        save_game_data(&nested, &data).await.unwrap();
        assert!(nested.join("gameData.json").exists());
    }
}
