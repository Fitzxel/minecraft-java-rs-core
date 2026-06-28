use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::launcher::options::LaunchOptions;
use crate::models::loader::LoaderType;
use crate::models::minecraft::{AssetItem, GameArgEntry, MinecraftVersionJson};
use crate::game::libraries::natives_base_dir;

// ── Loader context ────────────────────────────────────────────────────────────

/// Context contributed by the active mod loader for argument building.
/// Constructed in `launcher/mod.rs` from the stored `GameData` fields.
pub struct LoaderContext<'a> {
    /// Which loader was installed (used for Forge-specific JVM flags).
    pub loader_type: Option<&'a LoaderType>,
    /// Loader version id (e.g. `"1.20.4-forge-47.4.20"`). Used for `${version_name}`.
    pub version_id: Option<&'a str>,
    /// Extra plain-string game args from the loader JSON, merged after vanilla args.
    pub extra_game_args: &'a [String],
    /// Extra JVM args from the loader version JSON (`arguments.jvm`), pre-resolved.
    pub extra_jvm_args: &'a [String],
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build vanilla + loader game arguments (everything after the main class).
///
/// Handles both the legacy `minecraftArguments` string (pre-1.13) and the
/// modern `arguments.game` array (1.13+). Conditional entries in the modern
/// format are skipped. Extra args from `options.game_args` are appended last.
///
/// When `loader` is provided:
/// - `${version_name}` uses the loader version id.
/// - Loader's own game args (`extra_game_args`) are merged in (deduped).
pub fn get_game_arguments(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
    loader: Option<&LoaderContext<'_>>,
) -> Vec<String> {
    let ph = build_game_placeholders(options, version_json, loader);
    let mut args: Vec<String> = Vec::new();

    if let Some(raw) = &version_json.minecraft_arguments {
        // Legacy: space-separated string.
        for token in raw.split_whitespace() {
            args.push(replace_placeholders(token, &ph));
        }
    } else if let Some(arguments) = &version_json.arguments {
        if let Some(game) = &arguments.game {
            for entry in game {
                if let GameArgEntry::Plain(s) = entry {
                    args.push(replace_placeholders(s, &ph));
                }
                // Conditional entries skipped (added via screen options in launcher/mod.rs).
            }
        }
    }

    // Merge loader's extra game args (e.g. `--launchTarget fmlclient`), deduped.
    if let Some(ctx) = loader {
        for arg in ctx.extra_game_args {
            let resolved = replace_placeholders(arg, &ph);
            if !args.contains(&resolved) {
                args.push(resolved);
            }
        }
    }

    for extra in &options.game_args {
        args.push(replace_placeholders(extra, &ph));
    }

    args
}

