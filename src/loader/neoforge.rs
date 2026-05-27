use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::loader::forge::try_patcher_install;
use crate::models::loader::{ForgeVersionSection, InstallerInfo, LoaderLibrary, LoaderType};
use crate::models::minecraft::AssetItem;
use crate::net::downloader::{DownloadItem, Downloader};
use crate::utils::archive::{get_file_from_archive, ArchiveQueryResult};
use crate::utils::paths::get_path_libraries;

// ── Constants ─────────────────────────────────────────────────────────────────

const LEGACY_META_URL: &str =
    "https://maven.neoforged.net/releases/net/neoforged/forge/maven-metadata.xml";
const NEW_META_URL: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";

const LEGACY_MAVEN: &str =
    "https://maven.neoforged.net/releases/net/neoforged/forge";
const NEW_MAVEN: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge";

// ── XML maven-metadata.xml parser ─────────────────────────────────────────────

fn parse_maven_xml_versions(xml: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        rest = &rest[start + 9..];
        if let Some(end) = rest.find("</version>") {
            versions.push(rest[..end].trim().to_owned());
            rest = &rest[end + 10..];
        } else {
            break;
        }
    }
    versions
}

// ── Public API ────────────────────────────────────────────────────────────────

pub struct NeoForgeMC;

impl NeoForgeMC {
    pub fn new() -> Self {
        Self
    }

