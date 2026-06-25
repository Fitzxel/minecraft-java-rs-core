use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::loader::forge_patcher::{ForgePatcher, PatchConfig};
use crate::models::loader::{
    ForgeProfile, ForgeVersionSection, InstallerInfo, LoaderLibrary, LoaderType,
};
use crate::models::minecraft::AssetItem;
use crate::net::downloader::{DownloadItem, Downloader};
use crate::utils::archive::{get_file_from_archive, ArchiveQueryResult};
use crate::utils::paths::get_path_libraries;

// ── Constants ─────────────────────────────────────────────────────────────────

const META_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json";
const PROMOTIONS_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const MAVEN_BASE: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";

static FALLBACK_META: &[u8] = include_bytes!("../../assets/forge/maven-metadata.json");

#[derive(Deserialize)]
struct Promotions {
    promos: HashMap<String, String>,
}

// ── Public API ────────────────────────────────────────────────────────────────

pub struct ForgeMC;

impl ForgeMC {
    pub fn new() -> Self {
        Self
    }

    /// Install Forge by downloading and running the installer JAR in headless
    /// (`--installClient`) mode. The installer writes libraries to
    /// `<loader_base>/libraries/` and the version JSON to
    /// `<loader_base>/versions/<id>/<id>.json`.
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
    ) -> Result<
        (
            String,
            Option<String>,
            Vec<AssetItem>,
            Vec<String>,
            Vec<String>,
        ),
        LoaderError,
    > {
        let loader_base = options.loader_dir("forge");
        tokio::fs::create_dir_all(&loader_base).await?;

        // 1. Download the installer JAR.
        let installer = self
            .download_installer(options, mc_version, build, client, event_tx)
            .await?;

        // 2. Determine the version ID the installer will create.
        let version_id = read_installer_version_id(&installer.file_path).await?;
        let version_json_path = loader_base
            .join("versions")
            .join(&version_id)
            .join(format!("{version_id}.json"));

        // 3. Install: try the manual patcher first; fall back to --installClient.
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
                LoaderType::Forge,
                false, // neo_forge_old: not applicable for Forge
                event_tx,
            )
            .await;

            if !used_patcher {
                prepare_install_dir(&loader_base, mc_version, mc_jar, mc_json).await?;
                run_installer(java_path, &installer.file_path, &loader_base, event_tx).await?;
            }

            if !version_json_path.exists() {
                return Err(LoaderError::ApiError(format!(
                    "Forge install finished but no version JSON found at {}",
                    version_json_path.display()
                )));
            }
        }

        // 4. Parse the version JSON and build the loader result.
        let version_json = read_version_json(&version_json_path).await?;
        let libraries = build_library_assets(&loader_base, &version_json);
        let extra_game_args = extract_game_args(&version_json);
        let extra_jvm_args = extract_jvm_args(&loader_base, &version_id, &version_json);
        let main_class = version_json.main_class;

        Ok((
            version_id,
            main_class,
            libraries,
            extra_game_args,
            extra_jvm_args,
        ))
    }

    /// Download the Forge installer JAR for the given Minecraft version and build.
    pub async fn download_installer(
        &self,
        options: &LaunchOptions,
        mc_version: &str,
        build: &str,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<InstallerInfo, LoaderError> {
        let all_versions: HashMap<String, Vec<String>> = match client.get(META_URL).send().await {
            Ok(r) if r.status().is_success() => r.json().await?,
            _ => serde_json::from_slice(FALLBACK_META)?,
        };

        let versions = all_versions.get(mc_version).ok_or_else(|| {
            LoaderError::VersionNotFound(format!("Forge doesn't support Minecraft {mc_version}"))
        })?;

        let forge_build = resolve_forge_build(build, mc_version, versions, client).await?;

        if !versions.iter().any(|v| v == &forge_build) {
            let available = versions.join(", ");
            return Err(LoaderError::VersionNotFound(format!(
                "Forge build {forge_build} not found for {mc_version}. Available: {available}"
            )));
        }

        let installer_name = format!("forge-{forge_build}-installer.jar");
        let installer_folder = options.loader_dir("forge").join("installer");
        let installer_path = installer_folder.join(&installer_name);

        if !installer_path.exists() {
            let url = format!("{MAVEN_BASE}/{forge_build}/{installer_name}");
            let item = DownloadItem {
                url: url.clone(),
                path: installer_path.clone(),
                folder: installer_folder.clone(),
                name: installer_name.clone(),
                size: 0,
                r#type: Some("forge".into()),
                sha1: None,
            };
            let downloader =
                Downloader::new(options.timeout_secs, 1, options.force_ipv4, options.dns);
            downloader
                .download_multiple(vec![item], event_tx.clone())
                .await
                .map_err(|e| {
                    LoaderError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })?;
        }

        Ok(InstallerInfo {
            file_path: installer_path.to_string_lossy().into_owned(),
            meta_data: forge_build.clone(),
            ext: "jar".into(),
            id: format!("forge-{forge_build}"),
            old_api: false,
        })
    }
}

