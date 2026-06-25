use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::LaunchError;
use crate::launcher::options::LaunchOptions;
use crate::models::minecraft::{ArtifactInfo, AssetItem, Library, MinecraftVersionJson};
use crate::net::http::fetch_json;
use crate::utils::paths::get_path_libraries;
use crate::utils::platform::{mojang_os, skip_library};

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the full list of library/native items to include in the download
/// bundle.
///
/// For each library in `version_json.libraries`:
/// - **Native** (has a `natives` map): selects the platform-specific
///   classifier and emits a `NativeAsset`.
/// - **Regular**: applies Mojang rule evaluation via [`skip_library`]; emits
///   an `Asset`.  Falls back to deriving the URL from `lib.url` + Maven
///   coordinate when `downloads.artifact` is absent.
///
/// Appends the client JAR and the serialised version JSON (as a `CFile`) at
/// the end.
pub fn get_libraries(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
) -> Vec<AssetItem> {
    let base = &options.path;
    let current_os = mojang_os();
    let arch_suffix = arch_suffix_for_natives();

    let mut items: Vec<AssetItem> = Vec::new();

    for lib in &version_json.libraries {
        if let Some(natives_map) = &lib.natives {
            // ── Native branch ────────────────────────────────────────────────
            let native_key = match natives_map.get(current_os) {
                Some(k) => k.replace("${arch}", arch_suffix),
                None => continue,
            };

            let artifact = lib
                .downloads
                .as_ref()
                .and_then(|d| d.classifiers.as_ref())
                .and_then(|c| c.get(&native_key));

            if let Some(artifact) = artifact {
                if let Some(item) = artifact_to_item(base, artifact, &lib.name, true) {
                    items.push(item);
                }
            }
        } else {
            // ── Regular branch ───────────────────────────────────────────────
            if skip_library(lib.rules.as_deref().unwrap_or(&[])) {
                continue;
            }

            if let Some(item) = resolve_regular_library(base, lib) {
                items.push(item);
            }
        }
    }

    // Client JAR
    if let Some(dl) = &version_json.downloads {
        items.push(AssetItem::Asset {
            path: base
                .join("versions")
                .join(&version_json.id)
                .join(format!("{}.jar", version_json.id))
                .to_string_lossy()
                .into_owned(),
            sha1: dl.client.sha1.clone(),
            size: dl.client.size,
            url: dl.client.url.clone(),
        });
    }

    // Version JSON as CFile (written verbatim, no download needed)
    if let Ok(content) = serde_json::to_string(version_json) {
        items.push(AssetItem::CFile {
            path: base
                .join("versions")
                .join(&version_json.id)
                .join(format!("{}.json", version_json.id))
                .to_string_lossy()
                .into_owned(),
            content,
        });
    }

    items
}

/// Fetch a list of custom/additional assets from a remote URL.
///
/// The URL is expected to return a JSON array of
/// `{ path, hash, size, url }` objects.  Each entry is emitted as an
/// `AssetItem::Asset` with its path prefixed by `options.path` (and
/// `instances/<instance>/` when instanced).
///
/// Returns an empty `Vec` when `url` is `None`.
pub async fn get_assets_others(
    options: &LaunchOptions,
    url: Option<&str>,
    client: &reqwest::Client,
) -> Result<Vec<AssetItem>, LaunchError> {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return Ok(vec![]),
    };

    let raw: Vec<CustomAssetItem> = fetch_json(client, url)
        .await
        .map_err(LaunchError::InvalidData)?;

    let mut items = Vec::with_capacity(raw.len());

    for asset in raw {
        if asset.path.is_empty() {
            continue;
        }

        let full_path = match &options.instance {
            Some(inst) => options.path.join("instances").join(inst).join(&asset.path),
            None => options.path.join(&asset.path),
        };

        items.push(AssetItem::Asset {
            path: full_path.to_string_lossy().into_owned(),
            sha1: asset.hash,
            size: asset.size,
            url: asset.url,
        });
    }

    Ok(items)
}

/// Base natives directory for a version: `<path>/versions/<id>/natives`.
///
/// This is the value `${natives_directory}` expands to in the version JSON's
/// JVM arguments.
pub fn natives_base_dir(options: &LaunchOptions, version_json: &MinecraftVersionJson) -> PathBuf {
    options
        .path
        .join("versions")
        .join(&version_json.id)
        .join("natives")
}

