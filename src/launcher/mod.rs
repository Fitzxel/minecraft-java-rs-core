pub mod events;
pub mod game_data;
pub mod options;

pub use events::LaunchEvent;

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

use crate::error::LaunchError;
use crate::game::{
    arguments::{get_classpath, get_game_arguments, get_jvm_arguments, LoaderContext},
    assets::{copy_assets, get_assets},
    bundle::{check_bundle, check_files},
    java::get_java_files,
    libraries::{extract_natives, get_assets_others, get_libraries},
    version::get_version_json,
};
use crate::launcher::game_data::{load_game_data, save_game_data, GameData, JavaInfo};
use crate::launcher::options::LaunchOptions;
use crate::loader::{create_loader, types::LoaderInstallInput};
use crate::models::loader::LoaderType;
use crate::models::minecraft::AssetItem;
use crate::net::check::check_internet;
use crate::net::downloader::Downloader;
use crate::utils::version_check::is_old;

// ── Launcher ──────────────────────────────────────────────────────────────────

pub struct Launcher {
    options: LaunchOptions,
    game_data: Option<GameData>,
}

impl Launcher {
    pub fn new(mut options: LaunchOptions) -> Self {
        // Absolutize options.path so every path derived from it (classpath,
        // natives, game args, java binary) works even when the Java process
        // runs with a different current_dir (e.g. save_dir for Tauri).
        if options.path.is_relative() {
            if let Ok(abs) = std::env::current_dir().map(|cwd| cwd.join(&options.path)) {
                options.path = abs;
            }
        }
        Self {
            options,
            game_data: None,
        }
    }

    pub fn options(&self) -> &LaunchOptions {
        &self.options
    }

    pub fn game_data(&self) -> Option<&GameData> {
        self.game_data.as_ref()
    }

