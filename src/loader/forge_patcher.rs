/// Forge/NeoForge install-processor patcher.
///
/// This is a faithful Rust adaptation of `Minecraft-Loader/patcher.ts` from
/// `minecraft-java-core`. It runs each entry in `install_profile.json → processors`
/// sequentially, resolving placeholder arguments and the Main-Class from each
/// processor JAR's MANIFEST.MF, then spawning a Java process for it.
///
/// ## Intermediate-path strategy (caller's responsibility)
///
/// 1. Call `ForgePatcher::check()` — if all expected output files already exist,
///    patching was done in a previous run; skip entirely.
/// 2. Call `ForgePatcher::patch()` — runs the processors.
/// 3. On `LoaderError::ProfileNotFound` or any unrecoverable error, fall back
///    to the official `--installClient` installer approach.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::models::loader::{ForgeProfile, LoaderType};
use crate::utils::archive::{get_file_from_archive, ArchiveQueryResult};
use crate::utils::paths::get_path_libraries;

// ── Config ────────────────────────────────────────────────────────────────────

/// Runtime paths needed to resolve processor arguments.
///
/// Mirrors the `Config` interface from patcher.ts:
/// `{ java, minecraft, minecraftJson }` + the base `options.path`.
pub struct PatchConfig<'a> {
    /// Path to the Java binary used to run processors.
    pub java_path: &'a str,
    /// Absolute path to the vanilla Minecraft client JAR (`{MINECRAFT_JAR}`).
    pub minecraft_jar: &'a str,
    /// Absolute path to the vanilla Minecraft version JSON (`{MINECRAFT_VERSION}`).
    pub minecraft_json: &'a str,
    /// Main game data directory — used for `{ROOT}`, `{INSTALLER}`, `{LIBRARY_DIR}`.
    /// In the TS this is `options.path`; in Rust pass `options.path`.
    pub game_path: &'a Path,
}

// ── Patcher ───────────────────────────────────────────────────────────────────

/// Runs Forge/NeoForge install processors one by one.
///
/// Equivalent to the TypeScript `ForgePatcher` class.
pub struct ForgePatcher {
    /// Directory where the loader installed its libraries
    /// (e.g. `<options.path>/loader/forge/`).
    pub loader_base: PathBuf,
    /// Loader variant — used to locate the "universal" library in the profile.
    pub loader_type: LoaderType,
}

impl ForgePatcher {
    pub fn new(loader_base: PathBuf, loader_type: LoaderType) -> Self {
        Self {
            loader_base,
            loader_type,
        }
    }

    /// Check whether patching has already been applied.
    ///
    /// Iterates every client-side processor's arguments, collects the output
    /// file paths referenced via `profile.data`, and verifies each exists on
    /// disk. Returns `true` when all files are present (skip patching).
    ///
    /// Mirrors `ForgePatcher.check()` from patcher.ts.
    pub fn check(&self, profile: &ForgeProfile) -> bool {
        let processors = match profile.processors.as_deref() {
            Some(p) if !p.is_empty() => p,
            _ => return true,
        };
        let data = match &profile.data {
            Some(d) => d,
            None => return true,
        };

        let mut files: Vec<String> = Vec::new();

        for processor in processors {
            if !is_client_side(processor.sides.as_deref()) {
                continue;
            }
            for arg in &processor.args {
                let key = strip_braces(arg);
                if key == "BINPATCH" {
                    continue;
                }
                if let Some(entry) = data.get(key) {
                    let val = entry.client.trim_matches(|c| c == '[' || c == ']');
                    if !val.is_empty() && !files.contains(&val.to_owned()) {
                        files.push(val.to_owned());
                    }
                }
            }
        }

        for file in &files {
            let coord = file.trim_matches(|c| c == '[' || c == ']');
            if let Ok(info) = get_path_libraries(coord, None, None) {
                let path = self
                    .loader_base
                    .join("libraries")
                    .join(&info.path)
                    .join(&info.name);
                if !path.exists() {
                    return false;
                }
            }
        }

        true
    }