/// The directory natives must be extracted to so the JVM can load them.
///
/// `-Djava.library.path` is a flat search path, and its value is version-
/// dependent:
/// - Minecraft 26.x (LWJGL 3.4) sets it to `${natives_directory}/java` and
///   reserves sibling dirs (`/lwjgl`, `/jna`, `/netty`) as runtime scratch
///   space — so the loadable binaries belong in the `java` subdirectory.
/// - Older versions use `${natives_directory}` itself.
///
/// We mirror whatever the version's own `java.library.path` arg resolves to, so
/// extraction and the JVM agree on one location.
pub fn natives_dir_for(options: &LaunchOptions, version_json: &MinecraftVersionJson) -> PathBuf {
    let base = natives_base_dir(options, version_json);
    match natives_library_subdir(version_json) {
        Some(sub) => base.join(sub),
        None => base,
    }
}

/// Component appended to `${natives_directory}` by the version's
/// `-Djava.library.path` JVM argument, if any (e.g. `"java"` for 26.x).
///
/// Returns `None` when the version points `java.library.path` straight at
/// `${natives_directory}` or doesn't specify one.
fn natives_library_subdir(version_json: &MinecraftVersionJson) -> Option<String> {
    const PREFIX: &str = "-Djava.library.path=${natives_directory}";
    let jvm = version_json.arguments.as_ref()?.jvm.as_ref()?;
    for entry in jvm {
        // These properties are always plain (unconditional) string entries.
        let Some(s) = entry.as_str() else { continue };
        let Some(rest) = s.strip_prefix(PREFIX) else {
            continue;
        };
        let rest = rest.trim_start_matches('/');
        return if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        };
    }
    None
}

/// Extract all native JARs in `bundle` (`NativeAsset` items) into the directory
/// that the version's `-Djava.library.path` points to (see
/// [`natives_dir_for`]).
///
/// Skips `META-INF/` entries; sets executable bit on Unix (0o755).
/// Uses `spawn_blocking` so the synchronous `zip` operations don't block the
/// Tokio runtime.
pub async fn extract_natives(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
    bundle: &[AssetItem],
) -> Result<(), LaunchError> {
    let native_paths: Vec<PathBuf> = bundle
        .iter()
        .filter_map(|item| match item {
            AssetItem::NativeAsset { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect();

    if native_paths.is_empty() {
        return Ok(());
    }

    let natives_dir = natives_dir_for(options, version_json);
    tokio::fs::create_dir_all(&natives_dir).await?;

    for jar_path in native_paths {
        let dest = natives_dir.clone();
        tokio::task::spawn_blocking(move || extract_jar_to_dir(&jar_path, &dest))
            .await
            .map_err(|e| LaunchError::Archive(e.to_string()))??;
    }

    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Arch suffix used in native classifier names (`${arch}` placeholder).
///
/// Mojang classifiers use `"32"` / `"64"` for x86 variants; ARM platforms
/// leave the suffix empty.
fn arch_suffix_for_natives() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "32",
        "x86_64" => "64",
        _ => "",
    }
}

/// Convert an `ArtifactInfo` to an `AssetItem`, deriving the relative path
/// from the Maven coordinate when `artifact.path` is absent.
fn artifact_to_item(
    base: &Path,
    artifact: &ArtifactInfo,
    lib_name: &str,
    is_native: bool,
) -> Option<AssetItem> {
    let rel = artifact.path.clone().or_else(|| {
        get_path_libraries(lib_name, None, None)
            .ok()
            .map(|lp| lp.path)
    })?;

    let full_path = base
        .join("libraries")
        .join(&rel)
        .to_string_lossy()
        .into_owned();

    let sha1 = artifact.sha1.clone().unwrap_or_default();
    let size = artifact.size.unwrap_or(0);
    let url = artifact.url.clone();

    if is_native {
        Some(AssetItem::NativeAsset {
            path: full_path,
            sha1,
            size,
            url,
        })
    } else {
        Some(AssetItem::Asset {
            path: full_path,
            sha1,
            size,
            url,
        })
    }
}

/// Resolve a regular (non-native) library to an `AssetItem`.
///
/// Priority:
/// 1. `downloads.artifact` — direct download info from Mojang JSON.
/// 2. `lib.url` + Maven coordinate — for loader-injected libraries
///    (Fabric/Quilt) that carry a repository URL but no direct download block.
fn resolve_regular_library(base: &Path, lib: &Library) -> Option<AssetItem> {
    // Modern Minecraft (1.19+) encodes natives as separate library entries
    // with a "natives-<platform>" classifier in the Maven coordinate
    // (e.g. "org.lwjgl:lwjgl-glfw:3.3.2:natives-linux") instead of the
    // old-style `lib.natives` map. OS filtering is handled via rules so
    // by the time we reach here the library already matched the current
    // platform. Mark it as a native so its contents are extracted to the
    // natives directory rather than placed on the classpath.
    let is_native = lib
        .name
        .split(':')
        .nth(3)
        .map(|c| c.starts_with("natives-"))
        .unwrap_or(false);

    // Priority 1 — explicit artifact download block
    if let Some(artifact) = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
        return artifact_to_item(base, artifact, &lib.name, is_native);
    }

    // Priority 2 — build URL from Maven coordinate + repo base URL
    if let Some(repo) = &lib.url {
        if let Ok(lp) = get_path_libraries(&lib.name, None, None) {
            let url = format!("{}/{}", repo.trim_end_matches('/'), lp.path);
            return Some(AssetItem::Asset {
                path: base
                    .join("libraries")
                    .join(&lp.path)
                    .to_string_lossy()
                    .into_owned(),
                sha1: String::new(),
                size: 0,
                url,
            });
        }
    }

    None
}

/// Extract the native binaries from a JAR/ZIP into `dest`, **flattened**.
///
/// `-Djava.library.path` is a flat search path — the JVM does not recurse into
/// subdirectories — so every native binary must land directly in `dest`. The
/// jar's internal layout varies by LWJGL version:
/// - LWJGL ≤ 3.3 puts binaries at the jar root (`liblwjgl.so`).
/// - LWJGL ≥ 3.4 (Minecraft 26.x) nests them under `<os>/<arch>/…`
///   (`linux/x64/org/lwjgl/liblwjgl.so`).
///
/// Extracting by file name handles both; preserving the path would hide the
/// 3.4 binaries in a subdirectory and crash with "can't find liblwjgl.so".
///
/// Called inside `spawn_blocking`; all I/O is synchronous.
fn extract_jar_to_dir(jar_path: &Path, dest: &Path) -> Result<(), LaunchError> {
    let file = std::fs::File::open(jar_path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| LaunchError::Archive(e.to_string()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| LaunchError::Archive(e.to_string()))?;

        let name = entry.name().to_string();

        // META-INF holds the manifest and per-binary `.sha1`/`.git` markers —
        // none are loadable libraries.
        if name.starts_with("META-INF") || entry.is_dir() {
            continue;
        }

        // Flatten: drop the internal directory structure and key by file name.
        let file_name = match Path::new(&name).file_name() {
            Some(f) => f,
            None => continue,
        };
        let out = dest.join(file_name);

        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        std::fs::write(&out, &data)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&out)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&out, perms)?;
        }
    }

    Ok(())
}