/// Build JVM arguments (everything before the main class, excluding `-cp`).
///
/// Sources (in order):
/// 1. Memory flags `-Xms` / `-Xmx`.
/// 2. G1GC performance tuning (standard Minecraft launcher defaults).
/// 3. Forge/NeoForge specific flags when loader is present.
/// 4. OS-specific flags for modern MC (no `minecraftArguments`).
/// 5. Native library paths (`-Djava.library.path`, jna, lwjgl, netty).
/// 6. Modern JVM args from `version_json.arguments.jvm` (skipping `-cp`/`${classpath}`).
/// 7. Offline-bypass system properties when `options.bypass_offline` is true.
/// 8. Extra flags from `options.jvm_args`.
pub fn get_jvm_arguments(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
    natives_path: &Path,
    loader: Option<&LoaderContext<'_>>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let natives_str = natives_path.to_string_lossy().into_owned();

    // 1. Memory.
    args.push(format!("-Xms{}", options.memory.min));
    args.push(format!("-Xmx{}", options.memory.max));

    // 2. G1GC tuning — standard across all major Minecraft launchers.
    args.push("-XX:+UnlockExperimentalVMOptions".into());
    args.push("-XX:G1NewSizePercent=20".into());
    args.push("-XX:G1ReservePercent=20".into());
    args.push("-XX:MaxGCPauseMillis=50".into());
    args.push("-XX:G1HeapRegionSize=32M".into());

    // 3. Forge / NeoForge specific flags.
    if let Some(ctx) = loader {
        if matches!(
            ctx.loader_type,
            Some(LoaderType::Forge) | Some(LoaderType::NeoForge)
        ) {
            args.push("-Dfml.ignoreInvalidMinecraftCertificates=true".into());
            args.push("-Dfml.ignorePatchDiscrepancies=true".into());
        }
    }

    // 4. OS-specific flags — only for modern MC that uses `arguments.jvm`.
    //    Legacy MC (minecraftArguments) ships its own native loader that handles this.
    if version_json.minecraft_arguments.is_none() {
        match std::env::consts::OS {
            // macOS requires first-thread init for OpenGL (LWJGL).
            "macos" => args.push("-XstartOnFirstThread".into()),
            // Linux default stack is too small for some MC versions.
            "linux" => args.push("-Xss1M".into()),
            // Windows-only heap dump path required by some Mojang driver workarounds.
            "windows" => args.push(
                "-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump".into()
            ),
            _ => {}
        }
    }

    // 5. Native library directories.
    args.push(format!("-Djava.library.path={natives_str}"));
    args.push(format!("-Djna.tmpdir={natives_str}"));
    args.push(format!(
        "-Dorg.lwjgl.system.SharedLibraryExtractPath={natives_str}"
    ));
    args.push(format!("-Dio.netty.native.workdir={natives_str}"));

    // 6. Modern JVM args from the version JSON.
    //
    // `${natives_directory}` is the **base** natives dir
    // (`<path>/versions/<id>/natives`), NOT the per-version subdir
    // returned by `natives_dir_for`. The JSON templates append a suffix
    // like `/java` (MC 26.x / LWJGL 3.4) or `/jna`, `/lwjgl`, `/netty`,
    // so the base is the right anchor — if we substituted the final
    // path here, `-Djava.library.path=${natives_directory}/java` would
    // expand to `<base>/java/java` and the JVM would search a
    // non-existent directory.
    if let Some(arguments) = &version_json.arguments {
        if let Some(jvm_entries) = &arguments.jvm {
            let natives_base_str = natives_base_dir(options, version_json)
                .to_string_lossy()
                .into_owned();
            let ph = build_jvm_placeholders(
                options,
                version_json,
                &natives_base_str,
                "",
            );

            let mut skip_next = false;
            for val in jvm_entries {
                if skip_next {
                    skip_next = false;
                    continue;
                }

                if let Some(s) = val.as_str() {
                    if s == "-cp" || s == "--classpath" {
                        skip_next = true;
                        continue;
                    }
                    if s.contains("${classpath}") {
                        continue;
                    }
                    args.push(replace_placeholders(s, &ph));
                } else if val.is_object() {
                    if jvm_rule_passes(val) {
                        for token in extract_jvm_value(val) {
                            if token == "-cp"
                                || token == "--classpath"
                                || token.contains("${classpath}")
                            {
                                continue;
                            }
                            args.push(replace_placeholders(&token, &ph));
                        }
                    }
                }
            }
        }
    }

    // 7. Offline bypass: redirect Mojang auth endpoints.
    if options.bypass_offline {
        args.push("-Dminecraft.api.auth.host=https://invalidAuthServer.invalid".into());
        args.push("-Dminecraft.api.account.host=https://invalidAccountServer.invalid".into());
        args.push("-Dminecraft.api.session.host=https://invalidSessionServer.invalid".into());
        args.push("-Dminecraft.api.services.host=https://invalidServicesServer.invalid".into());
    }

    // 8. User-supplied extra JVM args.
    for extra in &options.jvm_args {
        args.push(extra.clone());
    }

    // 9. Loader JVM args (pre-resolved; e.g. Forge --add-opens, -p module path).
    if let Some(ctx) = loader {
        for arg in ctx.extra_jvm_args {
            args.push(arg.clone());
        }
    }

    args
}

