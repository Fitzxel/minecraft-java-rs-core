use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::models::loader::{FabricJson, QuiltMeta};
use crate::models::minecraft::AssetItem;
use crate::net::downloader::{DownloadItem, Downloader};
use crate::utils::paths::get_path_libraries;

use super::fabric::resolve_lib_url;

const QUILT_META: &str = "https://meta.quiltmc.org/v3/versions";
const QUILT_PROFILE: &str =
    "https://meta.quiltmc.org/v3/versions/loader/${version}/${build}/profile/json";

// ── Public API ────────────────────────────────────────────────────────────────

pub struct QuiltMC;

impl QuiltMC {
    pub fn new() -> Self {
        Self
    }

    /// Fetch the Quilt loader profile JSON for the given Minecraft version.
    ///
    /// `build` options:
    /// - `"latest"` → first build in the list (highest version).
    /// - `"recommended"` → first build whose version does not contain `"beta"`.
    /// - Any other string → exact version match.
    pub async fn download_json(
        &self,
        mc_version: &str,
        build: &str,
        client: &reqwest::Client,
    ) -> Result<FabricJson, LoaderError> {
        let meta: QuiltMeta = client
            .get(QUILT_META)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if !meta.game.iter().any(|g| g.version == mc_version) {
            return Err(LoaderError::VersionNotFound(format!(
                "QuiltMC doesn't support Minecraft {mc_version}"
            )));
        }

        let build_ver = match build {
            "latest" => meta
                .loader
                .first()
                .map(|b| b.version.clone())
                .ok_or_else(|| LoaderError::VersionNotFound("No Quilt builds available".into()))?,
            "recommended" => meta
                .loader
                .iter()
                .find(|b| !b.version.contains("beta"))
                .map(|b| b.version.clone())
                .ok_or_else(|| {
                    LoaderError::VersionNotFound("No stable Quilt build found".into())
                })?,
            ver => meta
                .loader
                .iter()
                .find(|b| b.version == ver)
                .map(|b| b.version.clone())
                .ok_or_else(|| {
                    let available: Vec<_> = meta.loader.iter().map(|b| b.version.as_str()).collect();
                    LoaderError::VersionNotFound(format!(
                        "Quilt build {ver} not found. Available: {}",
                        available.join(", ")
                    ))
                })?,
        };

        let profile_url = QUILT_PROFILE
            .replace("${version}", mc_version)
            .replace("${build}", &build_ver);

        let json: FabricJson = client
            .get(&profile_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(|e| LoaderError::ApiError(e.to_string()))?;

        Ok(json)
    }

    /// Download any Quilt libraries not yet on disk.
    ///
    /// Behaviour is identical to `FabricMC::download_libraries`.
    pub async fn download_libraries(
        &self,
        options: &LaunchOptions,
        quilt_json: &FabricJson,
        _client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<Vec<AssetItem>, LoaderError> {
        let libs = &quilt_json.libraries;
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

            if lib.rules.is_some() {
                continue;
            }

            let lib_info = match get_path_libraries(&lib.name, None, None) {
                Ok(i) => i,
                Err(_) => continue,
            };

            let folder = options
                .loader_dir("quilt")
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
            let downloader = Downloader::new(options.timeout_secs, options.download_concurrency);
            downloader
                .download_multiple(pending, event_tx.clone())
                .await
                .map_err(|e| {
                    LoaderError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
                })?;
        }

        Ok(items)
    }
}

impl Default for QuiltMC {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quilt_mc_constructs_without_options() {
        let _q = QuiltMC::new();
    }

    #[test]
    fn quilt_mc_default_same_as_new() {
        let _q = QuiltMC::default();
    }
}
