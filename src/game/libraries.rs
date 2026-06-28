use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::LaunchError;
use crate::launcher::options::LaunchOptions;
use crate::models::minecraft::{ArtifactInfo, AssetItem, Library, MinecraftVersionJson};
use crate::net::http::fetch_json;
use crate::utils::paths::get_path_libraries;
use crate::utils::platform::{mojang_arch, mojang_os, skip_library};

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

            // Modern natives (1.19+) are classifier-based (`natives-<os>[-<arch>]`)
            // with rules that match every Windows / Linux / macOS. Without this
            // filter, the bundle would include every arch variant (e.g.
            // `natives-windows`, `natives-windows-x86`, `natives-windows-arm64`),
            // all of which flatten to the same file name on extraction and
            // overwrite each other — the last-written (usually the wrong arch)
            // is what the JVM ends up loading.
            if !native_classifier_arch_matches(&lib.name) {
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

/// Should the natives library with the given Maven coordinate be included on
/// the current host? Returns `true` when the library should be downloaded and
/// extracted.
///
/// Mojang's version JSON lists every `(os, arch)` variant of the natives
/// (e.g. `natives-windows`, `natives-windows-x86`, `natives-windows-arm64`)
/// with the same permissive rule (`os.name: windows`, `arch: null`) — the
/// arch filtering is encoded in the classifier itself, not the rule. Without
/// this check the bundle would include every arch variant and their binaries
/// would overwrite each other on extraction (the JVM would then load the
/// last-written one, typically the wrong arch).
///
/// Classifier grammar:
/// - `natives-<os>`           → default arch (`x86_64`).
/// - `natives-<os>-<suffix>`  → explicit arch; suffix is normalised to one of
///   `x86`, `x86_64`, `aarch64`, `arm` (Mojang uses several aliases:
///   `x86_32`, `64`, `arm64`, `aarch64`, `arm32`, `arm`, …).
/// - Anything else (no `natives-` prefix, or unknown suffix) is left to the
///   rest of the filtering: we only reject **known natives classifiers** that
///   don't match the host's arch. Non-native libraries and unknown suffixes
///   pass through.
///
/// `true` ⇒ include; `false` ⇒ skip the library entirely.
fn native_classifier_arch_matches(name: &str) -> bool {
    native_classifier_arch_matches_for(name, mojang_arch())
}

/// Like [`native_classifier_arch_matches`], but takes the current arch as an
/// argument. Exposed for unit tests so the truth table can be exercised on
/// every arch without spawning a subprocess.
fn native_classifier_arch_matches_for(name: &str, current_arch: &str) -> bool {
    let classifier = match name.split(':').nth(3) {
        Some(c) if c.starts_with("natives-") => c,
        _ => return true, // not a natives classifier — not our concern
    };

    // Classifier grammar: `natives-<os>[-<arch>]`. The OS is always the
    // first segment after `natives-`; the arch (if any) is the second.
    // We only apply the arch filter to known OSes — `skip_library`
    // already filters unknown OSes via the `os.name` rule upstream.
    let (os, arch_suffix) = match classifier.splitn(3, '-').collect::<Vec<_>>().as_slice() {
        ["natives", os, arch] => (*os, *arch),
        ["natives", os] => (*os, ""),
        _ => return true, // malformed — let upstream filters handle it
    };

    if !matches!(os, "linux" | "windows" | "osx" | "macos") {
        return true; // unknown OS — skip_library's job, not ours
    }

    // Normalise Mojang's classifier strings to the vocabulary of
    // `mojang_arch()` (`x86` | `x86_64` | `aarch64` | `arm`).
    // Unknown suffixes are excluded — fail-closed so a future classifier
    // we don't know about doesn't silently load the wrong binary.
    let classifier_arch = match arch_suffix {
        "" => "x86_64", // default for the no-suffix variant
        "x86" | "x86_32" | "32" => "x86",
        "x64" | "x86_64" | "64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        "arm" | "arm32" => "arm",
        _ => return false,
    };

    classifier_arch == current_arch
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
    fn get_libraries_filters_natives_by_current_arch() {
        // Regression: Mojang's 1.20.1 JSON lists every (os, arch) variant
        // of LWJGL natives with the same `os.name: <os>` rule and no
        // arch filter — e.g. on Windows, `natives-windows`,
        // `natives-windows-x86` and `natives-windows-arm64` all enter
        // the bundle. Their binaries (`*.dll` / `*.so` / `*.dylib`)
        // flatten to the same name during extraction, so the
        // last-written one — usually the wrong arch — is what the JVM
        // loads. After the classifier-arch filter, only the host's
        // variant survives.
        //
        // OS-agnostic: candidates are built from the current OS so
        // `skip_library` lets them through, then we check that exactly
        // one (the host's arch) reaches the bundle.
        let dir = TempDir::new().unwrap();
        let mut vj = bare_version();

        let current_os = mojang_os();
        let candidates: &[&str] = match current_os {
            "windows" => &[
                "natives-windows",
                "natives-windows-x86",
                "natives-windows-arm64",
            ],
            "linux" => &[
                "natives-linux",
                "natives-linux-aarch64",
                "natives-linux-arm64",
            ],
            "osx" => &["natives-osx", "natives-osx-arm64"],
            // Unknown OS: fall back to a single classifier so the test
            // still exercises the path.
            _ => &["natives-windows"],
        };

        let mut libs = Vec::new();
        for classifier in candidates {
            let lib_name = format!("org.lwjgl:lwjgl:3.3.1:{classifier}");
            let jar_path =
                format!("org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-{classifier}.jar");
            libs.push(Library {
                name: lib_name,
                rules: Some(vec![crate::utils::platform::LibraryRule {
                    action: crate::utils::platform::RuleAction::Allow,
                    os: Some(crate::utils::platform::OsRule {
                        name: Some(current_os.into()),
                        version: None,
                        arch: None,
                    }),
                    features: None,
                }]),
                natives: None,
                downloads: Some(LibraryDownloads {
                    artifact: Some(artifact(
                        &jar_path,
                        &format!("https://example.com/{jar_path}"),
                    )),
                    classifiers: None,
                }),
                url: None,
                loader: None,
            });
        }
        vj.libraries = libs;

        let items = get_libraries(&opts(dir.path().to_path_buf()), &vj);
        let native_files: Vec<String> = items
            .iter()
            .filter_map(|i| match i {
                AssetItem::NativeAsset { path, .. } => std::path::Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();

        // Every survivor must be one of the candidates (sanity: no
        // foreign-OS native leaked through).
        for file in &native_files {
            assert!(
                candidates.iter().any(|c| file.contains(c)),
                "unexpected native {file}; candidates were {candidates:?}"
            );
        }

        // Re-derive the classifier from the file name
        // (`<artifact>-natives-<os>[-<arch>].jar`) and verify the arch
        // component matches the host. Mirrors the production helper so
        // the test stays self-consistent.
        for file in &native_files {
            let stem = file.strip_suffix(".jar").unwrap_or(file);
            let classifier = match stem.split("-natives-").nth(1) {
                Some(c) => c,
                None => panic!("native file {file} has no `-natives-` segment"),
            };
            let mut parts = classifier.splitn(2, '-');
            let _os = parts.next().unwrap_or("");
            let arch_suffix = parts.next().unwrap_or("");
            let classifier_arch = match arch_suffix {
                "" => "x86_64",
                "x86" | "x86_32" | "32" => "x86",
                "x64" | "x86_64" | "64" => "x86_64",
                "arm64" | "aarch64" => "aarch64",
                "arm" | "arm32" => "arm",
                other => panic!("unexpected arch suffix {other:?} in {file}"),
            };
            assert_eq!(
                classifier_arch,
                mojang_arch(),
                "arch mismatch in {file}: classifier says {classifier_arch}, host is {}",
                mojang_arch()
            );
        }

        // Exactly one native must survive: the one matching the host's
        // arch. All other variants of the same OS are excluded by the
        // classifier-arch filter.
        assert_eq!(
            native_files.len(),
            1,
            "expected exactly 1 native for current arch {} on os {}, got {:?}",
            mojang_arch(),
            current_os,
            native_files
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

    // ── native_classifier_arch_matches ────────────────────────────────────────

    #[test]
    fn classifier_match_default_x86_64_on_x86_64() {
        assert!(native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-windows",
            "x86_64"
        ));
        assert!(native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-linux",
            "x86_64"
        ));
        assert!(native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-macos",
            "x86_64"
        ));
    }

    #[test]
    fn classifier_match_default_arch_skipped_on_non_default_archs() {
        // The no-suffix variant means "x86_64" — it must NOT be included on
        // aarch64 (where the explicit `-arm64`/`-aarch64` variant is needed)
        // or x86 (where `-x86` is needed).
        for arch in ["x86", "aarch64", "arm"] {
            assert!(
                !native_classifier_arch_matches_for("org.lwjgl:lwjgl:3.3.1:natives-linux", arch),
                "natives-linux must be skipped on arch={arch}"
            );
            assert!(
                !native_classifier_arch_matches_for("org.lwjgl:lwjgl:3.3.1:natives-windows", arch),
                "natives-windows must be skipped on arch={arch}"
            );
        }
    }

    #[test]
    fn classifier_match_explicit_x86_aliases() {
        // All three are equivalent Mojang spellings for x86.
        for classifier in [
            "natives-windows-x86",
            "natives-windows-x86_32",
            "natives-windows-32",
        ] {
            assert!(
                native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "x86"
                ),
                "{classifier} must match arch=x86"
            );
            assert!(
                !native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "x86_64"
                ),
                "{classifier} must NOT match arch=x86_64"
            );
        }
    }

    #[test]
    fn classifier_match_explicit_x86_64_aliases() {
        for classifier in [
            "natives-linux-x64",
            "natives-linux-x86_64",
            "natives-linux-64",
        ] {
            assert!(
                native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "x86_64"
                ),
                "{classifier} must match arch=x86_64"
            );
            assert!(
                !native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "x86"
                ),
                "{classifier} must NOT match arch=x86"
            );
        }
    }

    #[test]
    fn classifier_match_arm_aliases() {
        for classifier in ["natives-linux-aarch64", "natives-linux-arm64"] {
            assert!(
                native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "aarch64"
                ),
                "{classifier} must match arch=aarch64"
            );
            assert!(
                !native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "x86_64"
                ),
                "{classifier} must NOT match arch=x86_64"
            );
        }
        for classifier in ["natives-linux-arm32", "natives-linux-arm"] {
            assert!(
                native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "arm"
                ),
                "{classifier} must match arch=arm"
            );
            assert!(
                !native_classifier_arch_matches_for(
                    &format!("org.lwjgl:lwjgl:3.3.1:{classifier}"),
                    "aarch64"
                ),
                "{classifier} must NOT match arch=aarch64"
            );
        }
    }

    #[test]
    fn classifier_match_unknown_suffix_is_excluded() {
        // Known OS + unknown arch suffix: fail-closed so a future
        // classifier we don't recognise doesn't silently load the wrong
        // binary. This is the case the user's bug hinges on — Mojang
        // could add a new arch (riscv64, loongarch64, …) and we'd refuse
        // to pick the wrong one.
        assert!(!native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-windows-riscv64",
            "x86_64"
        ));
        assert!(!native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-linux-loongarch64",
            "x86_64"
        ));
    }

    #[test]
    fn classifier_match_unknown_os_passes_through() {
        // Unknown OSes (e.g. `natives-foo-…`) aren't our concern — the
        // `os.name` rule check in `skip_library` filters them by the
        // current platform. We must not interfere.
        assert!(native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-foo-32",
            "x86"
        ));
        assert!(native_classifier_arch_matches_for(
            "org.lwjgl:lwjgl:3.3.1:natives-foo",
            "x86_64"
        ));
    }

    #[test]
    fn classifier_match_non_native_passes_through() {
        // Regular libraries (no `natives-` prefix in the classifier) are not
        // touched by the filter — `is_native=false` upstream.
        assert!(native_classifier_arch_matches("org.lwjgl:lwjgl:3.3.1"));
        assert!(native_classifier_arch_matches(
            "net.java.dev.jna:jna:5.12.1"
        ));
        // Non-native classifier (e.g. sources/javadoc) also passes through.
        assert!(native_classifier_arch_matches(
            "com.example:lib:1.0:sources"
        ));
        assert!(native_classifier_arch_matches(
            "com.example:lib:1.0:javadoc"
        ));
        // Three-segment coordinates (no classifier) are also non-native.
        assert!(native_classifier_arch_matches("com.example:lib:1.0"));
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
