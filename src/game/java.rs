use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::Sender;

use crate::error::LaunchError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::models::java::{AdoptiumRelease, JavaFileItem, JavaManifestData, JavaVersionManifest};
use crate::models::minecraft::MinecraftVersionJson;
use crate::net::downloader::{DownloadItem, Downloader};
use crate::net::http::{fetch_json, fetch_text};
use crate::utils::archive::extract_tar_gz;

const ALL_JSON_URL: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const ADOPTIUM_API_BASE: &str = "https://api.adoptium.net/v3/assets/latest";

// ── Public types ──────────────────────────────────────────────────────────────

pub struct JavaDownloadResult {
    /// Absolute path to the `java` (or `javaw.exe`) executable.
    pub java_path: String,
    /// Flat list of downloaded runtime files for `JavaInfo` / bundle checks.
    pub files: Vec<JavaFileItem>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Resolve and (if needed) download the Java runtime for a Minecraft version.
///
/// Priority:
/// 1. `options.java.path` set → use verbatim.
/// 2. Binary already cached at the computed runtime path → return fast.
/// 3. Mojang all.json has an entry for this platform/component → Mojang path.
/// 4. Adoptium API fallback.
pub async fn get_java_files(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
    client: &reqwest::Client,
    event_tx: &Sender<LaunchEvent>,
) -> Result<JavaDownloadResult, LaunchError> {
    if let Some(java_path) = &options.java.path {
        return Ok(JavaDownloadResult {
            java_path: java_path.to_string_lossy().into_owned(),
            files: vec![],
        });
    }

    let (component, major_version) = java_component(options, version_json);
    let platform = mojang_platform_key(options.intel_enabled_mac);
    let runtime_root = options
        .path
        .join("runtime")
        .join(&component)
        .join(&platform);

    let java_bin = find_cached_java_bin(&runtime_root);

    if java_bin.exists() {
        return Ok(JavaDownloadResult {
            java_path: java_bin.to_string_lossy().into_owned(),
            files: vec![],
        });
    }

    if let Some(result) = try_mojang(
        options,
        &component,
        &platform,
        &runtime_root,
        client,
        event_tx,
    )
    .await?
    {
        return Ok(result);
    }

    get_from_adoptium(
        options,
        &component,
        &runtime_root,
        major_version,
        client,
        event_tx,
    )
    .await
}

// ── Platform helpers ──────────────────────────────────────────────────────────

pub fn mojang_platform_key(intel_enabled_mac: bool) -> String {
    use std::env::consts::{ARCH, OS};
    match (OS, ARCH) {
        ("linux", "x86_64") => "linux",
        ("linux", "x86") => "linux-i386",
        ("macos", "x86_64") => "mac-os",
        ("macos", "aarch64") if intel_enabled_mac => "mac-os",
        ("macos", "aarch64") => "mac-os-arm64",
        ("windows", "x86_64") => "windows-x64",
        ("windows", "x86") => "windows-x86",
        ("windows", "aarch64") => "windows-arm64",
        _ => "linux",
    }
    .to_string()
}

pub fn java_component(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
) -> (String, u32) {
    if let Some(ver) = &options.java.version {
        let major = ver.parse::<u32>().unwrap_or(8);
        return (format!("jre-{major}"), major);
    }
    match &version_json.java_version {
        Some(jv) => {
            let major = jv.major_version.unwrap_or(8);
            (format!("jre-{major}"), major)
        }
        None => ("jre-8".into(), 8),
    }
}

pub fn java_bin_path(runtime_root: &Path) -> PathBuf {
    let bin = if cfg!(target_os = "windows") {
        "javaw.exe"
    } else {
        "java"
    };
    runtime_root.join("bin").join(bin)
}

/// Like `java_bin_path` but also checks the macOS bundle layout used by some
/// Mojang runtimes (e.g. jre-legacy): `jre.bundle/Contents/Home/bin/java`.
/// Returns the first path that exists on disk, or the standard path as a
/// fallback so callers can still attempt the download.
fn find_cached_java_bin(runtime_root: &Path) -> PathBuf {
    let primary = java_bin_path(runtime_root);
    if primary.exists() {
        return primary;
    }
    #[cfg(target_os = "macos")]
    {
        let bundle = runtime_root.join("jre.bundle/Contents/Home/bin/java");
        if bundle.exists() {
            return bundle;
        }
    }
    primary
}

fn adoptium_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    }
}