    /// Download, verify, and optionally install a mod loader for the configured
    /// Minecraft version. Stores the result in `self.game_data`.
    ///
    /// Emits progress events on `event_tx`. After this call,
    /// [`Launcher::launch`] can be invoked without downloading again.
    ///
    /// If there is no internet connection and a valid cache exists, the cache
    /// is loaded without network access. If there is no cache either, returns
    /// [`LaunchError::NoInternetNoCache`].
    pub async fn download_game(
        &mut self,
        event_tx: Sender<LaunchEvent>,
    ) -> Result<(), LaunchError> {
        let options = &self.options;

        // ── Offline fast-path ─────────────────────────────────────────────────
        if !check_internet().await {
            self.game_data = Some(
                load_game_data(&options.save_dir())
                    .await
                    .map_err(|_| LaunchError::NoInternetNoCache)?,
            );
            return Ok(());
        }

        // ── Cache fast-path (skip_bundle_check) ──────────────────────────────
        // "Skip if possible" hint: load from cache and return early when the
        // caller trusts the existing installation.  If the cache is absent,
        // fall through to the full download path without error.
        //
        // Java is the one thing we never skip: without the runtime the process
        // can't even spawn (start() fails with os error 2), so `is_corrupt_crash`
        // would never get to see the crash logs produced by any missing game
        // files. We therefore always ensure Java is present here, while still
        // skipping the (expensive) bundle integrity check for everything else.
        if options.skip_bundle_check {
            if let Ok(mut cached) = load_game_data(&options.save_dir()).await {
                let java_present = std::path::Path::new(&cached.minecraft_java.path).exists();
                if !java_present {
                    let client =
                        crate::net::client::build_client(options.timeout_secs, options.force_ipv4)
                            .map_err(LaunchError::Http)?;
                    let java_result =
                        get_java_files(options, &cached.minecraft_json, &client, &event_tx).await?;
                    cached.minecraft_java = JavaInfo {
                        files: java_result.files,
                        path: java_result.java_path,
                    };
                    save_game_data(&options.save_dir(), &cached).await?;
                }
                self.game_data = Some(cached);
                let _ = event_tx.send(LaunchEvent::GameDownloadFinished).await;
                return Ok(());
            }
            // Cache absent → continue with normal download.
        }

        // ── Shared HTTP client ────────────────────────────────────────────────
        let client = crate::net::client::build_client(options.timeout_secs, options.force_ipv4)
            .map_err(LaunchError::Http)?;

        // ── Version JSON ──────────────────────────────────────────────────────
        let mut version_json = get_version_json(options, &client).await?;
        let mc_version = version_json.id.clone();

        // ── File bundle ───────────────────────────────────────────────────────
        let mut bundle: Vec<AssetItem> = Vec::new();
        bundle.extend(get_libraries(options, &version_json));
        bundle.extend(get_assets_others(options, options.url.as_deref(), &client).await?);
        bundle.extend(get_assets(options, &version_json, &client).await?);

        // Java runtime download is managed separately (has its own concurrency
        // and progress reporting); its files are not added to the bundle.
        let java_result = get_java_files(options, &version_json, &client, &event_tx).await?;

        // ── Bundle integrity check & download ─────────────────────────────────
        let pending =
            check_bundle(&bundle, &event_tx, options.clamped_verify_concurrency()).await?;
        if !pending.is_empty() {
            let downloader = Downloader::new(
                options.timeout_secs,
                options.clamped_concurrency(),
                options.force_ipv4,
            );
            downloader
                .download_multiple(pending, event_tx.clone())
                .await?;
        }

        // ── Mod loader install ────────────────────────────────────────────────
        let (
            loader_libraries,
            loader_main_class,
            loader_version_id,
            loader_type,
            loader_extra_game_args,
            loader_extra_jvm_args,
        ) = if options.loader.enable {
            if let Some(loader_type) = &options.loader.loader_type {
                let mc_jar = options
                    .path
                    .join("versions")
                    .join(&mc_version)
                    .join(format!("{mc_version}.jar"))
                    .to_string_lossy()
                    .into_owned();
                let mc_json = options
                    .path
                    .join("versions")
                    .join(&mc_version)
                    .join(format!("{mc_version}.json"))
                    .to_string_lossy()
                    .into_owned();

                let input = LoaderInstallInput {
                    mc_version: mc_version.clone(),
                    java_path: java_result.java_path.clone(),
                    mc_jar,
                    mc_json,
                };

                let loader_impl = create_loader(loader_type.clone());
                let result = loader_impl
                    .install(options, &input, &client, &event_tx)
                    .await?;
                (
                    result.libraries,
                    result.main_class,
                    Some(result.loader_version),
                    Some(result.loader_type),
                    result.extra_game_args,
                    result.extra_jvm_args,
                )
            } else {
                (vec![], None, None, None, vec![], vec![])
            }
        } else {
            (vec![], None, None, None, vec![], vec![])
        };

        // ── Download Forge/NeoForge runtime libraries ─────────────────────────
        // The loader install step (above) downloads processor/install-time JARs
        // but NOT the runtime classpath libraries listed in version.json (e.g.
        // bootstraplauncher, securejarhandler, modlauncher).  We check and
        // download them here.  When --installClient was used the files already
        // exist and check_bundle returns an empty pending list immediately.
        if !loader_libraries.is_empty() {
            let loader_pending = check_bundle(
                &loader_libraries,
                &event_tx,
                options.clamped_verify_concurrency(),
            )
            .await?;
            if !loader_pending.is_empty() {
                let downloader = Downloader::new(
                    options.timeout_secs,
                    options.clamped_concurrency(),
                    options.force_ipv4,
                );
                downloader
                    .download_multiple(loader_pending, event_tx.clone())
                    .await?;
            }
        }

        // ── Optional post-download SHA-1 verify ───────────────────────────────
        if options.verify {
            check_files(&bundle, &event_tx, options.clamped_verify_concurrency()).await?;
        }

        // ── Extract native JARs ───────────────────────────────────────────────
        extract_natives(options, &version_json, &bundle).await?;
        version_json.has_natives = bundle
            .iter()
            .any(|item| matches!(item, AssetItem::NativeAsset { .. }));

        // ── Legacy asset copy (pre-1.6) ───────────────────────────────────────
        if is_old(version_json.assets.as_deref()) {
            copy_assets(options, &version_json).await?;
        }

        // ── Persist & store ───────────────────────────────────────────────────
        let game_data = GameData {
            minecraft_json: version_json,
            minecraft_loader: None,
            minecraft_version: mc_version,
            minecraft_java: JavaInfo {
                files: java_result.files,
                path: java_result.java_path,
            },
            loader_libraries,
            loader_main_class,
            loader_version_id,
            loader_type,
            loader_extra_game_args,
            loader_extra_jvm_args,
        };

        save_game_data(&options.save_dir(), &game_data).await?;
        self.game_data = Some(game_data);

        let _ = event_tx.send(LaunchEvent::GameDownloadFinished).await;

        Ok(())
    }

