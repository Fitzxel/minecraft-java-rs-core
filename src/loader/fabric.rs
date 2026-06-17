use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::models::loader::{FabricJson, FabricMeta, LoaderType};
use crate::models::minecraft::AssetItem;
use crate::net::downloader::{DownloadItem, Downloader};
use crate::net::http::fetch_json;
use crate::utils::paths::get_path_libraries;

// ── Constants ─────────────────────────────────────────────────────────────────

const FABRIC_META: &str = "https://meta.fabricmc.net/v2/versions";
const FABRIC_PROFILE: &str =
    "https://meta.fabricmc.net/v2/versions/loader/${version}/${build}/profile/json";

const LEGACY_META: &str = "https://meta.legacyfabric.net/v2/versions";
const LEGACY_PROFILE: &str =
    "https://meta.legacyfabric.net/v2/versions/loader/${version}/${build}/profile/json";

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricVariant {
    Modern,
    Legacy,
}

pub struct FabricMC {
    variant: FabricVariant,
}

// ── Public API ────────────────────────────────────────────────────────────────

impl FabricMC {
    pub fn new(variant: FabricVariant) -> Self {
        Self { variant }
    }

    pub fn loader_type(&self) -> LoaderType {
        match self.variant {
            FabricVariant::Modern => LoaderType::Fabric,
            FabricVariant::Legacy => LoaderType::LegacyFabric,
        }
    }

    /// Fetch the Fabric/LegacyFabric loader profile JSON for the given Minecraft version.
    ///
    /// `build` can be `"latest"`, `"recommended"` (treated same as latest for Fabric),
    /// or an exact version string.
    pub async fn download_json(
        &self,
        mc_version: &str,
        build: &str,
        client: &reqwest::Client,
    ) -> Result<FabricJson, LoaderError> {
        let (meta_url, profile_template) = match self.variant {
            FabricVariant::Modern => (FABRIC_META, FABRIC_PROFILE),
            FabricVariant::Legacy => (LEGACY_META, LEGACY_PROFILE),
        };

        let meta: FabricMeta = fetch_json(client, meta_url)
            .await
            .map_err(LoaderError::ApiError)?;

        // Validate the MC version is supported.
        let version_name = match self.variant {
            FabricVariant::Modern => "FabricMC",
            FabricVariant::Legacy => "LegacyFabric",
        };
        if !meta.game.iter().any(|g| g.version == mc_version) {
            return Err(LoaderError::VersionNotFound(format!(
                "{version_name} doesn't support Minecraft {mc_version}"
            )));
        }

        // Resolve build.
        let build_ver = if matches!(build, "latest" | "recommended") {
            meta.loader
                .first()
                .map(|b| b.version.clone())
                .ok_or_else(|| {
                    LoaderError::VersionNotFound(format!("No {version_name} builds available"))
                })?
        } else {
            meta.loader
                .iter()
                .find(|b| b.version == build)
                .map(|b| b.version.clone())
                .ok_or_else(|| {
                    let available: Vec<_> =
                        meta.loader.iter().map(|b| b.version.as_str()).collect();
                    LoaderError::VersionNotFound(format!(
                        "{version_name} build {build} not found. Available: {}",
                        available.join(", ")
                    ))
                })?
        };

        let profile_url = profile_template
            .replace("${version}", mc_version)
            .replace("${build}", &build_ver);

        let json: FabricJson = fetch_json(client, &profile_url)
            .await
            .map_err(LoaderError::ApiError)?;

        Ok(json)
    }