// ── Custom asset item (from remote URL) ───────────────────────────────────────

#[derive(Deserialize)]
struct CustomAssetItem {
    path: String,
    hash: String,
    size: u64,
    url: String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
    use crate::models::minecraft::{
        ArtifactInfo, Authenticator, DownloadArtifact, LibraryDownloads, VersionDownloads,
    };

    fn opts(path: PathBuf) -> LaunchOptions {
        LaunchOptions {
            path,
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
            dns: None,
        }
    }

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

    fn artifact(path: &str, url: &str) -> ArtifactInfo {
        ArtifactInfo {
            path: Some(path.into()),
            sha1: Some("aabbcc".into()),
            size: Some(1024),
            url: url.into(),
        }
    }

    fn lib_with_artifact(name: &str, path: &str, url: &str) -> Library {
        Library {
            name: name.into(),
            rules: None,
            natives: None,
            downloads: Some(LibraryDownloads {
                artifact: Some(artifact(path, url)),
                classifiers: None,
            }),
            url: None,
            loader: None,
        }
    }

    // ── get_libraries ─────────────────────────────────────────────────────────

    #[test]
    fn includes_client_jar_when_downloads_present() {
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();
        vj.downloads = Some(VersionDownloads {
            client: DownloadArtifact {
                sha1: "abc".into(),
                size: 42,
                url: "https://example.com/client.jar".into(),
            },
            server: None,
            client_mappings: None,
            server_mappings: None,
        });

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(items
            .iter()
            .any(|i| matches!(i, AssetItem::Asset { path, .. } if path.ends_with("1.20.4.jar"))));
    }

    #[test]
    fn includes_version_json_as_cfile() {
        let dir = TempDir::new().unwrap();
        let vj = bare_version();
        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(items
            .iter()
            .any(|i| matches!(i, AssetItem::CFile { path, .. } if path.ends_with("1.20.4.json"))));
    }