    /// Install NeoForge by running the installer JAR with `--installClient`.
    pub async fn install(
        &self,
        options: &LaunchOptions,
        mc_version: &str,
        java_path: &str,
        mc_jar: &str,
        mc_json: &str,
        build: &str,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<(String, Option<String>, Vec<AssetItem>, Vec<String>, Vec<String>), LoaderError> {
        let loader_base = options.loader_dir("neoforge");
        tokio::fs::create_dir_all(&loader_base).await?;

        let installer = self
            .download_installer(options, mc_version, build, client, event_tx)
            .await?;

        let version_id = read_installer_version_id(&installer.file_path).await?;
        let version_json_path = loader_base
            .join("versions")
            .join(&version_id)
            .join(format!("{version_id}.json"));

        if !version_json_path.exists() {
            let used_patcher = try_patcher_install(
                &installer.file_path,
                &loader_base,
                &version_json_path,
                mc_jar,
                mc_json,
                java_path,
                &options.path,
                options,
                LoaderType::NeoForge,
                installer.old_api,
                event_tx,
            )
            .await;

            if !used_patcher {
                prepare_install_dir(&loader_base, mc_version, mc_jar, mc_json).await?;
                run_installer(java_path, &installer.file_path, &loader_base, event_tx).await?;
            }

            if !version_json_path.exists() {
                return Err(LoaderError::ApiError(format!(
                    "NeoForge installer finished but no version JSON was created at {}",
                    version_json_path.display()
                )));
            }
        }

        let version_json = read_version_json(&version_json_path).await?;
        let libraries = build_library_assets(&loader_base, &version_json);
        let extra_game_args = extract_game_args(&version_json);
        let extra_jvm_args = extract_jvm_args(&loader_base, &version_id, &version_json);
        let main_class = version_json.main_class;

        Ok((version_id, main_class, libraries, extra_game_args, extra_jvm_args))
    }

    /// Download the NeoForge installer JAR.
    pub async fn download_installer(
        &self,
        options: &LaunchOptions,
        mc_version: &str,
        build: &str,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<InstallerInfo, LoaderError> {
        // Try legacy API first.
        let legacy = client.get(LEGACY_META_URL).send().await.ok();
        let (legacy_versions, _old_api) = if let Some(r) = legacy.filter(|r| r.status().is_success()) {
            let text = r.text().await.unwrap_or_default();
            let prefix = format!("{mc_version}-");
            let filtered: Vec<String> = parse_maven_xml_versions(&text)
                .into_iter()
                .filter(|v| v.starts_with(&prefix))
                .collect();
            (filtered, true)
        } else {
            (Vec::new(), true)
        };

        let (versions, old_api) = if legacy_versions.is_empty() {
            let text = client
                .get(NEW_META_URL)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let short_prefix = make_short_prefix(mc_version);
            let filtered: Vec<String> = parse_maven_xml_versions(&text)
                .into_iter()
                .filter(|v| v.starts_with(&short_prefix))
                .collect();
            if filtered.is_empty() {
                return Err(LoaderError::VersionNotFound(format!(
                    "NeoForge doesn't support Minecraft {mc_version}"
                )));
            }
            (filtered, false)
        } else {
            (legacy_versions, true)
        };

        let chosen = resolve_neo_build(build, &versions)?;
        let (maven_base, artifact_prefix) = if old_api {
            (LEGACY_MAVEN, "forge")
        } else {
            (NEW_MAVEN, "neoforge")
        };

        let installer_name = format!("{artifact_prefix}-{chosen}-installer.jar");
        let installer_folder = options.loader_dir("neoforge").join("installer");
        let installer_path = installer_folder.join(&installer_name);

        if !installer_path.exists() {
            let url = format!("{maven_base}/{chosen}/{installer_name}");
            let item = DownloadItem {
                url,
                path: installer_path.clone(),
                folder: installer_folder.clone(),
                name: installer_name.clone(),
                size: 0,
                r#type: Some("neoforge".into()),
                sha1: None,
            };
            let downloader = Downloader::new(options.timeout_secs, 1);
            downloader
                .download_multiple(vec![item], event_tx.clone())
                .await
                .map_err(|e| {
                    LoaderError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
                })?;
        }

        Ok(InstallerInfo {
            file_path: installer_path.to_string_lossy().into_owned(),
            meta_data: chosen.clone(),
            ext: "jar".into(),
            id: format!("neoforge-{chosen}"),
            old_api,
        })
    }
}

impl Default for NeoForgeMC {
    fn default() -> Self {
        Self::new()
    }
}

// ── Installer driver (identical to Forge) ─────────────────────────────────────

async fn read_installer_version_id(installer_path: &str) -> Result<String, LoaderError> {
    let result = get_file_from_archive(
        PathBuf::from(installer_path),
        Some("install_profile.json".into()),
        None,
        false,
    )
    .await
    .map_err(|e| LoaderError::Archive(e.to_string()))?;

    let bytes = match result {
        ArchiveQueryResult::FileData(b) => b,
        _ => return Err(LoaderError::ProfileNotFound),
    };

    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;

    if let Some(v) = raw.get("version").and_then(|v| v.as_str()) {
        return Ok(v.to_owned());
    }
    if let Some(v) = raw
        .get("install")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
    {
        return Ok(v.to_owned());
    }
    if let Some(v) = raw
        .get("versionInfo")
        .and_then(|i| i.get("id"))
        .and_then(|v| v.as_str())
    {
        return Ok(v.to_owned());
    }

    Err(LoaderError::ApiError(
        "Could not determine version ID from install_profile.json".into(),
    ))
}

async fn prepare_install_dir(
    loader_base: &Path,
    mc_version: &str,
    mc_jar: &str,
    mc_json: &str,
) -> Result<(), LoaderError> {
    let profiles_path = loader_base.join("launcher_profiles.json");
    if !profiles_path.exists() {
        tokio::fs::write(&profiles_path, b"{\"profiles\":{}}\n").await?;
    }

    let dest_dir = loader_base.join("versions").join(mc_version);
    tokio::fs::create_dir_all(&dest_dir).await?;

    let dest_jar = dest_dir.join(format!("{mc_version}.jar"));
    if !dest_jar.exists() {
        tokio::fs::copy(mc_jar, &dest_jar).await?;
    }
    let dest_json = dest_dir.join(format!("{mc_version}.json"));
    if !dest_json.exists() {
        tokio::fs::copy(mc_json, &dest_json).await?;
    }

    Ok(())
}

async fn run_installer(
    java_path: &str,
    installer_path: &str,
    loader_base: &Path,
    event_tx: &Sender<LaunchEvent>,
) -> Result<(), LoaderError> {
    let _ = event_tx
        .send(LaunchEvent::Patch(format!(
            "Running NeoForge installer: {}",
            installer_path
        )))
        .await;

    let mut child = tokio::process::Command::new(java_path)
        .arg("-jar")
        .arg(installer_path)
        .arg("--installClient")
        .arg(loader_base.as_os_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(LoaderError::Io)?;

    if let Some(stdout) = child.stdout.take() {
        let tx = event_tx.clone();
        let mut lines = BufReader::new(stdout).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(LaunchEvent::Patch(line)).await;
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = event_tx.clone();
        let mut lines = BufReader::new(stderr).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(LaunchEvent::Patch(line)).await;
            }
        });
    }

    let status = child.wait().await.map_err(LoaderError::Io)?;
    if !status.success() {
        let _ = event_tx
            .send(LaunchEvent::Patch(format!(
                "NeoForge installer exited with code {:?} (checking for version JSON)",
                status.code()
            )))
            .await;
    }
    Ok(())
}

async fn read_version_json(path: &Path) -> Result<ForgeVersionSection, LoaderError> {
    let bytes = tokio::fs::read(path).await?;
    let version: ForgeVersionSection = serde_json::from_slice(&bytes)?;
    Ok(version)
}

fn extract_game_args(version: &ForgeVersionSection) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(mc_args) = &version.minecraft_arguments {
        for token in mc_args.split_whitespace() {
            args.push(token.to_owned());
        }
    }
    if let Some(forge_args) = &version.arguments {
        for entry in &forge_args.game {
            if let Some(s) = entry.as_str() {
                args.push(s.to_owned());
            }
        }
    }
    args
}