    /// Assemble the Java command line and spawn the Minecraft process.
    ///
    /// Resolves game data from `self.game_data` (set by [`Launcher::download_game`])
    /// or, if absent, from the persisted cache on disk. Returns
    /// [`LaunchError::GameDataNotReady`] if neither is available.
    ///
    /// Stdout and stderr are piped; each line is forwarded as a
    /// [`LaunchEvent::Data`] event. The caller is responsible for calling
    /// `child.wait()` and emitting [`LaunchEvent::Close`] when appropriate.
    pub async fn launch(
        &self,
        event_tx: Sender<LaunchEvent>,
    ) -> Result<tokio::process::Child, LaunchError> {
        let loaded;
        let game_data: &GameData = match &self.game_data {
            Some(gd) => gd,
            None => {
                loaded = load_game_data(&self.options.save_dir())
                    .await
                    .map_err(|_| LaunchError::GameDataNotReady)?;
                &loaded
            }
        };

        let options = &self.options;
        let version_json = &game_data.minecraft_json;

        // Natives directory used for -Djava.library.path.
        let natives_path: PathBuf = options
            .path
            .join("versions")
            .join(&version_json.id)
            .join("natives");

        // Build the classpath: loader libraries FIRST so Forge/NeoForge classes
        // take precedence over vanilla when there are collisions.
        let mut bundle: Vec<AssetItem> = game_data.loader_libraries.clone();
        let mut vanilla_libs = get_libraries(options, version_json);
        // Modern Forge (1.17+) and NeoForge use the bootstraplauncher which manages
        // Minecraft classes via client-slim.jar. Including the full vanilla jar
        // causes split-package conflicts in the Java module layer.
        // Old Forge (pre-1.17, no module path args) uses FML's class patcher which
        // needs the vanilla jar directly on the classpath — don't exclude it there.
        let uses_module_path = game_data
            .loader_extra_jvm_args
            .iter()
            .any(|a| a == "-p" || a == "--module-path");
        let exclude_vanilla_jar = matches!(game_data.loader_type, Some(LoaderType::NeoForge))
            || (matches!(game_data.loader_type, Some(LoaderType::Forge)) && uses_module_path);
        if exclude_vanilla_jar {
            let mc_jar = options
                .path
                .join("versions")
                .join(&version_json.id)
                .join(format!("{}.jar", version_json.id))
                .to_string_lossy()
                .into_owned();
            vanilla_libs
                .retain(|lib| !matches!(lib, AssetItem::Asset { path, .. } if path == &mc_jar));
        }
        bundle.extend(vanilla_libs);

        // Argument assembly.
        let loader_ctx = game_data
            .loader_version_id
            .as_ref()
            .map(|vid| LoaderContext {
                loader_type: game_data.loader_type.as_ref(),
                version_id: Some(vid.as_str()),
                extra_game_args: &game_data.loader_extra_game_args,
                extra_jvm_args: &game_data.loader_extra_jvm_args,
            });
        let jvm_args = get_jvm_arguments(options, version_json, &natives_path, loader_ctx.as_ref());
        let mut game_args = get_game_arguments(options, version_json, loader_ctx.as_ref());
        let (cp_args, vanilla_main_class) = get_classpath(version_json, &bundle);

        // Screen size / fullscreen (conditional args from the version JSON are
        // skipped by get_game_arguments, so we add them here explicitly).
        if let Some(w) = options.screen.width {
            game_args.push("--width".into());
            game_args.push(w.to_string());
        }
        if let Some(h) = options.screen.height {
            game_args.push("--height".into());
            game_args.push(h.to_string());
        }
        if options.screen.fullscreen {
            game_args.push("--fullscreen".into());
        }

        let main_class = game_data
            .loader_main_class
            .as_deref()
            .unwrap_or(&vanilla_main_class)
            .to_owned();

        // Collect JARs on the Java module path (-p flag) from JVM args so we
        // can exclude them from -cp. NeoForge places bootstrap JARs on the
        // module path; having them on both paths causes IllegalStateException.
        let module_path_jars: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            let mut iter = jvm_args.iter().peekable();
            while let Some(arg) = iter.next() {
                if arg == "-p" {
                    if let Some(module_path) = iter.next() {
                        for jar in module_path.split(':') {
                            // Normalize to a canonical filename for matching.
                            if let Some(name) = std::path::Path::new(jar).file_name() {
                                set.insert(name.to_string_lossy().into_owned());
                            }
                        }
                    }
                }
            }
            set
        };