    #[test]
    fn regular_library_becomes_asset() {
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();
        vj.libraries = vec![lib_with_artifact(
            "com.example:lib:1.0",
            "com/example/lib/1.0/lib-1.0.jar",
            "https://example.com/lib.jar",
        )];

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(items.iter().any(
            |i| matches!(i, AssetItem::Asset { url, .. } if url == "https://example.com/lib.jar")
        ));
    }

    #[test]
    fn native_library_becomes_native_asset() {
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();

        let current_os = mojang_os();
        let classifier_key = format!("natives-{current_os}");

        let mut classifiers = std::collections::HashMap::new();
        classifiers.insert(
            classifier_key.clone(),
            artifact(
                &format!("org/lwjgl/lwjgl/{classifier_key}/lwjgl-native.jar"),
                "https://example.com/native.jar",
            ),
        );

        let mut natives_map = std::collections::HashMap::new();
        natives_map.insert(current_os.to_string(), classifier_key);

        vj.libraries = vec![Library {
            name: "org.lwjgl:lwjgl:3.3.1".into(),
            rules: None,
            natives: Some(natives_map),
            downloads: Some(LibraryDownloads {
                artifact: None,
                classifiers: Some(classifiers),
            }),
            url: None,
            loader: None,
        }];

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(items.iter().any(|i| matches!(i, AssetItem::NativeAsset { url, .. } if url == "https://example.com/native.jar")));
    }

    #[test]
    fn modern_native_classifier_in_name_becomes_native_asset() {
        // 1.19+ LWJGL natives: no `lib.natives` map; classifier is in the name.
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();

        let current_os = mojang_os();
        let classifier = format!("natives-{current_os}");
        let lib_name = format!("org.lwjgl:lwjgl-glfw:3.3.2:{classifier}");
        let jar_path = format!("org/lwjgl/lwjgl-glfw/3.3.2/lwjgl-glfw-3.3.2-{classifier}.jar");

        vj.libraries = vec![Library {
            name: lib_name,
            rules: None,
            natives: None,
            downloads: Some(LibraryDownloads {
                artifact: Some(artifact(
                    &jar_path,
                    "https://libraries.minecraft.net/native.jar",
                )),
                classifiers: None,
            }),
            url: None,
            loader: None,
        }];

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(
            items
                .iter()
                .any(|i| matches!(i, AssetItem::NativeAsset { .. })),
            "expected NativeAsset for modern natives-<os> classifier, got: {items:?}"
        );
    }

    #[test]
    fn library_with_url_fallback_builds_url() {
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();
        vj.libraries = vec![Library {
            name: "net.fabricmc:fabric-loader:0.15.0".into(),
            rules: None,
            natives: None,
            downloads: None,
            url: Some("https://maven.fabricmc.net".into()),
            loader: None,
        }];

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        assert!(items.iter().any(|i| match i {
            AssetItem::Asset { url, .. } => url.starts_with("https://maven.fabricmc.net"),
            _ => false,
        }));
    }

    // ── get_assets_others ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_assets_others_none_url_returns_empty() {
        let dir = TempDir::new().unwrap();
        let client = reqwest::Client::new();
        let result = get_assets_others(&opts(dir.path().to_path_buf()), None, &client)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_assets_others_empty_string_returns_empty() {
        let dir = TempDir::new().unwrap();
        let client = reqwest::Client::new();
        let result = get_assets_others(&opts(dir.path().to_path_buf()), Some(""), &client)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // ── extract_natives ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn extract_natives_noop_with_empty_bundle() {
        let dir = TempDir::new().unwrap();
        let vj = bare_version();
        extract_natives(&opts(dir.path().to_path_buf()), &vj, &[])
            .await
            .unwrap();
        assert!(!dir.path().join("versions").exists());
    }