/// Build the classpath argument pair and return the main class name.
///
/// Returns `(["-cp", "<path1>:<path2>:…"], "net.minecraft.client.main.Main")`.
///
/// Library JARs are deduplicated by Maven artifact name: when two JARs share
/// the same artifact name with different versions, only the highest version is
/// kept. Insertion order is preserved (loader libs come first when the bundle
/// is built with loader-first ordering in `launcher/mod.rs`).
///
/// If both `log4j-slf4j-impl` and `log4j-slf4j2-impl` are present, the older
/// SLF4J 1.x binding is removed to prevent duplicate SLF4J binding warnings.
pub fn get_classpath(
    version_json: &MinecraftVersionJson,
    bundle: &[AssetItem],
) -> (Vec<String>, String) {
    let jar_paths: Vec<PathBuf> = bundle
        .iter()
        .filter_map(|item| match item {
            AssetItem::Asset { path, .. } => {
                if path.ends_with(".jar") && !path.contains("/assets/objects/") {
                    Some(PathBuf::from(path))
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    let mut deduped = deduplicate_classpath(jar_paths);

    // Resolve SLF4J binding conflict: if the loader ships slf4j2-impl, drop the
    // vanilla slf4j-impl (1.x) to avoid "multiple SLF4J bindings" warnings/crashes.
    let has_slf4j2 = deduped.iter().any(|p| {
        p.file_name()
            .map_or(false, |f| f.to_string_lossy().contains("log4j-slf4j2-impl"))
    });
    if has_slf4j2 {
        deduped.retain(|p| {
            let name = p
                .file_name()
                .map_or(String::new(), |f| f.to_string_lossy().into_owned());
            // Keep everything except the SLF4J 1.x binding.
            !name.contains("log4j-slf4j-impl") || name.contains("log4j-slf4j2-impl")
        });
    }

    let sep = classpath_separator();
    let cp = deduped
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep);

    let main_class = version_json.main_class.clone().unwrap_or_default();
    (vec!["-cp".into(), cp], main_class)
}

// ── Placeholder helpers ───────────────────────────────────────────────────────

fn build_game_placeholders<'a>(
    options: &'a LaunchOptions,
    version_json: &'a MinecraftVersionJson,
    loader: Option<&LoaderContext<'_>>,
) -> HashMap<&'a str, String> {
    let auth = &options.authenticator;

    let assets_id = version_json
        .asset_index
        .as_ref()
        .map(|ai| ai.id.clone())
        .or_else(|| version_json.assets.clone())
        .unwrap_or_default();

    // 1.16.x requires the literal string "Xbox" for online play.
    let user_type = if version_json.id.starts_with("1.16") {
        "Xbox".to_string()
    } else if auth.xbox_account.is_some() {
        "msa".to_string()
    } else {
        "legacy".to_string()
    };

    // Loader version id overrides vanilla id so Forge profiles report correctly.
    let version_name = loader
        .and_then(|ctx| ctx.version_id)
        .unwrap_or(version_json.id.as_str())
        .to_owned();

    // auth_xuid falls back to access_token (matches TS behavior).
    let auth_xuid = auth
        .xbox_account
        .as_ref()
        .map(|x| x.xuid.clone())
        .unwrap_or_else(|| auth.access_token.clone());

    // clientid: clientId → client_token → access_token (matches TS triple fallback).
    let clientid = auth
        .client_id
        .clone()
        .or_else(|| auth.client_token.clone())
        .unwrap_or_else(|| auth.access_token.clone());

    let game_directory = options.save_dir().to_string_lossy().into_owned();

    // Legacy (pre-1.7.10) assets live under resources/ instead of assets/.
    let is_legacy = matches!(
        version_json.assets.as_deref(),
        Some("legacy") | Some("pre-1.6")
    );
    let assets_root = if is_legacy {
        options
            .path
            .join("resources")
            .to_string_lossy()
            .into_owned()
    } else {
        options.path.join("assets").to_string_lossy().into_owned()
    };

    let mut ph: HashMap<&str, String> = HashMap::new();
    ph.insert("auth_player_name", auth.name.clone());
    ph.insert("version_name", version_name);
    ph.insert("game_directory", game_directory);
    ph.insert("assets_root", assets_root.clone());
    ph.insert("game_assets", assets_root); // legacy alias
    ph.insert("assets_index_name", assets_id);
    ph.insert("auth_uuid", auth.uuid.clone());
    ph.insert("auth_access_token", auth.access_token.clone());
    ph.insert("auth_session", auth.access_token.clone()); // legacy alias
    ph.insert("auth_xuid", auth_xuid);
    ph.insert("user_type", user_type);
    ph.insert("version_type", version_json.version_type.clone());
    ph.insert(
        "user_properties",
        auth.user_properties.clone().unwrap_or_else(|| "{}".into()),
    );
    ph.insert("clientid", clientid);
    ph
}

/// Build the placeholder map used to expand `${...}` tokens in the version
/// JSON's `arguments.jvm` entries.
///
/// `natives_str` must be the **base** natives directory
/// (`<path>/versions/<id>/natives`), not the per-version subdir returned by
/// `natives_dir_for`. The JSON templates append a suffix themselves
/// (e.g. `/java` for MC 26.x, `/jna`, `/lwjgl`, `/netty`), so the base is
/// the correct anchor for `${natives_directory}`.
fn build_jvm_placeholders<'a>(
    options: &'a LaunchOptions,
    _version_json: &'a MinecraftVersionJson,
    natives_str: &'a str,
    classpath: &'a str,
) -> HashMap<&'a str, String> {
    let mut ph: HashMap<&str, String> = HashMap::new();
    ph.insert("natives_directory", natives_str.to_owned());
    ph.insert("launcher_name", "minecraft-java-rs-core".into());
    ph.insert("launcher_version", env!("CARGO_PKG_VERSION").into());
    ph.insert("classpath_separator", classpath_separator().to_string());
    ph.insert("classpath", classpath.to_owned());
    ph.insert(
        "library_directory",
        options
            .path
            .join("libraries")
            .to_string_lossy()
            .into_owned(),
    );
    ph
}