    /// Download any libraries from `fabric_json` that are not yet on disk.
    ///
    /// Emits `LaunchEvent::Check` for each library processed.
    /// Returns `Vec<AssetItem>` covering every library (for bundle-integrity tracking).
    pub async fn download_libraries(
        &self,
        options: &LaunchOptions,
        fabric_json: &FabricJson,
        _client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<Vec<AssetItem>, LoaderError> {
        let libs = &fabric_json.libraries;
        let total = libs.len();
        let mut items: Vec<AssetItem> = Vec::with_capacity(total);
        let mut pending: Vec<DownloadItem> = Vec::new();

        for (idx, lib) in libs.iter().enumerate() {
            let _ = event_tx
                .send(LaunchEvent::Check {
                    current: idx + 1,
                    total,
                    kind: "libraries".into(),
                })
                .await;

            // Skip libraries with OS rules — same as the JS loader.
            if lib.rules.is_some() {
                continue;
            }

            let lib_info = match get_path_libraries(&lib.name, None, None) {
                Ok(i) => i,
                Err(_) => continue,
            };

            let loader_name = match self.variant {
                FabricVariant::Modern => "fabric",
                FabricVariant::Legacy => "legacyfabric",
            };
            let folder = options
                .loader_dir(loader_name)
                .join("libraries")
                .join(&lib_info.path);
            let dest = folder.join(&lib_info.name);

            let url = resolve_lib_url(lib, &lib_info.path, &lib_info.name);

            items.push(AssetItem::Asset {
                path: dest.to_string_lossy().into_owned(),
                sha1: lib
                    .downloads
                    .as_ref()
                    .and_then(|d| d.artifact.as_ref())
                    .and_then(|a| a.sha1.clone())
                    .unwrap_or_default(),
                size: lib
                    .downloads
                    .as_ref()
                    .and_then(|d| d.artifact.as_ref())
                    .and_then(|a| a.size)
                    .unwrap_or(0),
                url: url.clone(),
            });

            if !dest.exists() {
                pending.push(DownloadItem {
                    url,
                    path: dest,
                    folder,
                    name: lib_info.name,
                    size: 0,
                    r#type: Some("libraries".into()),
                    sha1: None,
                });
            }
        }

        if !pending.is_empty() {
            let downloader = Downloader::new(
                options.timeout_secs,
                options.clamped_concurrency(),
                options.force_ipv4,
            );
            downloader
                .download_multiple(pending, event_tx.clone())
                .await
                .map_err(|e| {
                    LoaderError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })?;
        }

        Ok(items)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn resolve_lib_url(
    lib: &crate::models::loader::LoaderLibrary,
    rel_path: &str,
    name: &str,
) -> String {
    // Prefer explicit download URL from the downloads section.
    if let Some(url) = lib
        .downloads
        .as_ref()
        .and_then(|d| d.artifact.as_ref())
        .map(|a| a.url.as_str())
    {
        return url.to_owned();
    }

    // Build URL from the base Maven repo URL.
    let base = lib
        .url
        .as_deref()
        .unwrap_or("https://repo1.maven.org/maven2/");
    let base = base.trim_end_matches('/');
    format!("{base}/{rel_path}/{name}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_variant_modern_and_legacy_differ() {
        assert_ne!(FabricVariant::Modern, FabricVariant::Legacy);
    }

    #[test]
    fn resolve_lib_url_uses_downloads_url_when_present() {
        use crate::models::loader::{LoaderArtifact, LoaderLibraryDownloads};
        let lib = crate::models::loader::LoaderLibrary {
            name: "a:b:1.0".into(),
            url: Some("https://repo.example.com/".into()),
            downloads: Some(LoaderLibraryDownloads {
                artifact: Some(LoaderArtifact {
                    sha1: None,
                    size: None,
                    path: None,
                    url: "https://direct.example.com/b-1.0.jar".into(),
                }),
            }),
            rules: None,
            clientreq: None,
        };
        let url = resolve_lib_url(&lib, "a/b/1.0", "b-1.0.jar");
        assert_eq!(url, "https://direct.example.com/b-1.0.jar");
    }

    #[test]
    fn resolve_lib_url_constructs_from_base_url() {
        let lib = crate::models::loader::LoaderLibrary {
            name: "a:b:1.0".into(),
            url: Some("https://maven.fabricmc.net/".into()),
            downloads: None,
            rules: None,
            clientreq: None,
        };
        let url = resolve_lib_url(&lib, "a/b/1.0", "b-1.0.jar");
        assert_eq!(url, "https://maven.fabricmc.net/a/b/1.0/b-1.0.jar");
    }

    #[test]
    fn resolve_lib_url_falls_back_to_maven_central() {
        let lib = crate::models::loader::LoaderLibrary {
            name: "a:b:1.0".into(),
            url: None,
            downloads: None,
            rules: None,
            clientreq: None,
        };
        let url = resolve_lib_url(&lib, "a/b/1.0", "b-1.0.jar");
        assert!(url.contains("repo1.maven.org"));
        assert!(url.contains("b-1.0.jar"));
    }
}