    #[tokio::test]
    async fn extract_natives_extracts_to_natives_dir() {
        // Build a tiny ZIP with one native file and one META-INF entry.
        let dir = TempDir::new().unwrap();
        let jar_path = dir.path().join("native.jar");

        {
            use zip::write::SimpleFileOptions;
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts_zip = SimpleFileOptions::default();

            w.start_file("META-INF/MANIFEST.MF", opts_zip).unwrap();
            w.write_all(b"Manifest-Version: 1.0\n").unwrap();

            // META-INF marker files must never be extracted.
            w.start_file("META-INF/linux/x64/org/lwjgl/liblwjgl.so.sha1", opts_zip)
                .unwrap();
            w.write_all(b"deadbeef").unwrap();

            // LWJGL ≤ 3.3 layout: binary at the jar root.
            w.start_file("liblwjgl.so", opts_zip).unwrap();
            w.write_all(b"ELF root native").unwrap();

            // LWJGL ≥ 3.4 layout: binary nested under <os>/<arch>/… — must be
            // flattened into the natives dir, not buried in subdirectories.
            w.start_file("linux/x64/org/lwjgl/liblwjgl_opengl.so", opts_zip)
                .unwrap();
            w.write_all(b"ELF nested native").unwrap();

            let finished = w.finish().unwrap();
            std::fs::write(&jar_path, finished.get_ref()).unwrap();
        }

        let vj = bare_version();
        let options = opts(dir.path().to_path_buf());

        let bundle = vec![AssetItem::NativeAsset {
            path: jar_path.to_string_lossy().into_owned(),
            sha1: String::new(),
            size: 0,
            url: String::new(),
        }];

        extract_natives(&options, &vj, &bundle).await.unwrap();

        let natives_dir = dir.path().join("versions").join("1.20.4").join("natives");

        // Root-level binary lands directly in the natives dir.
        assert_eq!(
            std::fs::read(natives_dir.join("liblwjgl.so")).unwrap(),
            b"ELF root native"
        );
        // Nested binary is flattened to the natives dir root (the fix).
        assert_eq!(
            std::fs::read(natives_dir.join("liblwjgl_opengl.so")).unwrap(),
            b"ELF nested native"
        );
        // No directory structure or META-INF leaks through.
        assert!(!natives_dir.join("linux").exists());
        assert!(!natives_dir.join("META-INF").exists());
        assert!(!natives_dir.join("liblwjgl.so.sha1").exists());
    }

    /// Build a version whose `arguments.jvm` mirrors Minecraft 26.x: it points
    /// `java.library.path` at the `${natives_directory}/java` subdirectory.
    fn version_with_java_subdir(id: &str) -> MinecraftVersionJson {
        let mut vj = bare_version();
        vj.id = id.to_string();
        vj.arguments = Some(crate::models::minecraft::Arguments {
            game: None,
            jvm: Some(vec![
                serde_json::Value::String("--enable-native-access=ALL-UNNAMED".into()),
                serde_json::Value::String("-Djava.library.path=${natives_directory}/java".into()),
                serde_json::Value::String(
                    "-Dorg.lwjgl.system.SharedLibraryExtractPath=${natives_directory}/lwjgl".into(),
                ),
            ]),
        });
        vj
    }

    #[test]
    fn natives_subdir_detected_for_modern_scheme() {
        let vj = version_with_java_subdir("26.2");
        assert_eq!(natives_library_subdir(&vj), Some("java".to_string()));
        // Legacy versions (no arguments.jvm) extract straight to the root.
        assert_eq!(natives_library_subdir(&bare_version()), None);

        let options = opts(PathBuf::from("/tmp/mc"));
        let expected = options
            .path
            .join("versions")
            .join("26.2")
            .join("natives")
            .join("java");
        assert_eq!(natives_dir_for(&options, &vj), expected);
    }

    #[tokio::test]
    async fn extract_natives_targets_java_subdir_on_modern_versions() {
        // Regression: Minecraft 26.x sets `java.library.path=${natives_directory}/java`,
        // so liblwjgl.so must land in `natives/java/`, not `natives/`. Extracting
        // to the root makes the JVM crash with "Failed to locate library: liblwjgl.so".
        let dir = TempDir::new().unwrap();
        let jar_path = dir.path().join("native.jar");
        {
            use zip::write::SimpleFileOptions;
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let o = SimpleFileOptions::default();
            w.start_file("linux/x64/org/lwjgl/liblwjgl.so", o).unwrap();
            w.write_all(b"ELF").unwrap();
            let finished = w.finish().unwrap();
            std::fs::write(&jar_path, finished.get_ref()).unwrap();
        }

        let vj = version_with_java_subdir("26.2");
        let options = opts(dir.path().to_path_buf());
        let bundle = vec![AssetItem::NativeAsset {
            path: jar_path.to_string_lossy().into_owned(),
            sha1: String::new(),
            size: 0,
            url: String::new(),
        }];

        extract_natives(&options, &vj, &bundle).await.unwrap();

        let natives_root = dir.path().join("versions").join("26.2").join("natives");
        assert!(
            natives_root.join("java").join("liblwjgl.so").exists(),
            "liblwjgl.so must be in the java.library.path subdir"
        );
        assert!(
            !natives_root.join("liblwjgl.so").exists(),
            "must not be left at the natives root for modern versions"
        );
    }
}
