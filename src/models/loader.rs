use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Loader type enum ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoaderType {
    Forge,
    NeoForge,
    Fabric,
    // serde rename_all = "lowercase" produces "legacyfabric" ✓
    LegacyFabric,
    Quilt,
}

impl std::fmt::Display for LoaderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LoaderType::Forge => "forge",
            LoaderType::NeoForge => "neoforge",
            LoaderType::Fabric => "fabric",
            LoaderType::LegacyFabric => "legacyfabric",
            LoaderType::Quilt => "quilt",
        };
        f.write_str(s)
    }
}

// ── Common loader library ────────────────────────────────────────────────────
// Used by Fabric, Quilt, Forge, and NeoForge installer JSONs.
// Simpler than the Mojang `Library` type: no native classifiers, and `rules`
// is treated as opaque (we only check presence, not content).

#[derive(Debug, Deserialize, Serialize)]
pub struct LoaderLibrary {
    pub name: String,
    /// Base Maven repository URL (Fabric: `"https://maven.fabricmc.net"`).
    #[serde(default)]
    pub url: Option<String>,
    pub downloads: Option<LoaderLibraryDownloads>,
    /// When present (any value), this library should be skipped.
    #[serde(default)]
    pub rules: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LoaderLibraryDownloads {
    pub artifact: Option<LoaderArtifact>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LoaderArtifact {
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub path: Option<String>,
    pub url: String,
}

// ── Fabric / LegacyFabric / Quilt ────────────────────────────────────────────

/// Response from `meta.fabricmc.net/v2/versions` (and equivalents for Quilt/LegacyFabric).
#[derive(Debug, Deserialize)]
pub struct FabricMeta {
    pub game: Vec<FabricGameVersion>,
    pub loader: Vec<FabricLoaderBuild>,
}

#[derive(Debug, Deserialize)]
pub struct FabricGameVersion {
    pub version: String,
    pub stable: bool,
}

#[derive(Debug, Deserialize)]
pub struct FabricLoaderBuild {
    pub version: String,
    pub stable: bool,
}

/// Quilt's loader build has extra fields and no `stable` flag.
#[derive(Debug, Deserialize)]
pub struct QuiltLoaderBuild {
    pub version: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Deserialize)]
pub struct QuiltMeta {
    pub game: Vec<FabricGameVersion>,
    pub loader: Vec<QuiltLoaderBuild>,
}

/// Profile JSON fetched from the Fabric/Quilt meta API
/// (e.g. `meta.fabricmc.net/v2/versions/loader/{mc}/{build}/profile/json`).
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FabricJson {
    pub id: String,
    #[serde(default)]
    pub libraries: Vec<LoaderLibrary>,
    pub main_class: Option<String>,
    pub minecraft_arguments: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// ── Forge / NeoForge ─────────────────────────────────────────────────────────
// `install_profile.json` is highly dynamic. Critical fields are typed;
// the rest is captured in `extra` to round-trip without data loss.

/// Variable entry in the Forge profile `data` map.
/// Each variable has a `client` value (and sometimes `server`).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProfileDataEntry {
    pub client: String,
    #[serde(default)]
    pub server: Option<String>,
}

/// `arguments` block inside a Forge/NeoForge version JSON (1.13+ format).
#[derive(Debug, Deserialize, Serialize, Default)]
pub struct ForgeArguments {
    #[serde(default)]
    pub game: Vec<serde_json::Value>,
    #[serde(default)]
    pub jvm: Vec<serde_json::Value>,
}

/// Top-level `install_profile.json`. Dual format:
/// - **Old format** (pre-1.13): has a top-level `install` key → the profile IS `install`.
/// - **New format** (1.13+): `install` and `version` are separate sections.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ForgeProfile {
    pub install: Option<ForgeInstallSection>,
    pub version: Option<ForgeVersionSection>,
    /// Variable map used by processors (`{BINPATCH}`, `{MAPPINGS}`, etc.).
    /// Present in new-format (1.13+) profiles.
    pub data: Option<HashMap<String, ProfileDataEntry>>,
    /// Path within the installer ZIP to the primary artifact (old format).
    pub file_path: Option<String>,
    /// Maven coordinate of the primary Forge artifact (old format).
    pub path: Option<String>,
    pub processors: Option<Vec<ForgeProcessor>>,
    pub libraries: Option<Vec<LoaderLibrary>>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ForgeInstallSection {
    pub libraries: Option<Vec<LoaderLibrary>>,
    /// Name of the version JSON file inside the installer JAR.
    pub json: Option<String>,
    pub path: Option<String>,
    pub file_path: Option<String>,
    pub minecraft: Option<String>,
    pub processors: Option<Vec<ForgeProcessor>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ForgeVersionSection {
    pub id: Option<String>,
    pub libraries: Option<Vec<LoaderLibrary>>,
    pub main_class: Option<String>,
    pub minecraft_arguments: Option<String>,
    /// Modern 1.13+ argument block (game/jvm arrays).
    pub arguments: Option<ForgeArguments>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A Forge installer processor: a Java tool that patches/transforms Minecraft files.
#[derive(Debug, Deserialize, Serialize)]
pub struct ForgeProcessor {
    pub jar: String,
    pub classpath: Vec<String>,
    pub args: Vec<String>,
    #[serde(default)]
    pub sides: Option<Vec<String>>,
}

// ── Internal result types (not from JSON) ────────────────────────────────────

/// Result of a successful installer JAR download.
#[derive(Debug)]
pub struct InstallerInfo {
    pub file_path: String,
    pub meta_data: String,
    pub ext: String,
    pub id: String,
    /// NeoForge only: whether to use the legacy (1.20.1-era) API path.
    pub old_api: bool,
}