        // Filter module-path JARs out of -cp to avoid duplicate module errors.
        let cp_args = if module_path_jars.is_empty() {
            cp_args
        } else {
            cp_args
                .into_iter()
                .map(|arg| {
                    // The classpath string is the arg after "-cp".
                    if arg.contains(':') || arg.ends_with(".jar") {
                        let filtered: Vec<&str> = arg
                            .split(':')
                            .filter(|entry| {
                                let fname = std::path::Path::new(entry)
                                    .file_name()
                                    .map(|f| f.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                !module_path_jars.contains(&fname)
                            })
                            .collect();
                        filtered.join(":")
                    } else {
                        arg
                    }
                })
                .collect()
        };

        let mut all_args: Vec<String> = Vec::new();
        all_args.extend(jvm_args);
        #[cfg(target_os = "linux")]
        all_args.push("-DGLFW_PLATFORM=x11".into());
        all_args.extend(cp_args);
        all_args.push(main_class);
        all_args.extend(game_args);

        let java_path_raw = &game_data.minecraft_java.path;
        // Resolve to an absolute path so the binary is found regardless of
        // what current_dir is set to below.
        let java_path_buf = std::path::Path::new(java_path_raw)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(java_path_raw));
        let java_path = java_path_buf.to_string_lossy();

        // Sanitize auth token before logging the command.
        let access_token = &options.authenticator.access_token;
        let cmd_str = format!("{} {}", java_path, all_args.join(" "));
        let sanitized = if access_token.is_empty() {
            cmd_str
        } else {
            cmd_str.replace(access_token.as_str(), "<access_token>")
        };
        let _ = event_tx.send(LaunchEvent::Data(sanitized)).await;

        // Spawn the process.
        let mut cmd = tokio::process::Command::new(java_path.as_ref());
        cmd.args(&all_args)
            .current_dir(options.save_dir())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // On Linux, force GLFW 3.4+ to use X11 via XWayland to avoid the
        // [0x1000C] "Wayland: does not provide window position" error that
        // Forge treats as fatal in its strict GLX._initGlfw error callback.
        //
        // DISPLAY may not be exported on pure Wayland sessions (e.g. GNOME on
        // Fedora with on-demand XWayland) even when XWayland is available, so
        // we fall back to probing the X11 socket directly.
        //
        // Removing WAYLAND_DISPLAY alone is not enough: libwayland's
        // wl_display_connect(NULL) falls back to "wayland-0" via
        // XDG_RUNTIME_DIR even when WAYLAND_DISPLAY is absent. Setting
        // WAYLAND_SOCKET to a non-numeric value causes wl_display_connect to
        // return NULL immediately (before the fallback path), so GLFW's Wayland
        // backend fails and it falls through to X11.
        #[cfg(target_os = "linux")]
        {
            let display = std::env::var_os("DISPLAY").or_else(|| {
                (0..10u8).find_map(|n| {
                    let sock = format!("/tmp/.X11-unix/X{n}");
                    std::path::Path::new(&sock)
                        .exists()
                        .then(|| format!(":{n}").into())
                })
            });
            if let Some(disp) = display {
                cmd.env("DISPLAY", disp);
                cmd.env_remove("WAYLAND_DISPLAY");
                cmd.env("GLFW_PLATFORM", "x11");
                cmd.env("WAYLAND_SOCKET", "invalid");
            }
        }