    /// Run all client-side processors from the install profile sequentially.
    ///
    /// For each processor:
    /// 1. Resolve arguments via `set_argument` + `compute_path`.
    /// 2. Build a classpath from the processor JAR and its declared dependencies.
    /// 3. Read `Main-Class` from `META-INF/MANIFEST.MF` inside the processor JAR.
    /// 4. Spawn `java -classpath <cp> <MainClass> <args>`.
    /// 5. Stream stdout/stderr as `LaunchEvent::Patch` events.
    ///
    /// Mirrors `ForgePatcher.patcher()` from patcher.ts.
    pub async fn patch(
        &self,
        profile: &ForgeProfile,
        config: &PatchConfig<'_>,
        neo_forge_old: bool,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<(), LoaderError> {
        let processors = profile
            .processors
            .as_ref()
            .ok_or(LoaderError::ProfileNotFound)?;

        for processor in processors {
            if !is_client_side(processor.sides.as_deref()) {
                continue;
            }

            // ── Resolve processor JAR path ────────────────────────────────────
            let jar_info = match get_path_libraries(&processor.jar, None, None) {
                Ok(i) => i,
                Err(_) => {
                    let _ = event_tx
                        .send(LaunchEvent::Patch(format!(
                            "[patcher] Cannot resolve processor JAR: {}",
                            processor.jar
                        )))
                        .await;
                    continue;
                }
            };
            let jar_path = self
                .loader_base
                .join("libraries")
                .join(&jar_info.path)
                .join(&jar_info.name);

            // ── Resolve arguments ─────────────────────────────────────────────
            let args: Vec<String> = processor
                .args
                .iter()
                .map(|a| self.set_argument(a, profile, config, neo_forge_old))
                .map(|a| self.compute_path(&a))
                .collect();

            // ── Build classpath ───────────────────────────────────────────────
            // Processor JAR comes first, then each entry in processor.classpath.
            let mut cp_entries: Vec<String> = vec![jar_path.to_string_lossy().into_owned()];
            for cp_coord in &processor.classpath {
                if let Ok(info) = get_path_libraries(cp_coord, None, None) {
                    let p = self
                        .loader_base
                        .join("libraries")
                        .join(&info.path)
                        .join(&info.name);
                    cp_entries.push(p.to_string_lossy().into_owned());
                }
            }
            let classpath = cp_entries.join(cp_separator());

            // ── Read Main-Class from the processor JAR's manifest ─────────────
            let main_class = match read_jar_manifest(&jar_path).await {
                Ok(Some(c)) => c,
                Ok(None) => {
                    let _ = event_tx
                        .send(LaunchEvent::Patch(format!(
                            "[patcher] No Main-Class in manifest: {}",
                            jar_path.display()
                        )))
                        .await;
                    continue;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(LaunchEvent::Patch(format!(
                            "[patcher] Failed reading manifest for {}: {e}",
                            jar_path.display()
                        )))
                        .await;
                    continue;
                }
            };

            // ── Spawn processor ───────────────────────────────────────────────
            let label = processor.jar.clone();
            let _ = event_tx
                .send(LaunchEvent::Patch(format!("[patcher] Running: {label}")))
                .await;

            let mut child = tokio::process::Command::new(config.java_path)
                .arg("-classpath")
                .arg(&classpath)
                .arg(&main_class)
                .args(&args)
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
                return Err(LoaderError::ProcessorFailed {
                    processor: label,
                    code: status.code(),
                });
            }
        }

        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Resolve a single processor argument, expanding `{PLACEHOLDER}` tokens.
    ///
    /// Resolution order:
    /// 1. If the stripped key matches an entry in `profile.data`:
    ///    - `BINPATCH` → absolute path to the `-clientdata.lzma` file.
    ///    - Anything else → `profile.data[key].client`.
    /// 2. Otherwise, replace known fixed tokens in the raw argument string:
    ///    `{SIDE}`, `{ROOT}`, `{MINECRAFT_JAR}`, `{MINECRAFT_VERSION}`,
    ///    `{INSTALLER}`, `{LIBRARY_DIR}`.
    ///
    /// Mirrors `setArgument()` from patcher.ts.
    fn set_argument(
        &self,
        arg: &str,
        profile: &ForgeProfile,
        config: &PatchConfig<'_>,
        neo_forge_old: bool,
    ) -> String {
        let key = strip_braces(arg);

        // Find the "universal" Forge JAR coordinate in profile.libraries.
        let universal_name: Option<&str> = profile.libraries.as_deref().and_then(|libs| {
            libs.iter()
                .find(|lib| match &self.loader_type {
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
                .map(|lib| lib.name.as_str())
        });

        if let Some(data) = &profile.data {
            if let Some(entry) = data.get(key) {
                if key == "BINPATCH" {
                    // Resolve the binary patch archive.
                    // Prefer profile.path, then install.path, then the universal lib name.
                    let coord = profile
                        .path
                        .as_deref()
                        .or_else(|| profile.install.as_ref().and_then(|i| i.path.as_deref()))
                        .or(universal_name)
                        .unwrap_or("");
                    if !coord.is_empty() {
                        if let Ok(info) = get_path_libraries(coord, None, None) {
                            let lzma_name = info.name.replace(".jar", "-clientdata.lzma");
                            let lzma_path = self
                                .loader_base
                                .join("libraries")
                                .join(&info.path)
                                .join(lzma_name);
                            return lzma_path.to_string_lossy().into_owned();
                        }
                    }
                    // Fallback: use the raw client value as-is.
                    return entry.client.clone();
                }
                return entry.client.clone();
            }
        }

        // Fixed placeholder substitutions (no quotes needed — Rust doesn't use shell).
        let libs_dir = self
            .loader_base
            .join("libraries")
            .to_string_lossy()
            .into_owned();
        let root_dir = config.game_path.to_string_lossy().into_owned();

        arg.replace("{SIDE}", "client")
            .replace("{ROOT}", &root_dir)
            .replace("{MINECRAFT_JAR}", config.minecraft_jar)
            .replace("{MINECRAFT_VERSION}", config.minecraft_json)
            .replace("{INSTALLER}", &libs_dir)
            .replace("{LIBRARY_DIR}", &libs_dir)
    }

    /// Convert a `[group:artifact:version]` reference to an absolute path.
    ///
    /// Arguments that don't start with `[` are returned unchanged.
    ///
    /// Mirrors `computePath()` from patcher.ts.
    fn compute_path(&self, arg: &str) -> String {
        if arg.starts_with('[') {
            let coord = arg.trim_matches(|c| c == '[' || c == ']');
            if let Ok(info) = get_path_libraries(coord, None, None) {
                return self
                    .loader_base
                    .join("libraries")
                    .join(&info.path)
                    .join(&info.name)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        arg.to_owned()
    }
}

// ── Module-level helpers ──────────────────────────────────────────────────────

/// Returns `true` if the processor should run on the client side.
///
/// A processor with no `sides` field runs on both; one that lists sides must
/// include `"client"` to be selected.
fn is_client_side(sides: Option<&[String]>) -> bool {
    match sides {
        None => true,
        Some(s) => s.iter().any(|side| side == "client"),
    }
}

/// Strip outer `{` / `}` from a placeholder token.
///
/// `{BINPATCH}` → `BINPATCH`. Non-placeholder args pass through unchanged.
///
/// Uses `trim_start_matches`/`trim_end_matches` (mirrors the TS single-char
/// `replace('{', '').replace('}', '')` for the common single-placeholder case).
fn strip_braces(s: &str) -> &str {
    s.trim_start_matches('{').trim_end_matches('}')
}

/// Read the `Main-Class` value from a JAR's `META-INF/MANIFEST.MF`.
///
/// Handles both CRLF and LF line endings (fixes the latent TS bug where
/// `split('\r\n')` silently included `\n` on Unix-created manifests).
///
/// Mirrors `readJarManifest()` from patcher.ts.
async fn read_jar_manifest(jar_path: &Path) -> Result<Option<String>, LoaderError> {
    let result = get_file_from_archive(
        jar_path.to_path_buf(),
        Some("META-INF/MANIFEST.MF".into()),
        None,
        false,
    )
    .await
    .map_err(|e| LoaderError::Archive(e.to_string()))?;

    let bytes = match result {
        ArchiveQueryResult::FileData(b) => b,
        _ => return Ok(None),
    };

    // str::lines() handles \r\n, \r, and \n — no CRLF/LF ambiguity.
    let content = String::from_utf8_lossy(&bytes);
    for line in content.lines() {
        if let Some(class) = line.strip_prefix("Main-Class: ") {
            return Ok(Some(class.trim().to_owned()));
        }
    }

    Ok(None)
}

/// Classpath separator: `;` on Windows, `:` everywhere else.
fn cp_separator() -> &'static str {
    if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    use crate::models::loader::{ForgeProcessor, ProfileDataEntry};

    fn make_patcher() -> ForgePatcher {
        ForgePatcher {
            loader_base: PathBuf::from("/mc/loader/forge"),
            loader_type: LoaderType::Forge,
        }
    }

    fn make_config<'a>(game_path: &'a Path) -> PatchConfig<'a> {
        PatchConfig {
            java_path: "/usr/bin/java",
            minecraft_jar: "/mc/versions/1.20.1/1.20.1.jar",
            minecraft_json: "/mc/versions/1.20.1/1.20.1.json",
            game_path,
        }
    }

    // ── strip_braces ──────────────────────────────────────────────────────────

    #[test]
    fn strip_braces_removes_outer() {
        assert_eq!(strip_braces("{BINPATCH}"), "BINPATCH");
        assert_eq!(strip_braces("{SIDE}"), "SIDE");
        assert_eq!(strip_braces("--no-braces"), "--no-braces");
    }

    // ── is_client_side ────────────────────────────────────────────────────────

    #[test]
    fn no_sides_means_client() {
        assert!(is_client_side(None));
    }

    #[test]
    fn client_in_sides_passes() {
        let sides = vec!["client".to_owned()];
        assert!(is_client_side(Some(&sides)));
    }

    #[test]
    fn server_only_filtered() {
        let sides = vec!["server".to_owned()];
        assert!(!is_client_side(Some(&sides)));
    }

    // ── compute_path ──────────────────────────────────────────────────────────

    #[test]
    fn compute_path_plain_arg_unchanged() {
        let p = make_patcher();
        assert_eq!(p.compute_path("--some-flag"), "--some-flag");
    }

    #[test]
    fn compute_path_bracket_coord_resolved() {
        let p = make_patcher();
        let result = p.compute_path("[net.minecraftforge:forge:1.20.1-47.4.20]");
        assert!(result.contains("net/minecraftforge/forge"));
        assert!(result.contains("forge-1.20.1-47.4.20.jar"));
        assert!(result.contains("/mc/loader/forge/libraries/"));
    }

    // ── set_argument ──────────────────────────────────────────────────────────

    #[test]
    fn set_argument_resolves_data_entry() {
        let p = make_patcher();
        let game = PathBuf::from("/mc");

        let mut data = HashMap::new();
        data.insert(
            "MAPPINGS".to_owned(),
            ProfileDataEntry {
                client: "[net.minecraftforge:forge:1.20.1-47.4.20:client-mappings@txt]".to_owned(),
                server: None,
            },
        );
        let profile = ForgeProfile {
            data: Some(data),
            ..Default::default()
        };
        let config = make_config(&game);
        let result = p.set_argument("{MAPPINGS}", &profile, &config, true);
        assert_eq!(
            result,
            "[net.minecraftforge:forge:1.20.1-47.4.20:client-mappings@txt]"
        );
    }

    #[test]
    fn set_argument_fixed_side_placeholder() {
        let p = make_patcher();
        let game = PathBuf::from("/mc");
        let config = make_config(&game);
        let result = p.set_argument("{SIDE}", &ForgeProfile::default(), &config, true);
        assert_eq!(result, "client");
    }

    #[test]
    fn set_argument_minecraft_jar_placeholder() {
        let p = make_patcher();
        let game = PathBuf::from("/mc");
        let config = make_config(&game);
        let result = p.set_argument("{MINECRAFT_JAR}", &ForgeProfile::default(), &config, true);
        assert_eq!(result, "/mc/versions/1.20.1/1.20.1.jar");
    }

    #[test]
    fn set_argument_library_dir_placeholder() {
        let p = make_patcher();
        let game = PathBuf::from("/mc");
        let config = make_config(&game);
        let result = p.set_argument("{LIBRARY_DIR}", &ForgeProfile::default(), &config, true);
        assert_eq!(result, "/mc/loader/forge/libraries");
    }

    // ── check ─────────────────────────────────────────────────────────────────

    #[test]
    fn check_returns_true_when_no_processors() {
        let p = make_patcher();
        let profile = ForgeProfile {
            processors: None,
            ..Default::default()
        };
        assert!(p.check(&profile));
    }

    #[test]
    fn check_returns_true_when_processors_empty() {
        let p = make_patcher();
        let profile = ForgeProfile {
            processors: Some(vec![]),
            ..Default::default()
        };
        assert!(p.check(&profile));
    }

    #[test]
    fn check_returns_false_when_output_file_missing() {
        let p = make_patcher();
        let mut data = HashMap::new();
        data.insert(
            "MC_SLIM".to_owned(),
            ProfileDataEntry {
                client: "[net.minecraftforge:forge:1.20.1-47.4.20:slim]".to_owned(),
                server: None,
            },
        );
        let profile = ForgeProfile {
            data: Some(data),
            processors: Some(vec![ForgeProcessor {
                jar: "cpw.mods:jarsplitter:1.1.4".to_owned(),
                classpath: vec![],
                args: vec!["{MC_SLIM}".to_owned()],
                sides: None,
            }]),
            ..Default::default()
        };
        // File doesn't exist on disk → check returns false.
        assert!(!p.check(&profile));
    }

    #[test]
    fn check_skips_server_side_processors() {
        let p = make_patcher();
        let mut data = HashMap::new();
        data.insert(
            "SERVER_EXTRA".to_owned(),
            ProfileDataEntry {
                client: "[some:artifact:1.0]".to_owned(),
                server: None,
            },
        );
        let profile = ForgeProfile {
            data: Some(data),
            processors: Some(vec![ForgeProcessor {
                jar: "some:tool:1.0".to_owned(),
                classpath: vec![],
                args: vec!["{SERVER_EXTRA}".to_owned()],
                // Server-only processor — skipped.
                sides: Some(vec!["server".to_owned()]),
            }]),
            ..Default::default()
        };
        // No client-side processors with tracked output → check returns true.
        assert!(p.check(&profile));
    }

    // ── read_jar_manifest ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_jar_manifest_finds_main_class() {
        use zip::write::SimpleFileOptions;

        let mut tmp = NamedTempFile::new().unwrap();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"Manifest-Version: 1.0\r\nMain-Class: com.example.Main\r\n")
                .unwrap();
            let data = w.finish().unwrap();
            tmp.write_all(data.get_ref()).unwrap();
        }