fn extract_jvm_args(loader_base: &Path, version_id: &str, version: &ForgeVersionSection) -> Vec<String> {
    let lib_dir = loader_base.join("libraries").to_string_lossy().into_owned();
    let sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    let mut args = Vec::new();
    if let Some(forge_args) = &version.arguments {
        for entry in &forge_args.jvm {
            if let Some(s) = entry.as_str() {
                args.push(
                    s.replace("${library_directory}", &lib_dir)
                     .replace("${classpath_separator}", sep)
                     .replace("${version_name}", version_id),
                );
            }
        }
    }
    args
}

fn build_library_assets(loader_base: &Path, version: &ForgeVersionSection) -> Vec<AssetItem> {
    let libs = version.libraries.as_deref().unwrap_or(&[]);
    let mut items: Vec<AssetItem> = Vec::with_capacity(libs.len());

    for lib in libs {
        if lib.rules.is_some() {
            continue;
        }
        let (path, sha1, size, url) = resolve_library_entry(loader_base, lib);
        items.push(AssetItem::Asset { path, sha1, size, url });
    }

    items
}

fn resolve_library_entry(
    loader_base: &Path,
    lib: &LoaderLibrary,
) -> (String, String, u64, String) {
    let libs_dir = loader_base.join("libraries");

    let artifact = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref());

    let rel_path = artifact
        .and_then(|a| a.path.clone())
        .or_else(|| {
            get_path_libraries(&lib.name, None, None)
                .ok()
                .map(|info| format!("{}/{}", info.path, info.name))
        })
        .unwrap_or_default();

    let abs_path = libs_dir.join(&rel_path);

    let sha1 = artifact.and_then(|a| a.sha1.clone()).unwrap_or_default();
    let size = artifact.and_then(|a| a.size).unwrap_or(0);
    let url = artifact.map(|a| a.url.clone()).unwrap_or_default();

    (abs_path.to_string_lossy().into_owned(), sha1, size, url)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_short_prefix(mc_version: &str) -> String {
    // "1.20.4" → "20.4."
    let parts: Vec<&str> = mc_version.splitn(3, '.').collect();
    let minor = parts.get(1).copied().unwrap_or("0");
    let patch = parts.get(2).copied().unwrap_or("0");
    format!("{minor}.{patch}.")
}

fn resolve_neo_build(build: &str, versions: &[String]) -> Result<String, LoaderError> {
    match build {
        "latest" => versions
            .last()
            .cloned()
            .ok_or_else(|| LoaderError::VersionNotFound("No NeoForge builds available".into())),
        "recommended" => versions
            .iter()
            .rev()
            .find(|v| !v.contains("beta"))
            .cloned()
            .or_else(|| versions.last().cloned())
            .ok_or_else(|| LoaderError::VersionNotFound("No stable NeoForge build found".into())),
        specific => versions
            .iter()
            .find(|v| v.as_str() == specific)
            .cloned()
            .ok_or_else(|| {
                let available = versions.join(", ");
                LoaderError::VersionNotFound(format!(
                    "NeoForge build {specific} not found. Available: {available}"
                ))
            }),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_short_prefix_splits_correctly() {
        assert_eq!(make_short_prefix("1.20.4"), "20.4.");
        assert_eq!(make_short_prefix("1.21.0"), "21.0.");
        assert_eq!(make_short_prefix("1.21"), "21.0.");
    }

    #[test]
    fn resolve_neo_build_latest() {
        let versions = vec!["20.4.1".into(), "20.4.2".into(), "20.4.3-beta".into()];
        let result = resolve_neo_build("latest", &versions).unwrap();
        assert_eq!(result, "20.4.3-beta");
    }

    #[test]
    fn resolve_neo_build_recommended_skips_beta() {
        let versions = vec!["20.4.1".into(), "20.4.2".into(), "20.4.3-beta".into()];
        let result = resolve_neo_build("recommended", &versions).unwrap();
        assert_eq!(result, "20.4.2");
    }

    #[test]
    fn resolve_neo_build_specific() {
        let versions = vec!["20.4.1".into(), "20.4.2".into()];
        let result = resolve_neo_build("20.4.1", &versions).unwrap();
        assert_eq!(result, "20.4.1");
    }

    #[test]
    fn resolve_neo_build_specific_not_found() {
        let versions = vec!["20.4.1".into()];
        assert!(resolve_neo_build("99.9.9", &versions).is_err());
    }

    #[test]
    fn parse_maven_xml_versions_extracts_all() {
        let xml = "<metadata><versioning><versions>\
                   <version>1.0</version><version>1.1</version>\
                   </versions></versioning></metadata>";
        let v = parse_maven_xml_versions(xml);
        assert_eq!(v, vec!["1.0", "1.1"]);
    }

    #[test]
    fn neo_forge_mc_constructs() {
        let _n = NeoForgeMC::new();
    }
}
