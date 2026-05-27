use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::utils::platform::LibraryRule;

// ── Mojang version manifest ──────────────────────────────────────────────────
// Fetched from launchermeta.mojang.com/mc/game/version_manifest_v2.json

#[derive(Debug, Clone, Deserialize)]
pub struct MojangVersionManifest {
    pub latest: LatestVersions,
    pub versions: Vec<VersionEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LatestVersions {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub version_type: String,
    pub url: String,
    pub time: String,
    pub release_time: String,
}

// ── Per-version JSON (e.g. 1.20.1.json) ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftVersionJson {
    pub id: String,
    #[serde(rename = "type")]
    pub version_type: String,
    pub assets: Option<String>,
    pub asset_index: Option<AssetIndexRef>,
    pub downloads: Option<VersionDownloads>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    pub main_class: Option<String>,
    pub java_version: Option<JavaVersionInfo>,
    // Legacy format (pre-1.13): space-separated arg string
    pub minecraft_arguments: Option<String>,
    // Modern format (1.13+): structured args
    pub arguments: Option<Arguments>,
    // Runtime field: true when native JARs were found and extracted.
    // Not present in Mojang JSON; stored in our game_data.json.
    #[serde(rename = "nativesList", default)]
    pub has_natives: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndexRef {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub total_size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionDownloads {
    pub client: DownloadArtifact,
    pub server: Option<DownloadArtifact>,
    pub client_mappings: Option<DownloadArtifact>,
    pub server_mappings: Option<DownloadArtifact>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadArtifact {
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersionInfo {
    pub component: Option<String>,
    pub major_version: Option<u32>,
}

// ── Arguments ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Arguments {
    pub game: Option<Vec<GameArgEntry>>,
    // JVM args can also be conditional objects with rules
    pub jvm: Option<Vec<serde_json::Value>>,
}

/// Modern (1.13+) game argument: either a plain string or a conditional object.
/// Conditional objects are dropped during argument construction (same as JS).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GameArgEntry {
    Plain(String),
    Conditional(serde_json::Value),
}

// ── Library ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Library {
    pub name: String,
    /// OS/feature rules from Mojang. Evaluated by `utils::platform::skip_library`.
    pub rules: Option<Vec<LibraryRule>>,
    /// Maps OS names to native classifier suffixes (e.g. `"linux" → "natives-linux"`).
    pub natives: Option<HashMap<String, String>>,
    pub downloads: Option<LibraryDownloads>,
    /// Base repository URL — used by Fabric/Quilt loader libraries (not in Mojang JSON).
    #[serde(default)]
    pub url: Option<String>,
    /// Loader root path — injected at runtime when building loader classpath.
    /// Not present in Mojang JSON; set by the loader installer.
    #[serde(default)]
    pub loader: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibraryDownloads {
    pub artifact: Option<ArtifactInfo>,
    /// Keyed by classifier string (e.g. `"natives-linux-aarch64"`).
    pub classifiers: Option<HashMap<String, ArtifactInfo>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactInfo {
    pub sha1: Option<String>,
    pub size: Option<u64>,
    /// Relative Maven path (e.g. `"net/minecraftforge/forge/1.19-41.0.63/forge-1.19-41.0.63.jar"`).
    pub path: Option<String>,
    pub url: String,
}

// ── Asset index ──────────────────────────────────────────────────────────────

/// Content of the asset index JSON (fetched from `assetIndex.url`).
#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndexData {
    pub objects: HashMap<String, AssetObject>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetObject {
    pub hash: String,
    pub size: u64,
}

/// Resolved bundle entry produced by the game/* modules.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AssetItem {
    /// A file whose content is known at resolve-time — written verbatim to disk
    /// without downloading (e.g. `assets/indexes/<id>.json`,
    /// `versions/<id>/<id>.json`).
    CFile { path: String, content: String },
    /// A regular downloadable file verified by SHA-1.
    Asset {
        path: String,
        sha1: String,
        size: u64,
        url: String,
    },
    /// A native JAR (`.dll`/`.so`/`.dylib` bundle) — downloaded like `Asset`
    /// but then extracted to `versions/<id>/natives/` by `extract_natives`.
    NativeAsset {
        path: String,
        sha1: String,
        size: u64,
        url: String,
    },
}

// ── Authenticator ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authenticator {
    pub access_token: String,
    pub name: String,
    pub uuid: String,
    #[serde(default)]
    pub xbox_account: Option<XboxAccount>,
    #[serde(default)]
    pub user_properties: Option<String>,
    #[serde(default)]
    pub meta: Option<AuthMeta>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct XboxAccount {
    pub xuid: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthMeta {
    #[serde(rename = "type")]
    pub auth_type: String,
}