fn adoptium_arch(intel_enabled_mac: bool) -> &'static str {
    use std::env::consts::{ARCH, OS};
    if intel_enabled_mac && OS == "macos" {
        return "x64";
    }
    match ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "aarch64",
        "arm" => "arm",
        _ => "x64",
    }
}

// ── Mojang download path ──────────────────────────────────────────────────────

async fn try_mojang(
    options: &LaunchOptions,
    component: &str,
    platform: &str,
    runtime_root: &Path,
    client: &reqwest::Client,
    event_tx: &Sender<LaunchEvent>,
) -> Result<Option<JavaDownloadResult>, LaunchError> {
    let all_text = match fetch_text(client, ALL_JSON_URL).await {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };

    let all: HashMap<String, HashMap<String, Vec<JavaVersionManifest>>> =
        serde_json::from_str(&all_text)?;

    let manifest_url = all
        .get(platform)
        .and_then(|p| p.get(component))
        .and_then(|versions| versions.first())
        .and_then(|v| v.manifest.as_ref())
        .map(|m| m.url.clone());

    let manifest_url = match manifest_url {
        Some(url) => url,
        None => return Ok(None),
    };

    let manifest_text = fetch_text(client, &manifest_url)
        .await
        .map_err(LaunchError::InvalidData)?;

    let manifest: JavaManifestData = serde_json::from_str(&manifest_text)?;

    let mut items: Vec<DownloadItem> = Vec::new();
    let mut file_records: Vec<JavaFileItem> = Vec::new();

    for (rel_path, entry) in &manifest.files {
        if entry.file_type != "file" {
            continue;
        }
        let raw = match entry.downloads.as_ref().and_then(|d| d.raw.as_ref()) {
            Some(r) => r,
            None => continue,
        };

        let dest = runtime_root.join(rel_path);
        let folder = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| runtime_root.to_path_buf());

        items.push(DownloadItem {
            url: raw.url.clone(),
            path: dest,
            folder,
            name: rel_path.clone(),
            size: raw.size,
            r#type: Some("java".into()),
            sha1: Some(raw.sha1.clone()),
        });

        file_records.push(JavaFileItem {
            path: rel_path.clone(),
            executable: entry.executable,
            sha1: Some(raw.sha1.clone()),
            size: Some(raw.size),
            url: Some(raw.url.clone()),
            file_type: Some("file".into()),
        });
    }

    let downloader = Downloader::new(
        options.timeout_secs,
        options.clamped_concurrency(),
        options.force_ipv4,
    );
    downloader
        .download_multiple(items, event_tx.clone())
        .await?;

    #[cfg(unix)]
    for (rel_path, entry) in &manifest.files {
        if entry.executable == Some(true) {
            use std::os::unix::fs::PermissionsExt;
            let path = runtime_root.join(rel_path);
            if path.exists() {
                let perms = std::fs::Permissions::from_mode(0o755);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
    }

    // Find the java binary by scanning manifest entries — some Mojang runtimes
    // on macOS use a bundle layout (e.g. jre.bundle/Contents/Home/bin/java)
    // rather than the flat bin/java expected by java_bin_path.
    let java_bin = manifest
        .files
        .iter()
        .filter_map(|(rel_path, entry)| {
            if entry.executable != Some(true) {
                return None;
            }
            let p = std::path::Path::new(rel_path);
            let fname = p.file_name()?.to_str()?;
            let in_bin = p.parent()?.file_name()?.to_str()? == "bin";
            if in_bin && (fname == "java" || fname == "javaw.exe") {
                Some(runtime_root.join(rel_path))
            } else {
                None
            }
        })
        .next()
        .unwrap_or_else(|| java_bin_path(runtime_root));

    Ok(Some(JavaDownloadResult {
        java_path: java_bin.to_string_lossy().into_owned(),
        files: file_records,
    }))
}

// ── Adoptium fallback ─────────────────────────────────────────────────────────

async fn get_from_adoptium(
    options: &LaunchOptions,
    _component: &str,
    runtime_root: &Path,
    major_version: u32,
    client: &reqwest::Client,
    event_tx: &Sender<LaunchEvent>,
) -> Result<JavaDownloadResult, LaunchError> {
    let os = adoptium_os();
    let arch = adoptium_arch(options.intel_enabled_mac);
    let image_type = &options.java.image_type;

    let url = format!(
        "{ADOPTIUM_API_BASE}/{major_version}/hotspot?os={os}&architecture={arch}&image_type={image_type}&jvm_impl=hotspot&vendor=eclipse"
    );

    let releases: Vec<AdoptiumRelease> = fetch_json(client, &url)
        .await
        .map_err(LaunchError::InvalidData)?;

    let release = releases.into_iter().next().ok_or_else(|| {
        LaunchError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("No Adoptium release found for Java {major_version} on {os}/{arch}"),
        ))
    })?;

    let pkg = release.binary.package;
    let is_windows = cfg!(target_os = "windows");
    let ext = if is_windows { "zip" } else { "tar.gz" };
    let archive_path = runtime_root.join(format!("adoptium-jre.{ext}"));

    if let Some(parent) = archive_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let item = DownloadItem {
        url: pkg.link.clone(),
        path: archive_path.clone(),
        folder: runtime_root.to_path_buf(),
        name: pkg.name.clone(),
        size: 0,
        r#type: Some("java".into()),
        sha1: None,
    };

    let downloader = Downloader::new(options.timeout_secs, 1, options.force_ipv4);
    downloader
        .download_multiple(vec![item], event_tx.clone())
        .await?;

    if is_windows {
        extract_zip_to(archive_path.clone(), runtime_root).await?;
    } else {
        extract_tar_gz(archive_path.clone(), runtime_root.to_path_buf(), 1).await?;
    }

    let _ = tokio::fs::remove_file(&archive_path).await;

    let java_bin = java_bin_path(runtime_root);

    #[cfg(unix)]
    if java_bin.exists() {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = std::fs::set_permissions(&java_bin, perms);
    }

    Ok(JavaDownloadResult {
        java_path: java_bin.to_string_lossy().into_owned(),
        files: vec![JavaFileItem {
            path: java_bin.to_string_lossy().into_owned(),
            executable: Some(true),
            sha1: None,
            size: None,
            url: Some(pkg.link),
            file_type: Some("file".into()),
        }],
    })
}