fn replace_placeholders(s: &str, ph: &HashMap<&str, String>) -> String {
    let mut result = s.to_owned();
    for (key, val) in ph {
        result = result.replace(&format!("${{{key}}}"), val);
    }
    result
}

// ── JVM arg rule evaluation ───────────────────────────────────────────────────

fn jvm_rule_passes(val: &serde_json::Value) -> bool {
    let rules = match val.get("rules").and_then(|r| r.as_array()) {
        Some(r) => r,
        None => return true,
    };

    let os_name = std::env::consts::OS;
    let mojang_os = match os_name {
        "macos" => "osx",
        "windows" => "windows",
        "linux" => "linux",
        other => other,
    };

    let mut result = false;
    for rule in rules {
        let action = rule
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("disallow");
        let allow = action == "allow";

        if let Some(os) = rule.get("os") {
            let name_matches = os
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| n == mojang_os)
                .unwrap_or(true);

            if name_matches {
                result = allow;
            }
        } else {
            result = allow;
        }
    }

    result
}

fn extract_jvm_value(val: &serde_json::Value) -> Vec<String> {
    match val.get("value") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

// ── Classpath helpers ─────────────────────────────────────────────────────────

pub fn classpath_separator() -> &'static str {
    if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    }
}

/// Keep only the highest-version JAR for each Maven artifact name.
///
/// Deduplication key is the artifact directory (grandparent of the JAR file).
/// Insertion order is preserved: the first time an artifact is seen determines
/// its position in the output. When a higher-version JAR is found later, only
/// the path is updated in-place — the position stays with the first insertion.
/// This ensures loader-first ordering is preserved after deduplication.
fn deduplicate_classpath(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    // key → (best_version, best_path)
    let mut entries: HashMap<String, (String, PathBuf)> = HashMap::new();
    // Maintains insertion order of keys (first time each key is seen).
    let mut key_order: Vec<String> = Vec::new();

    for path in paths {
        let components: Vec<_> = path.components().collect();
        let n = components.len();

        let (artifact_key, version_dir) = if n >= 3 {
            let version = components[n - 2].as_os_str().to_string_lossy().into_owned();
            let artifact = components[n - 3].as_os_str().to_string_lossy().into_owned();
            // Include Maven classifier in the key so that e.g. forge-client.jar and
            // forge-universal.jar (same artifact dir, same version, different classifier)
            // are treated as distinct entries rather than de-duplicated against each other.
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let base = format!("{artifact}-{version}");
            let key = if stem.starts_with(&format!("{base}-")) {
                let classifier = &stem[base.len() + 1..];
                format!("{artifact}-{classifier}")
            } else {
                artifact
            };
            (key, version)
        } else {
            // Non-Maven path (e.g. client JAR) — use full path as key.
            (path.to_string_lossy().into_owned(), String::new())
        };

        if let Some((existing_ver, existing_path)) = entries.get_mut(&artifact_key) {
            // Already seen — keep whichever version is higher.
            if version_is_higher(&version_dir, existing_ver) {
                *existing_ver = version_dir;
                *existing_path = path;
            }
        } else {
            key_order.push(artifact_key.clone());
            entries.insert(artifact_key, (version_dir, path));
        }
    }

    key_order
        .into_iter()
        .filter_map(|k| entries.remove(&k).map(|(_, p)| p))
        .collect()
}