impl Default for ForgeMC {
    fn default() -> Self {
        Self::new()
    }
}

// ── Installer driver ──────────────────────────────────────────────────────────

/// Read the `version` field from `install_profile.json` inside the installer.
///
/// This is the directory name the installer will create under `versions/`.
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

    // New format (1.13+): top-level `version` string.
    if let Some(v) = raw.get("version").and_then(|v| v.as_str()) {
        return Ok(v.to_owned());
    }
    // Old format: install.version or versionInfo.id.
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

/// The Forge installer refuses to run unless `launcher_profiles.json` exists
/// and the base Minecraft version is present under `versions/<mc>/`.
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

/// Spawn `java -jar <installer> --installClient <loader_base>` and stream
/// its stdout/stderr as `LaunchEvent::Patch` events.
async fn run_installer(
    java_path: &str,
    installer_path: &str,
    loader_base: &Path,
    event_tx: &Sender<LaunchEvent>,
) -> Result<(), LoaderError> {
    let _ = event_tx
        .send(LaunchEvent::Patch(format!(
            "Running Forge installer: {}",
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
        // Known issue: Forge jarsplitter produces non-deterministic checksums under
        // Java 17.0.10+, causing exit code 1 even though the version JSON was written
        // successfully. Treat as a warning; the caller checks for the version JSON.
        let _ = event_tx
            .send(LaunchEvent::Patch(format!(
                "Forge installer exited with code {:?} (checking for version JSON)",
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

/// Extract plain-string game args from the Forge version JSON.
/// Handles both legacy `minecraftArguments` (string) and modern `arguments.game` (array).
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

/// Extract and resolve JVM args from the Forge version JSON (`arguments.jvm`).
///
/// `${library_directory}` is resolved to `loader_base/libraries` (the Forge-local
/// library path, not the vanilla MC path). `${classpath_separator}` and
/// `${version_name}` are also substituted so the strings are ready to use as-is.
fn extract_jvm_args(
    loader_base: &Path,
    version_id: &str,
    version: &ForgeVersionSection,
) -> Vec<String> {
    let lib_dir = loader_base.join("libraries").to_string_lossy().into_owned();
    let sep = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };
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

/// Build classpath entries for the loader libraries the installer wrote.
fn build_library_assets(loader_base: &Path, version: &ForgeVersionSection) -> Vec<AssetItem> {
    let libs = version.libraries.as_deref().unwrap_or(&[]);
    let mut items: Vec<AssetItem> = Vec::with_capacity(libs.len());

    for lib in libs {
        if lib.rules.is_some() {
            continue;
        }
        // Old-format Forge marks server-only libraries with clientreq: false.
        if lib.clientreq == Some(false) {
            continue;
        }

        let (path, sha1, size, url) = resolve_library_entry(loader_base, lib);
        items.push(AssetItem::Asset {
            path,
            sha1,
            size,
            url,
        });
    }

    items
}

fn resolve_library_entry(loader_base: &Path, lib: &LoaderLibrary) -> (String, String, u64, String) {
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
    // For old-format Forge libs that have no downloads.artifact, construct the
    // URL from the lib's base `url` + relative Maven path, or fall back to the
    // standard Minecraft library repo (used by launchwrapper, asm-all, etc.).
    let url = artifact
        .map(|a| a.url.clone())
        .filter(|u| !u.is_empty())
        .or_else(|| {
            lib.url
                .as_ref()
                .filter(|u| !u.is_empty())
                .map(|base| format!("{}/{}", base.trim_end_matches('/'), &rel_path))
        })
        .or_else(|| {
            if !rel_path.is_empty() {
                Some(format!("https://libraries.minecraft.net/{rel_path}"))
            } else {
                None
            }
        })
        .unwrap_or_default();

    (abs_path.to_string_lossy().into_owned(), sha1, size, url)
}

// ── Build resolution helpers ──────────────────────────────────────────────────

/// Older Forge builds append `-{mc_version}` to the build number in the Maven
/// artifact ID (e.g. `1.8.9-11.15.1.2318-1.8.9`), while the promotions API
/// returns only the bare build number. Try the plain candidate first; if it
/// isn't in the versions list, try the suffixed form.
fn match_promo_in_versions(candidate: &str, mc_version: &str, versions: &[String]) -> String {
    if versions.iter().any(|v| v == candidate) {
        return candidate.to_owned();
    }
    let with_suffix = format!("{candidate}-{mc_version}");
    if versions.iter().any(|v| v == &with_suffix) {
        return with_suffix;
    }
    candidate.to_owned()
}

async fn resolve_forge_build(
    build: &str,
    mc_version: &str,
    versions: &[String],
    client: &reqwest::Client,
) -> Result<String, LoaderError> {
    match build {
        "latest" => {
            if let Ok(promos) = client.get(PROMOTIONS_URL).send().await {
                if let Ok(p) = promos.json::<Promotions>().await {
                    let key = format!("{mc_version}-latest");
                    if let Some(ver) = p.promos.get(&key) {
                        let candidate = format!("{mc_version}-{ver}");
                        return Ok(match_promo_in_versions(&candidate, mc_version, versions));
                    }
                }
            }
            versions.last().cloned().ok_or_else(|| {
                LoaderError::VersionNotFound(format!("No Forge builds for {mc_version}"))
            })
        }
        "recommended" => {
            if let Ok(promos) = client.get(PROMOTIONS_URL).send().await {
                if let Ok(p) = promos.json::<Promotions>().await {
                    let rec_key = format!("{mc_version}-recommended");
                    let lat_key = format!("{mc_version}-latest");
                    let ver = p.promos.get(&rec_key).or_else(|| p.promos.get(&lat_key));
                    if let Some(v) = ver {
                        let candidate = format!("{mc_version}-{v}");
                        return Ok(match_promo_in_versions(&candidate, mc_version, versions));
                    }
                }
            }
            versions.last().cloned().ok_or_else(|| {
                LoaderError::VersionNotFound(format!("No Forge builds for {mc_version}"))
            })
        }
        specific => Ok(specific.to_owned()),
    }
}

// ── Intermediate-path patcher ─────────────────────────────────────────────────

/// Try to patch Forge manually instead of running `--installClient`.
///
/// Returns `true` if patching succeeded (version JSON written, processors ran).
/// Returns `false` if patching cannot proceed (no processors, old format) or
/// if any step failed — caller should fall back to `--installClient`.
pub(crate) async fn try_patcher_install(
    installer_path: &str,
    loader_base: &Path,
    version_json_path: &Path,
    mc_jar: &str,
    mc_json: &str,
    java_path: &str,
    game_path: &Path,
    options: &LaunchOptions,
    loader_type: LoaderType,
    neo_forge_old: bool,
    event_tx: &Sender<LaunchEvent>,
) -> bool {
    match try_patcher_install_inner(
        installer_path,
        loader_base,
        version_json_path,
        mc_jar,
        mc_json,
        java_path,
        game_path,
        options,
        loader_type,
        neo_forge_old,
        event_tx,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            let _ = event_tx
                .send(LaunchEvent::Patch(format!(
                    "[patcher] Manual patch failed ({e}); falling back to --installClient"
                )))
                .await;
            false
        }
    }
}

async fn try_patcher_install_inner(
    installer_path: &str,
    loader_base: &Path,
    version_json_path: &Path,
    mc_jar: &str,
    mc_json: &str,
    java_path: &str,
    game_path: &Path,
    options: &LaunchOptions,
    loader_type: LoaderType,
    neo_forge_old: bool,
    event_tx: &Sender<LaunchEvent>,
) -> Result<bool, LoaderError> {
    // 1. Deserialize install_profile.json from the installer JAR.
    let profile = read_install_profile(installer_path).await?;

    // 2. New-format profiles have processors; old-format ones don't.
    let has_processors = profile.processors.as_ref().map_or(false, |p| !p.is_empty());
    if !has_processors {
        // Old-format (pre-1.13): versionInfo is inline in install_profile.json.
        if profile.version_info.is_some() {
            install_old_forge_legacy(
                installer_path,
                loader_base,
                version_json_path,
                &profile,
                event_tx,
            )
            .await?;
            return Ok(true);
        }
        return Ok(false);
    }

    // 3. Extract version.json from the installer JAR.
    if !version_json_path.exists() {
        extract_version_json(installer_path, version_json_path).await?;
    }

    // 4. Extract bundled processor JARs from the installer's maven/ tree.
    let libs_dir = loader_base.join("libraries");
    extract_maven_entries(installer_path, &libs_dir).await?;

    // 5. Download any processor JARs that weren't bundled in the installer.
    download_profile_libraries(&profile, &libs_dir, options, event_tx).await?;

    // 6. Extract data files embedded in the installer (BINPATCH, etc.).
    extract_data_files(
        installer_path,
        &profile,
        &libs_dir,
        &loader_type,
        neo_forge_old,
    )
    .await?;

    // 7. Skip patching if all processor outputs already exist on disk.
    let patcher = ForgePatcher::new(loader_base.to_path_buf(), loader_type);
    if patcher.check(&profile) {
        let _ = event_tx
            .send(LaunchEvent::Patch(
                "[patcher] Already patched, skipping".into(),
            ))
            .await;
        return Ok(true);
    }

    // 8. Run processors sequentially.
    let config = PatchConfig {
        java_path,
        minecraft_jar: mc_jar,
        minecraft_json: mc_json,
        game_path,
    };
    patcher
        .patch(&profile, &config, neo_forge_old, event_tx)
        .await?;
    Ok(true)
}

/// Deserialize `install_profile.json` from inside the installer JAR.
///
/// New-format profiles (1.13+) have a top-level `"version": "<id>"` string
/// that conflicts with `ForgeProfile.version: Option<ForgeVersionSection>`.
/// We strip it before deserializing; the version ID is obtained separately
/// via `read_installer_version_id`.
async fn read_install_profile(installer_path: &str) -> Result<ForgeProfile, LoaderError> {
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

    let mut raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(obj) = raw.as_object_mut() {
        if obj.get("version").and_then(|v| v.as_str()).is_some() {
            obj.remove("version");
        }
    }

    let profile: ForgeProfile = serde_json::from_value(raw)?;
    Ok(profile)
}

/// Extract `version.json` from the installer JAR to `dest_path`.
/// Install old-format Forge (pre-1.13): write versionInfo as the version JSON
/// and extract the bundled universal JAR into the loader's Maven libs tree.
/// No processors exist in this format; the universal JAR IS the Forge runtime.
async fn install_old_forge_legacy(
    installer_path: &str,
    loader_base: &Path,
    version_json_path: &Path,
    profile: &ForgeProfile,
    event_tx: &Sender<LaunchEvent>,
) -> Result<(), LoaderError> {
    let version_info = profile.version_info.as_ref().expect("caller checked Some");

    if let Some(parent) = version_json_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_json_path, serde_json::to_vec_pretty(version_info)?).await?;
    let _ = event_tx
        .send(LaunchEvent::Patch(
            "[patcher] Old-format Forge: wrote version JSON".into(),
        ))
        .await;

    if let Some(install) = &profile.install {
        if let (Some(file_in_zip), Some(maven_coord)) = (&install.file_path, &install.path) {
            if let Ok(lib_info) = get_path_libraries(maven_coord, None, None) {
                let dest = loader_base
                    .join("libraries")
                    .join(&lib_info.path)
                    .join(&lib_info.name);
                if !dest.exists() {
                    let result = get_file_from_archive(
                        PathBuf::from(installer_path),
                        Some(file_in_zip.clone()),
                        None,
                        false,
                    )
                    .await
                    .map_err(|e| LoaderError::Archive(e.to_string()))?;

                    if let ArchiveQueryResult::FileData(bytes) = result {
                        if let Some(parent) = dest.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::write(&dest, bytes).await?;
                        let _ = event_tx
                            .send(LaunchEvent::Patch(format!(
                                "[patcher] Old-format Forge: extracted {}",
                                lib_info.name
                            )))
                            .await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn extract_version_json(installer_path: &str, dest_path: &Path) -> Result<(), LoaderError> {
    let result = get_file_from_archive(
        PathBuf::from(installer_path),
        Some("version.json".into()),
        None,
        false,
    )
    .await
    .map_err(|e| LoaderError::Archive(e.to_string()))?;

    let bytes = match result {
        ArchiveQueryResult::FileData(b) => b,
        _ => {
            return Err(LoaderError::ApiError(
                "version.json not found in installer JAR".into(),
            ))
        }
    };

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(dest_path, &bytes).await?;
    Ok(())
}

/// Extract all `maven/` entries from the installer JAR into `libs_dir`.
///
/// Forge installers bundle processor JARs under `maven/` using Maven layout.
/// We extract them so the patcher can find them without extra downloads.
async fn extract_maven_entries(installer_path: &str, libs_dir: &Path) -> Result<(), LoaderError> {
    let installer = PathBuf::from(installer_path);

    let names = match get_file_from_archive(installer.clone(), None, Some("maven/".into()), false)
        .await
        .map_err(|e| LoaderError::Archive(e.to_string()))?
    {
        ArchiveQueryResult::Names(n) => n,
        _ => return Ok(()),
    };

    for name in names {
        let rel = match name.strip_prefix("maven/") {
            Some(r) if !r.is_empty() => r.to_owned(),
            _ => continue,
        };

        let dest = libs_dir.join(&rel);
        if dest.exists() {
            continue;
        }

        let bytes = match get_file_from_archive(installer.clone(), Some(name), None, false)
            .await
            .map_err(|e| LoaderError::Archive(e.to_string()))?
        {
            ArchiveQueryResult::FileData(b) => b,
            _ => continue,
        };

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, &bytes).await?;
    }

    Ok(())
}

/// Download processor JARs from `profile.libraries` that aren't already on disk.
///
/// JARs bundled inside the installer (extracted by `extract_maven_entries`) are
/// skipped; only entries with a non-empty URL that are still missing are fetched.
async fn download_profile_libraries(
    profile: &ForgeProfile,
    libs_dir: &Path,
    options: &LaunchOptions,
    event_tx: &Sender<LaunchEvent>,
) -> Result<(), LoaderError> {
    let libs = match profile.libraries.as_deref() {
        Some(l) if !l.is_empty() => l,
        _ => return Ok(()),
    };

    let mut items: Vec<DownloadItem> = Vec::new();

    for lib in libs {
        let artifact = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref());
        let url = match artifact {
            Some(a) if !a.url.is_empty() => a.url.clone(),
            _ => continue,
        };

        let rel_path = artifact
            .and_then(|a| a.path.clone())
            .or_else(|| {
                get_path_libraries(&lib.name, None, None)
                    .ok()
                    .map(|info| format!("{}/{}", info.path, info.name))
            })
            .unwrap_or_default();

        if rel_path.is_empty() {
            continue;
        }

        let dest = libs_dir.join(&rel_path);
        if dest.exists() {
            continue;
        }

        let folder = dest.parent().unwrap_or(libs_dir).to_path_buf();
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        items.push(DownloadItem {
            url,
            path: dest,
            folder,
            name,
            size: artifact.and_then(|a| a.size).unwrap_or(0),
            r#type: Some("forge-lib".into()),
            sha1: artifact.and_then(|a| a.sha1.clone()),
        });
    }

    if !items.is_empty() {
        let downloader = Downloader::new(
            options.timeout_secs,
            options.clamped_concurrency(),
            options.force_ipv4,
            options.dns,
        );
        downloader
            .download_multiple(items, event_tx.clone())
            .await
            .map_err(|e| {
                LoaderError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            })?;
    }

    Ok(())
}

/// Extract data files embedded in the installer JAR (values starting with `/`).
///
/// The key case is `BINPATCH`: its client value (`"/data/client.lzma"`) points
/// to a file inside the installer. We extract it to the Maven path the patcher
/// expects: `libs_dir/<coord-path>/<name>-clientdata.lzma`.
async fn extract_data_files(
    installer_path: &str,
    profile: &ForgeProfile,
    libs_dir: &Path,
    loader_type: &LoaderType,
    neo_forge_old: bool,
) -> Result<(), LoaderError> {
    let data = match &profile.data {
        Some(d) => d,
        None => return Ok(()),
    };

    let universal_name: Option<String> = profile.libraries.as_deref().and_then(|libs| {
        libs.iter()
            .find(|lib| match loader_type {
                LoaderType::Forge => lib.name.starts_with("net.minecraftforge:forge"),
                LoaderType::NeoForge => {
                    if neo_forge_old {
                        lib.name.starts_with("net.neoforged:forge")
                    } else {
                        lib.name.starts_with("net.neoforged:neoforge")
                    }
                }
                _ => false,
            })
            .map(|lib| lib.name.clone())
    });

    for (key, entry) in data {
        let client_val = entry.client.trim();

        // Only extract files that are embedded in the installer (path starts with '/').
        if !client_val.starts_with('/') {
            continue;
        }
        let in_jar_path = &client_val[1..]; // strip leading '/'

        let dest: PathBuf = if key == "BINPATCH" {
            let coord = profile
                .path
                .as_deref()
                .or_else(|| profile.install.as_ref().and_then(|i| i.path.as_deref()))
                .or(universal_name.as_deref())
                .unwrap_or("");

            if coord.is_empty() {
                continue;
            }

            let info = match get_path_libraries(coord, None, None) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let lzma_name = info.name.replace(".jar", "-clientdata.lzma");
            libs_dir.join(&info.path).join(lzma_name)
        } else {
            libs_dir.join(in_jar_path)
        };

        if dest.exists() {
            continue;
        }

        let result = get_file_from_archive(
            PathBuf::from(installer_path),
            Some(in_jar_path.to_owned()),
            None,
            false,
        )
        .await
        .map_err(|e| LoaderError::Archive(e.to_string()))?;

        let bytes = match result {
            ArchiveQueryResult::FileData(b) => b,
            _ => continue,
        };

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, &bytes).await?;
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_metadata_is_valid_json() {
        let parsed: serde_json::Value = serde_json::from_slice(FALLBACK_META).unwrap();
        assert!(parsed.is_object(), "forge metadata should be a JSON object");
    }

    #[test]
    fn fallback_metadata_contains_versions() {
        let parsed: HashMap<String, Vec<String>> = serde_json::from_slice(FALLBACK_META).unwrap();
        assert!(!parsed.is_empty());
    }

    #[test]
    fn forge_mc_constructs() {
        let _f = ForgeMC::new();
    }

    #[test]
    fn build_library_assets_uses_explicit_artifact_path() {
        let version = ForgeVersionSection {
            id: Some("1.20.1-forge-47.4.20".into()),
            libraries: Some(vec![LoaderLibrary {
                name: "cpw.mods:bootstraplauncher:1.1.2".into(),
                url: None,
                downloads: Some(crate::models::loader::LoaderLibraryDownloads {
                    artifact: Some(crate::models::loader::LoaderArtifact {
                        sha1: Some("abc".into()),
                        size: Some(123),
                        path: Some(
                            "cpw/mods/bootstraplauncher/1.1.2/bootstraplauncher-1.1.2.jar".into(),
                        ),
                        url: "https://example.com/x.jar".into(),
                    }),
                }),
                rules: None,
                clientreq: None,
            }]),
            main_class: None,
            minecraft_arguments: None,
            arguments: None,
            extra: HashMap::new(),
        };
        let base = PathBuf::from("/mc/loader/forge");
        let items = build_library_assets(&base, &version);
        assert_eq!(items.len(), 1);
        match &items[0] {
            AssetItem::Asset { path, .. } => {
                assert!(path.ends_with("bootstraplauncher-1.1.2.jar"), "got {path}");
                assert!(path.contains("loader/forge/libraries/cpw/mods"));
            }
            _ => panic!("expected Asset"),
        }
    }

    #[test]
    fn build_library_assets_skips_rule_restricted() {
        let version = ForgeVersionSection {
            id: None,
            libraries: Some(vec![LoaderLibrary {
                name: "x:y:1".into(),
                url: None,
                downloads: None,
                rules: Some(vec![serde_json::json!({"action":"disallow"})]),
                clientreq: None,
            }]),
            main_class: None,
            minecraft_arguments: None,
            arguments: None,
            extra: HashMap::new(),
        };
        let items = build_library_assets(Path::new("/mc/loader/forge"), &version);
        assert!(items.is_empty());
    }
}