// ── ZIP extraction (Windows) ──────────────────────────────────────────────────

async fn extract_zip_to(archive: PathBuf, dest: &Path) -> Result<(), LaunchError> {
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), LaunchError> {
        let file = std::fs::File::open(&archive)?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_owned();
            let stripped = name.splitn(2, '/').nth(1).unwrap_or(&name).to_owned();
            let out = dest.join(&stripped);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| {
        LaunchError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        ))
    })??;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bare_version() -> MinecraftVersionJson {
        MinecraftVersionJson {
            id: "1.20.4".into(),
            version_type: "release".into(),
            assets: None,
            asset_index: None,
            downloads: None,
            libraries: vec![],
            arguments: None,
            minecraft_arguments: None,
            java_version: None,
            main_class: None,
            has_natives: false,
        }
    }

    fn bare_options() -> LaunchOptions {
        use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
        use crate::models::minecraft::Authenticator;
        LaunchOptions {
            path: PathBuf::from("/mc"),
            version: "1.20.4".into(),
            authenticator: Authenticator {
                access_token: "tok".into(),
                name: "Player".into(),
                uuid: "uuid".into(),
                xbox_account: None,
                user_properties: None,
                client_id: None,
                client_token: None,
            },
            timeout_secs: 10,
            download_concurrency: 5,
            verify_concurrency: 4,
            memory: MemoryConfig::default(),
            java: JavaOptions::default(),
            loader: LoaderConfig::default(),
            screen: ScreenConfig::default(),
            verify: false,
            game_args: vec![],
            jvm_args: vec![],
            instance: None,
            url: None,
            mcp: None,
            intel_enabled_mac: false,
            bypass_offline: false,
            skip_bundle_check: false,
            force_ipv4: false,
        }
    }

    #[test]
    fn java_component_defaults_when_no_java_version() {
        let opts = bare_options();
        let vj = bare_version();
        let (comp, major) = java_component(&opts, &vj);
        assert_eq!(comp, "jre-8");
        assert_eq!(major, 8);
    }

    #[test]
    fn java_component_uses_version_json() {
        use crate::models::minecraft::JavaVersionInfo;
        let opts = bare_options();
        let mut vj = bare_version();
        vj.java_version = Some(JavaVersionInfo {
            component: Some("java-runtime-gamma".into()),
            major_version: Some(17),
        });
        let (comp, major) = java_component(&opts, &vj);
        assert_eq!(comp, "jre-17");
        assert_eq!(major, 17);
    }

    #[test]
    fn java_component_java_option_overrides_version_json() {
        use crate::models::minecraft::JavaVersionInfo;
        let mut opts = bare_options();
        opts.java.version = Some("21".into());
        let mut vj = bare_version();
        vj.java_version = Some(JavaVersionInfo {
            component: Some("java-runtime-gamma".into()),
            major_version: Some(17),
        });
        let (comp, major) = java_component(&opts, &vj);
        assert_eq!(comp, "jre-21");
        assert_eq!(major, 21);
    }

    #[test]
    fn java_bin_path_is_runtime_root_bin_java() {
        let root = PathBuf::from("/mc/runtime/jre-legacy/linux");
        let bin = java_bin_path(&root);
        let path_str = bin.to_string_lossy();
        // Must be exactly runtime_root/bin/java — no extra component segment.
        assert!(path_str.ends_with("java") || path_str.ends_with("javaw.exe"));
        assert!(path_str.contains("/bin/"));
        assert!(
            !path_str[root.to_str().unwrap().len()..].contains("jre-legacy"),
            "component name must not appear after runtime_root: {path_str}"
        );
    }

    #[test]
    fn mojang_platform_key_returns_non_empty() {
        let key = mojang_platform_key(false);
        assert!(!key.is_empty());
    }

    #[test]
    fn mojang_platform_key_intel_mac_overrides_arm() {
        // On any platform intel_enabled_mac=true must not produce the arm64 key.
        let key = mojang_platform_key(true);
        assert_ne!(key, "mac-os-arm64");
    }

    #[tokio::test]
    async fn get_java_files_respects_custom_java_path() {
        use crate::launcher::options::JavaOptions;
        use tokio::sync::mpsc;
        let mut opts = bare_options();
        opts.java = JavaOptions {
            path: Some(PathBuf::from("/usr/bin/java")),
            version: None,
            image_type: "jre".into(),
        };
        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::channel(16);
        let result = get_java_files(&opts, &bare_version(), &client, &tx)
            .await
            .unwrap();
        assert_eq!(result.java_path, "/usr/bin/java");
        assert!(result.files.is_empty());
    }

    #[tokio::test]
    async fn get_java_files_returns_cached_when_binary_exists() {
        use tempfile::TempDir;
        use tokio::sync::mpsc;

        let dir = TempDir::new().unwrap();
        let mut opts = bare_options();
        opts.path = dir.path().to_path_buf();

        let (comp, _) = java_component(&opts, &bare_version());
        let platform = mojang_platform_key(false);
        let runtime_root = dir.path().join("runtime").join(&comp).join(&platform);
        let bin_dir = runtime_root.join("bin");
        tokio::fs::create_dir_all(&bin_dir).await.unwrap();
        tokio::fs::write(bin_dir.join("java"), b"#!/bin/sh\nexec java")
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::channel(16);
        let result = get_java_files(&opts, &bare_version(), &client, &tx)
            .await
            .unwrap();

        assert!(result.java_path.contains("java"));
        assert!(result.files.is_empty());
    }
}