        // LWJGL 2 (old Minecraft, pre-1.13) runs `xrandr` at startup via
        // Runtime.exec() to enumerate display modes. If xrandr is not installed
        // the subprocess returns nothing, getScreenNames() returns an empty
        // array, and LinuxDisplay:951 throws ArrayIndexOutOfBoundsException: 0.
        // Fix: if xrandr is missing, drop a minimal stub script into a cache
        // dir and prepend that dir to PATH so Java finds it first.
        #[cfg(target_os = "linux")]
        if crate::game::lwjgl_native::uses_lwjgl2(version_json)
            && !crate::game::lwjgl_native::xrandr_in_path()
        {
            let stub_dir = options.path.join("cache").join("xrandr-stub");
            if crate::game::lwjgl_native::write_xrandr_stub(&stub_dir)
                .await
                .is_ok()
            {
                let base_path = std::env::var("PATH").unwrap_or_default();
                cmd.env(
                    "PATH",
                    format!("{}:{}", stub_dir.to_string_lossy(), base_path),
                );
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| LaunchError::ProcessError(e.to_string()))?;

        // Pipe stdout lines → LaunchEvent::Data.
        if let Some(stdout) = child.stdout.take() {
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(LaunchEvent::Data(line)).await;
                }
            });
        }

        // Pipe stderr lines → LaunchEvent::Data.
        if let Some(stderr) = child.stderr.take() {
            let tx = event_tx;
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(LaunchEvent::Data(line)).await;
                }
            });
        }

        Ok(child)
    }

    /// Download the game and immediately launch it.
    ///
    /// Equivalent to `download_game` followed by `launch`. Returns the
    /// [`tokio::process::Child`] handle so the caller can monitor or kill
    /// the process.
    ///
    /// To receive a [`LaunchEvent::Close`] event, wait on the returned child
    /// and send it yourself:
    /// ```ignore
    /// let code = child.wait().await?.code().unwrap_or(-1);
    /// let _ = tx.send(LaunchEvent::Close(code)).await;
    /// ```
    pub async fn start(
        &mut self,
        event_tx: Sender<LaunchEvent>,
    ) -> Result<tokio::process::Child, LaunchError> {
        self.download_game(event_tx.clone()).await?;
        self.launch(event_tx).await
    }

    /// Heuristically detect whether a game crash was caused by a corrupt or
    /// incomplete installation.
    ///
    /// Returns `true` when `exit_code` is non-zero **and** at least one log
    /// line matches a known corrupt-installation pattern. The caller can use
    /// this as a signal to force a full re-check by calling `download_game()`
    /// with `skip_bundle_check: false` on the next attempt.
    ///
    /// # Example
    /// ```ignore
    /// let code = child.wait().await?.code().unwrap_or(-1);
    /// let lines: Vec<String> = /* collected LaunchEvent::Data lines */;
    /// if Launcher::is_corrupt_crash(code, &lines) {
    ///     // Re-run download_game with skip_bundle_check: false
    /// }
    /// ```
    pub fn is_corrupt_crash(exit_code: i32, logs: &[String]) -> bool {
        if exit_code == 0 {
            return false;
        }
        // Match only on JVM exception class names, which the runtime prints
        // verbatim regardless of its locale. Localized prose messages (e.g.
        // "Error: Could not find or load main class", "Unable to access
        // jarfile", "Error opening zip file") are translated on non-English
        // JVMs and must not be relied on — the corresponding exception is
        // always present alongside them and is locale-independent:
        //   missing main class / jar  → ClassNotFoundException / NoClassDefFoundError
        //   missing native library    → UnsatisfiedLinkError
        //   missing file              → FileNotFoundException / NoSuchFileException
        //   corrupt archive           → ZipException
        const PATTERNS: &[&str] = &[
            "NoClassDefFoundError",
            "ClassNotFoundException",
            "UnsatisfiedLinkError",
            "FileNotFoundException",
            "NoSuchFileException",
            "ZipException",
        ];
        logs.iter()
            .any(|line| PATTERNS.iter().any(|pat| line.contains(pat)))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_options() -> LaunchOptions {
        use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
        use crate::models::minecraft::Authenticator;
        LaunchOptions {
            path: PathBuf::from("/mc"),
            version: "1.20.4".into(),
            authenticator: Authenticator {
                access_token: "test-token".into(),
                name: "Player".into(),
                uuid: "test-uuid".into(),
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
    fn launcher_new_stores_options() {
        let opts = make_options();
        let launcher = Launcher::new(opts.clone());
        assert_eq!(launcher.options.version, "1.20.4");
        assert_eq!(launcher.options.path, PathBuf::from("/mc"));
    }

    #[test]
    fn launcher_save_dir_no_instance() {
        let opts = make_options();
        let launcher = Launcher::new(opts);
        assert_eq!(launcher.options.save_dir(), PathBuf::from("/mc"));
    }

    #[test]
    fn launcher_save_dir_with_instance() {
        let mut opts = make_options();
        opts.instance = Some("myworld".into());
        let launcher = Launcher::new(opts);
        assert_eq!(
            launcher.options.save_dir(),
            PathBuf::from("/mc/instances/myworld")
        );
    }

    #[test]
    fn sanitize_replaces_access_token() {
        let token = "secret-access-token";
        let cmd = format!("java -cp foo.jar Main --accessToken {token}");
        let sanitized = cmd.replace(token, "<access_token>");
        assert!(!sanitized.contains(token));
        assert!(sanitized.contains("<access_token>"));
    }

    #[test]
    fn all_args_order_is_correct() {
        // Verify the expected CLI ordering: jvm_args, -cp, classpath, main_class, game_args
        let jvm: Vec<String> = vec!["-Xms1G".into(), "-Xmx2G".into()];
        let cp: Vec<String> = vec!["-cp".into(), "a.jar:b.jar".into()];
        let main_class = "net.minecraft.client.main.Main".to_owned();
        let game: Vec<String> = vec!["--username".into(), "Player".into()];

        let mut all: Vec<String> = Vec::new();
        all.extend(jvm);
        all.extend(cp);
        all.push(main_class.clone());
        all.extend(game);

        assert_eq!(all[0], "-Xms1G");
        assert_eq!(all[2], "-cp");
        assert_eq!(all[4], main_class);
        assert_eq!(all[5], "--username");
    }

    #[test]
    fn screen_args_appended_when_set() {
        use crate::launcher::options::ScreenConfig;
        let screen = ScreenConfig {
            width: Some(1920),
            height: Some(1080),
            fullscreen: false,
        };
        let mut game_args: Vec<String> = vec!["--version".into(), "1.20.4".into()];
        if let Some(w) = screen.width {
            game_args.push("--width".into());
            game_args.push(w.to_string());
        }
        if let Some(h) = screen.height {
            game_args.push("--height".into());
            game_args.push(h.to_string());
        }
        assert!(game_args.contains(&"--width".to_string()));
        assert!(game_args.contains(&"1920".to_string()));
        assert!(game_args.contains(&"--height".to_string()));
        assert!(game_args.contains(&"1080".to_string()));
        assert!(!game_args.contains(&"--fullscreen".to_string()));
    }

    #[test]
    fn screen_fullscreen_appended_when_set() {
        use crate::launcher::options::ScreenConfig;
        let screen = ScreenConfig {
            width: None,
            height: None,
            fullscreen: true,
        };
        let mut game_args: Vec<String> = vec![];
        if screen.fullscreen {
            game_args.push("--fullscreen".into());
        }
        assert!(game_args.contains(&"--fullscreen".to_string()));
    }

    #[test]
    fn loader_main_class_overrides_vanilla() {
        let vanilla = "net.minecraft.client.main.Main".to_owned();
        let loader_main_class: Option<String> =
            Some("net.fabricmc.loader.impl.launch.knot.KnotClient".into());
        let main_class = loader_main_class.as_deref().unwrap_or(&vanilla).to_owned();
        assert_eq!(
            main_class,
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
    }

    #[test]
    fn no_loader_main_class_uses_vanilla() {
        let vanilla = "net.minecraft.client.main.Main".to_owned();
        let loader_main_class: Option<String> = None;
        let main_class = loader_main_class.as_deref().unwrap_or(&vanilla).to_owned();
        assert_eq!(main_class, "net.minecraft.client.main.Main");
    }

    // ── is_corrupt_crash ─────────────────────────────────────────────────────

    #[test]
    fn corrupt_crash_zero_exit_always_false() {
        let logs = vec!["NoClassDefFoundError: net/minecraft/Foo".into()];
        assert!(!Launcher::is_corrupt_crash(0, &logs));
    }

    #[test]
    fn corrupt_crash_nonzero_no_pattern_false() {
        let logs = vec!["Exception in thread \"main\" java.lang.RuntimeException".into()];
        assert!(!Launcher::is_corrupt_crash(1, &logs));
    }

    #[test]
    fn corrupt_crash_empty_logs_false() {
        assert!(!Launcher::is_corrupt_crash(1, &[]));
    }

    #[test]
    fn corrupt_crash_all_patterns_detected() {
        let cases = [
            "java.lang.NoClassDefFoundError: Foo",
            "java.lang.ClassNotFoundException: net.minecraft.Main",
            "java.lang.UnsatisfiedLinkError: /lib/foo.so",
            "java.io.FileNotFoundException: /mc/lib.jar (No such file)",
            "java.nio.file.NoSuchFileException: /mc/versions/1.20.4/1.20.4.jar",
            "java.util.zip.ZipException: invalid LOC header",
        ];
        for case in &cases {
            assert!(
                Launcher::is_corrupt_crash(1, &[case.to_string()]),
                "pattern not detected: {case}"
            );
        }
    }

    #[test]
    fn corrupt_crash_detected_on_localized_main_class_error() {
        // A non-English JVM translates the prose line but the cause exception
        // (ClassNotFoundException) is locale-independent and must still match.
        let logs = vec![
            "Error: no se ha encontrado o cargado la clase principal net.minecraft.client.main.Main".into(),
            "Causado por: java.lang.ClassNotFoundException: net.minecraft.client.main.Main".into(),
        ];
        assert!(Launcher::is_corrupt_crash(1, &logs));
    }
}
