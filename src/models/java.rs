use serde::{Deserialize, Serialize};

// ── Java file item ────────────────────────────────────────────────────────────
// Represents one file in the Java runtime as listed in Mojang's manifest.

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JavaFileItem {
    pub path: String,
    pub executable: Option<bool>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
}

// ── Mojang all.json ───────────────────────────────────────────────────────────
// Fetched from the hardcoded hash URL in Minecraft-Java.ts.
// Structure: HashMap<arch_os, HashMap<component, Vec<JavaVersionManifest>>>

#[derive(Debug, Deserialize)]
pub struct JavaVersionManifest {
    pub version: Option<JavaVersionName>,
    pub manifest: Option<JavaManifestRef>,
}

#[derive(Debug, Deserialize)]
pub struct JavaVersionName {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct JavaManifestRef {
    pub url: String,
}

// ── Mojang per-version manifest ───────────────────────────────────────────────
// Fetched from the URL inside JavaManifestRef.
// Structure: { "files": HashMap<relative_path, JavaManifestFile> }

#[derive(Debug, Deserialize)]
pub struct JavaManifestData {
    pub files: std::collections::HashMap<String, JavaManifestFile>,
}

#[derive(Debug, Deserialize)]
pub struct JavaManifestFile {
    #[serde(rename = "type")]
    pub file_type: String, // "file", "directory", "link"
    pub downloads: Option<JavaFileDownloads>,
    pub executable: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct JavaFileDownloads {
    pub raw: Option<JavaRawDownload>,
    pub lzma: Option<JavaRawDownload>,
}

#[derive(Debug, Deserialize)]
pub struct JavaRawDownload {
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

// ── Adoptium API ──────────────────────────────────────────────────────────────
// Fallback when Mojang doesn't have the runtime for the current platform.

#[derive(Debug, Deserialize)]
pub struct AdoptiumRelease {
    pub binary: AdoptiumBinary,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumBinary {
    pub package: AdoptiumPackage,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumPackage {
    pub link: String,
    pub name: String,
    pub checksum: Option<String>,
    pub checksum_link: Option<String>,
}