        let result = read_jar_manifest(tmp.path()).await.unwrap();
        assert_eq!(result, Some("com.example.Main".to_owned()));
    }

    #[tokio::test]
    async fn read_jar_manifest_lf_line_endings() {
        use zip::write::SimpleFileOptions;

        let mut tmp = NamedTempFile::new().unwrap();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            // LF only (the bug in TS) — Rust handles fine with str::lines().
            w.write_all(b"Manifest-Version: 1.0\nMain-Class: com.example.Main\n")
                .unwrap();
            let data = w.finish().unwrap();
            tmp.write_all(data.get_ref()).unwrap();
        }

        let result = read_jar_manifest(tmp.path()).await.unwrap();
        assert_eq!(result, Some("com.example.Main".to_owned()));
    }

    #[tokio::test]
    async fn read_jar_manifest_returns_none_when_no_main_class() {
        use zip::write::SimpleFileOptions;

        let mut tmp = NamedTempFile::new().unwrap();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"Manifest-Version: 1.0\r\n").unwrap();
            let data = w.finish().unwrap();
            tmp.write_all(data.get_ref()).unwrap();
        }

        let result = read_jar_manifest(tmp.path()).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn read_jar_manifest_returns_none_when_no_manifest() {
        use zip::write::SimpleFileOptions;

        let mut tmp = NamedTempFile::new().unwrap();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            w.start_file("some/other/file.txt", opts).unwrap();
            w.write_all(b"hello").unwrap();
            let data = w.finish().unwrap();
            tmp.write_all(data.get_ref()).unwrap();
        }

        let result = read_jar_manifest(tmp.path()).await.unwrap();
        assert_eq!(result, None);
    }
}