fn version_is_higher(a: &str, b: &str) -> bool {
    if let (Ok(va), Ok(vb)) = (semver::Version::parse(a), semver::Version::parse(b)) {
        return va > vb;
    }

    // Dot/dash split numeric fallback (handles "32.1.2-jre", "1.10", etc.).
    let parse_parts = |s: &str| -> Vec<u64> {
        s.split(|c: char| c == '.' || c == '-')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };

    parse_parts(a) > parse_parts(b)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_opts() -> LaunchOptions {
        use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
        use crate::models::minecraft::Authenticator;
        LaunchOptions {
            path: PathBuf::from("/mc"),
            version: "1.20.4".into(),
            authenticator: Authenticator {
                access_token: "token123".into(),
                name: "Steve".into(),
                uuid: "uuid-1234".into(),
                xbox_account: None,
                user_properties: None,
                client_id: None,
                client_token: None,
            },
            timeout_secs: 10,
            download_concurrency: 5,
            verify_concurrency: 4,
            memory: MemoryConfig {
                min: "512M".into(),
                max: "4G".into(),
            },
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
            assets: Some("17".into()),
            asset_index: None,
            downloads: None,
            libraries: vec![],
            arguments: None,
            minecraft_arguments: None,
            java_version: None,
            main_class: Some("net.minecraft.client.main.Main".into()),
            has_natives: false,
        }
    }

    // ── game args ─────────────────────────────────────────────────────────────

    #[test]
    fn legacy_game_args_split_and_replace() {
        let opts = make_opts();
        let mut vj = bare_version();
        vj.minecraft_arguments =
            Some("--username ${auth_player_name} --version ${version_name}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "--username");
        assert_eq!(args[1], "Steve");
        assert_eq!(args[2], "--version");
        assert_eq!(args[3], "1.20.4");
    }

    #[test]
    fn modern_game_args_plain_strings_only() {
        use crate::models::minecraft::Arguments;
        let opts = make_opts();
        let mut vj = bare_version();
        vj.arguments = Some(Arguments {
            game: Some(vec![
                GameArgEntry::Plain("--username".into()),
                GameArgEntry::Plain("${auth_player_name}".into()),
                GameArgEntry::Conditional(serde_json::json!({"rules": [], "value": "--demo"})),
            ]),
            jvm: None,
        });
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--username");
        assert_eq!(args[1], "Steve");
    }

    #[test]
    fn extra_game_args_appended() {
        let mut opts = make_opts();
        opts.game_args = vec!["--demo".into()];
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("--username ${auth_player_name}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args.last().unwrap(), "--demo");
    }

    #[test]
    fn user_type_is_msa_when_xbox_account_present() {
        use crate::models::minecraft::XboxAccount;
        let mut opts = make_opts();
        opts.authenticator.xbox_account = Some(XboxAccount {
            xuid: "x123".into(),
        });
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("${user_type}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "msa");
    }

    #[test]
    fn user_type_is_xbox_on_116() {
        let opts = make_opts();
        let mut vj = bare_version();
        vj.id = "1.16.5".into();
        vj.minecraft_arguments = Some("${user_type}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "Xbox");
    }

    #[test]
    fn loader_version_id_overrides_version_name() {
        let opts = make_opts();
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("${version_name}".into());
        let ctx = LoaderContext {
            loader_type: Some(&LoaderType::Forge),
            version_id: Some("1.20.4-forge-47.4.20"),
            extra_game_args: &[],
            extra_jvm_args: &[],
        };
        let args = get_game_arguments(&opts, &vj, Some(&ctx));
        assert_eq!(args[0], "1.20.4-forge-47.4.20");
    }

    #[test]
    fn loader_extra_game_args_merged_deduped() {
        let opts = make_opts();
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("--username ${auth_player_name}".into());
        let extra = vec![
            "--launchTarget".into(),
            "fmlclient".into(),
            "--username".into(),
        ];
        let ctx = LoaderContext {
            loader_type: Some(&LoaderType::Forge),
            version_id: None,
            extra_game_args: &extra,
            extra_jvm_args: &[],
        };
        let args = get_game_arguments(&opts, &vj, Some(&ctx));
        // "--username" from vanilla should not be duplicated
        let username_count = args.iter().filter(|a| *a == "--username").count();
        assert_eq!(username_count, 1);
        assert!(args.contains(&"--launchTarget".to_string()));
        assert!(args.contains(&"fmlclient".to_string()));
    }

    #[test]
    fn auth_session_placeholder_resolved() {
        let opts = make_opts();
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("${auth_session}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "token123");
    }

    #[test]
    fn clientid_falls_back_to_client_token() {
        let mut opts = make_opts();
        opts.authenticator.client_token = Some("ct-abc".into());
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("${clientid}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "ct-abc");
    }

    #[test]
    fn clientid_falls_back_to_access_token() {
        let opts = make_opts(); // no client_id or client_token
        let mut vj = bare_version();
        vj.minecraft_arguments = Some("${clientid}".into());
        let args = get_game_arguments(&opts, &vj, None);
        assert_eq!(args[0], "token123");
    }

    // ── jvm args ──────────────────────────────────────────────────────────────

    #[test]
    fn jvm_args_contain_memory_and_natives() {
        let opts = make_opts();
        let vj = bare_version();
        let natives = PathBuf::from("/mc/versions/1.20.4/natives");
        let args = get_jvm_arguments(&opts, &vj, &natives, None);
        assert!(args.contains(&"-Xms512M".to_string()));
        assert!(args.contains(&"-Xmx4G".to_string()));
        assert!(args.iter().any(|a| a.contains("-Djava.library.path=")));
    }

    #[test]
    fn jvm_args_contain_gc_flags() {
        let opts = make_opts();
        let args = get_jvm_arguments(&opts, &bare_version(), Path::new("/n"), None);
        assert!(args.contains(&"-XX:+UnlockExperimentalVMOptions".to_string()));
        assert!(args.contains(&"-XX:G1NewSizePercent=20".to_string()));
        assert!(args.contains(&"-XX:MaxGCPauseMillis=50".to_string()));
    }

    #[test]
    fn jvm_args_contain_jna_dirs() {
        let opts = make_opts();
        let natives = Path::new("/natives");
        let args = get_jvm_arguments(&opts, &bare_version(), natives, None);
        assert!(args.iter().any(|a| a.starts_with("-Djna.tmpdir=")));
        assert!(args
            .iter()
            .any(|a| a.starts_with("-Dorg.lwjgl.system.SharedLibraryExtractPath=")));
        assert!(args
            .iter()
            .any(|a| a.starts_with("-Dio.netty.native.workdir=")));
    }

    #[test]
    fn jvm_args_forge_adds_fml_flags() {
        let opts = make_opts();
        let ctx = LoaderContext {
            loader_type: Some(&LoaderType::Forge),
            version_id: None,
            extra_game_args: &[],
            extra_jvm_args: &[],
        };
        let args = get_jvm_arguments(&opts, &bare_version(), Path::new("/n"), Some(&ctx));
        assert!(args.contains(&"-Dfml.ignoreInvalidMinecraftCertificates=true".to_string()));
        assert!(args.contains(&"-Dfml.ignorePatchDiscrepancies=true".to_string()));
    }

    #[test]
    fn jvm_args_fabric_no_fml_flags() {
        let opts = make_opts();
        let ctx = LoaderContext {
            loader_type: Some(&LoaderType::Fabric),
            version_id: None,
            extra_game_args: &[],
            extra_jvm_args: &[],
        };
        let args = get_jvm_arguments(&opts, &bare_version(), Path::new("/n"), Some(&ctx));
        assert!(!args.iter().any(|a| a.contains("fml")));
    }

    #[test]
    fn bypass_offline_adds_sys_properties() {
        let mut opts = make_opts();
        opts.bypass_offline = true;
        let args = get_jvm_arguments(&opts, &bare_version(), Path::new("/n"), None);
        assert!(args.iter().any(|a| a.contains("invalidAuthServer")));
    }

    #[test]
    fn jvm_args_no_classpath_entry() {
        let opts = make_opts();
        let args = get_jvm_arguments(&opts, &bare_version(), Path::new("/n"), None);
        assert!(!args.iter().any(|a| a == "-cp" || a == "--classpath"));
    }

    // ── classpath ─────────────────────────────────────────────────────────────

    #[test]
    fn classpath_contains_jar_paths() {
        let vj = bare_version();
        let bundle = vec![
            AssetItem::Asset {
                path: "/mc/libraries/net/sf/jopt-simple/jopt-simple/5.0.4/jopt-simple-5.0.4.jar"
                    .into(),
                sha1: "aaa".into(),
                size: 100,
                url: "http://x".into(),
            },
            AssetItem::Asset {
                path: "/mc/assets/objects/aa/aabbcc".into(),
                sha1: "bbb".into(),
                size: 10,
                url: "http://y".into(),
            },
        ];
        let (cp_args, main) = get_classpath(&vj, &bundle);
        assert_eq!(cp_args[0], "-cp");
        let cp = &cp_args[1];
        assert!(cp.contains("jopt-simple-5.0.4.jar"));
        assert!(!cp.contains("aabbcc"));
        assert_eq!(main, "net.minecraft.client.main.Main");
    }

    #[test]
    fn classpath_deduplicates_lower_version() {
        let vj = bare_version();
        let bundle = vec![
            AssetItem::Asset {
                path: "/mc/libraries/com/google/guava/guava/21.0/guava-21.0.jar".into(),
                sha1: "a".into(),
                size: 1,
                url: "http://x".into(),
            },
            AssetItem::Asset {
                path: "/mc/libraries/com/google/guava/guava/32.1.2/guava-32.1.2.jar".into(),
                sha1: "b".into(),
                size: 2,
                url: "http://x".into(),
            },
        ];
        let (cp_args, _) = get_classpath(&vj, &bundle);
        let cp = &cp_args[1];
        assert!(cp.contains("32.1.2"), "should keep higher version: {cp}");
        assert!(!cp.contains("21.0"), "should drop lower version: {cp}");
    }

    #[test]
    fn classpath_preserves_loader_first_order() {
        let vj = bare_version();
        // Simulate: loader lib first, vanilla lib second.
        let bundle = vec![
            AssetItem::Asset {
                path: "/loader/forge/libraries/net/minecraftforge/forge/1.0/forge-1.0.jar".into(),
                sha1: "a".into(),
                size: 1,
                url: "http://x".into(),
            },
            AssetItem::Asset {
                path: "/mc/libraries/org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1.jar".into(),
                sha1: "b".into(),
                size: 2,
                url: "http://x".into(),
            },
        ];
        let (cp_args, _) = get_classpath(&vj, &bundle);
        let cp = &cp_args[1];
        let forge_pos = cp.find("forge-1.0.jar").unwrap();
        let lwjgl_pos = cp.find("lwjgl-3.3.1.jar").unwrap();
        assert!(
            forge_pos < lwjgl_pos,
            "loader lib should come before vanilla lib"
        );
    }

    #[test]
    fn classpath_removes_slf4j1_when_slf4j2_present() {
        let vj = bare_version();
        let bundle = vec![
            AssetItem::Asset {
                path: "/loader/forge/libraries/log4j/log4j-slf4j2-impl/18.0/log4j-slf4j2-impl-18.0.jar".into(),
                sha1: "a".into(),
                size: 1,
                url: "http://x".into(),
            },
            AssetItem::Asset {
                path: "/mc/libraries/log4j/log4j-slf4j-impl/18.0/log4j-slf4j-impl-18.0.jar".into(),
                sha1: "b".into(),
                size: 2,
                url: "http://x".into(),
            },
        ];
        let (cp_args, _) = get_classpath(&vj, &bundle);
        let cp = &cp_args[1];
        assert!(
            cp.contains("log4j-slf4j2-impl"),
            "should keep slf4j2 binding: {cp}"
        );
        assert!(
            !cp.contains("log4j-slf4j-impl-18"),
            "should drop slf4j1 binding: {cp}"
        );
    }

    #[test]
    fn classpath_keeps_both_classifiers_in_same_version_dir() {
        let vj = bare_version();
        let bundle = vec![
            AssetItem::Asset {
                path: "/loader/forge/libraries/net/minecraftforge/forge/26.1.2-64.0.8/forge-26.1.2-64.0.8-universal.jar".into(),
                sha1: "a".into(),
                size: 1,
                url: "http://x".into(),
            },
            AssetItem::Asset {
                path: "/loader/forge/libraries/net/minecraftforge/forge/26.1.2-64.0.8/forge-26.1.2-64.0.8-client.jar".into(),
                sha1: "b".into(),
                size: 2,
                url: "http://x".into(),
            },
        ];
        let (cp_args, _) = get_classpath(&vj, &bundle);
        let cp = &cp_args[1];
        assert!(
            cp.contains("forge-26.1.2-64.0.8-universal.jar"),
            "universal must be kept: {cp}"
        );
        assert!(
            cp.contains("forge-26.1.2-64.0.8-client.jar"),
            "client must be kept: {cp}"
        );
    }

    #[test]
    fn classpath_separator_is_colon_on_non_windows() {
        assert_eq!(classpath_separator(), ":");
    }

    // ── version_is_higher ─────────────────────────────────────────────────────

    #[test]
    fn higher_semver_wins() {
        assert!(version_is_higher("2.0.0", "1.9.9"));
        assert!(!version_is_higher("1.0.0", "1.0.0"));
    }

    #[test]
    fn numeric_dot_split_fallback() {
        assert!(version_is_higher("1.10.0", "1.9.0"));
        assert!(!version_is_higher("1.9.0", "1.10.0"));
    }

    // ── ${natives_directory} placeholder expansion ────────────────────────────

    /// Regression: discovered via real launch of MC 26.2 on Linux.
    /// Before this fix, `${natives_directory}` was expanded to the **final**
    /// natives path returned by `natives_dir_for` (e.g. `…/natives/java`).
    /// The 26.x JSON template is `-Djava.library.path=${natives_directory}/java`,
    /// so the substitution produced `…/natives/java/java` and the JVM crashed
    /// with "Failed to locate library: liblwjgl.so" (the second `-D` wins
    /// and the resulting path doesn't exist).
    ///
    /// The placeholder must expand to the **base** path
    /// (`<path>/versions/<id>/natives`); the JSON template's own `/java`
    /// suffix then points at the real extraction target.
    #[test]
    fn natives_directory_placeholder_expands_to_base_on_modern_versions() {
        use crate::game::libraries::natives_dir_for;
        use crate::models::minecraft::Arguments;
        let opts = make_opts();
        let mut vj = bare_version();
        vj.id = "26.2".into();
        // MC 26.x JVM args (simplified to the natives-related entries).
        vj.arguments = Some(Arguments {
            game: None,
            jvm: Some(vec![
                serde_json::Value::String(
                    "-Djava.library.path=${natives_directory}/java".into(),
                ),
                serde_json::Value::String("-Djna.tmpdir=${natives_directory}/jna".into()),
                serde_json::Value::String(
                    "-Dorg.lwjgl.system.SharedLibraryExtractPath=${natives_directory}/lwjgl"
                        .into(),
                ),
                serde_json::Value::String(
                    "-Dio.netty.native.workdir=${natives_directory}/netty".into(),
                ),
            ]),
        });

        let natives_path = natives_dir_for(&opts, &vj);
        let args = get_jvm_arguments(&opts, &vj, &natives_path, None);

        // The launcher's section 5 still adds its own -Djava.library.path
        // pointing at the final path. Both must agree on the same dir.
        let java_lib_path_values: Vec<&str> = args
            .iter()
            .filter_map(|a| a.strip_prefix("-Djava.library.path="))
            .collect();
        assert_eq!(
            java_lib_path_values.len(),
            2,
            "expected 2 -Djava.library.path (launcher + JSON), got {java_lib_path_values:?}"
        );
        for v in &java_lib_path_values {
            assert!(
                v.ends_with("/natives/java"),
                "every -Djava.library.path must end in /natives/java, got {v:?}"
            );
            assert!(
                !v.ends_with("/natives/java/java"),
                "DOUBLE /java BUG: {v:?} — placeholder was expanded to the final path"
            );
        }
        // Both values should be byte-identical.
        assert_eq!(
            java_lib_path_values[0], java_lib_path_values[1],
            "launcher and JSON -Djava.library.path must point to the same dir"
        );

        // The tmpdir / extract / workdir flags are added by both the
        // launcher (section 5, points at the final path) and the JSON
        // (section 6, points at the sibling dir). The JVM honours the
        // last occurrence, so we check **all** of them and assert that
        // at least one of them is the correct sibling path. Before this
        // fix, the JSON-substituted value was `…/natives/java/jna`
        // (the placeholder expanded to the final path, then `/jna` was
        // appended on top), so neither was the right sibling.
        let last = |prefix: &str| -> Option<&str> {
            args.iter()
                .rev()
                .find_map(|a| a.strip_prefix(prefix))
        };
        let jna = last("-Djna.tmpdir=").expect("missing -Djna.tmpdir");
        let lwjgl =
            last("-Dorg.lwjgl.system.SharedLibraryExtractPath=")
                .expect("missing SharedLibraryExtractPath");
        let netty =
            last("-Dio.netty.native.workdir=").expect("missing netty workdir");
        assert!(
            jna.ends_with("/natives/jna"),
            "last -Djna.tmpdir must end in /natives/jna, got {jna:?}"
        );
        assert!(
            lwjgl.ends_with("/natives/lwjgl"),
            "last SharedLibraryExtractPath must end in /natives/lwjgl, got {lwjgl:?}"
        );
        assert!(
            netty.ends_with("/natives/netty"),
            "last -Dio.netty.native.workdir must end in /natives/netty, got {netty:?}"
        );
    }
}
