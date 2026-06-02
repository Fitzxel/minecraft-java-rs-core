/// Integration tests for minecraft-java-rs-core.
///
/// Tests marked `#[ignore]` require a live internet connection and are skipped
/// in offline CI. Run them explicitly with:
///
///   cargo test -- --include-ignored
use minecraft_java_rs_core::{
    game::bundle::{check_bundle, check_files, get_total_size},
    loader::{create_loader, ModLoader},
    loader::types::{LoaderInstallInput, LoaderResult},
    models::{
        loader::LoaderType,
        minecraft::AssetItem,
    },
    utils::paths::get_path_libraries,
};
use tokio::sync::mpsc;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn drain_channel<T>(rx: &mut mpsc::Receiver<T>) {
    while rx.try_recv().is_ok() {}
}

// ── get_path_libraries — edge cases ──────────────────────────────────────────

#[test]
fn path_libraries_triple_coordinate_no_extras() {
    let info = get_path_libraries("com.example:mylib:1.0.0", None, None).unwrap();
    assert_eq!(info.name, "mylib-1.0.0.jar");
    assert_eq!(info.path, "com/example/mylib/1.0.0");
}

#[test]
fn path_libraries_with_classifier() {
    let info = get_path_libraries("com.example:mylib:1.0.0", Some("natives-linux"), None).unwrap();
    assert!(info.name.contains("natives-linux"));
    assert!(info.name.ends_with(".jar"));
}

#[test]
fn path_libraries_at_extension_overrides_jar() {
    let info = get_path_libraries("com.example:mylib:1.0.0@zip", None, None).unwrap();
    assert!(info.name.ends_with(".zip"), "expected .zip, got {}", info.name);
}

#[test]
fn path_libraries_force_ext_overrides_default_jar() {
    // force_ext replaces the default .jar when there is no @ext in the coordinate.
    let info = get_path_libraries("com.example:mylib:1.0.0", None, Some(".lzma")).unwrap();
    assert!(info.name.ends_with(".lzma"), "expected .lzma, got {}", info.name);
}

#[test]
fn path_libraries_at_ext_takes_precedence_over_force_ext() {
    // When the coordinate includes @zip, the @ext wins; force_ext is not applied.
    let info = get_path_libraries("com.example:mylib:1.0.0@zip", None, Some("lzma")).unwrap();
    assert!(info.name.ends_with(".zip"), "expected @zip to win, got {}", info.name);
}

#[test]
fn path_libraries_group_with_dots_becomes_slashes() {
    let info = get_path_libraries("org.lwjgl:lwjgl:3.3.1", None, None).unwrap();
    assert!(info.path.starts_with("org/lwjgl/lwjgl/"));
}

#[test]
fn path_libraries_malformed_returns_error() {
    assert!(get_path_libraries("onlyone", None, None).is_err());
    assert!(get_path_libraries("two:parts", None, None).is_err());
}

#[test]
fn path_libraries_version_with_snapshot_suffix() {
    let info = get_path_libraries("io.netty:netty-all:4.1.97.Final", None, None).unwrap();
    assert_eq!(info.name, "netty-all-4.1.97.Final.jar");
}

// ── check_bundle — real filesystem round-trip ─────────────────────────────────

#[tokio::test]
async fn check_bundle_writes_cfile_and_queues_missing_assets() {
    use sha1::{Digest, Sha1};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let (tx, mut rx) = mpsc::channel(64);

    // Create an existing file with a known SHA-1.
    let existing = dir.path().join("existing.jar");
    let existing_content = b"fake library content";
    std::fs::write(&existing, existing_content).unwrap();
    let existing_sha1 = format!("{:x}", Sha1::digest(existing_content));

    // Paths for files that will NOT exist yet.
    let cfile_path = dir.path().join("sub").join("index.json");
    let missing_asset = dir.path().join("assets").join("missing.bin");

    let bundle = vec![
        // CFile: written verbatim, no SHA check.
        AssetItem::CFile {
            path: cfile_path.to_string_lossy().into_owned(),
            content: r#"{"objects":{}}"#.into(),
        },
        // Asset that's already on disk with correct hash — should NOT be queued.
        AssetItem::Asset {
            path: existing.to_string_lossy().into_owned(),
            sha1: existing_sha1.clone(),
            size: existing_content.len() as u64,
            url: "https://example.com/existing.jar".into(),
        },
        // Asset that doesn't exist yet — must be queued for download.
        AssetItem::Asset {
            path: missing_asset.to_string_lossy().into_owned(),
            sha1: "da39a3ee5e6b4b0d3255bfef95601890afd80709".into(), // SHA-1 of empty
            size: 0,
            url: "https://resources.download.minecraft.net/da/da39a3ee5e6b4b0d3255bfef95601890afd80709".into(),
        },
    ];

    let total_size = get_total_size(&bundle);
    // CFile has size 0; Asset sizes are summed.
    assert_eq!(total_size, existing_content.len() as u64);

    let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
    drain_channel(&mut rx);

    // CFile must have been written to disk (including its parent directory).
    assert!(cfile_path.exists(), "CFile was not written");
    assert_eq!(
        std::fs::read_to_string(&cfile_path).unwrap(),
        r#"{"objects":{}}"#
    );

    // Only the missing asset should be queued.
    assert_eq!(pending.len(), 1, "expected 1 pending download, got {:?}", pending.len());
    assert!(
        pending[0].url.contains("da39a3ee"),
        "wrong URL in pending: {}",
        pending[0].url
    );

    // ── check_files: correct file passes; corrupted file is flagged ─────────
    // Write the missing file with the WRONG content so SHA-1 mismatch is detected.
    std::fs::create_dir_all(missing_asset.parent().unwrap()).unwrap();
    std::fs::write(&missing_asset, b"wrong content").unwrap();

    let (tx2, mut rx2) = mpsc::channel(64);
    let corrupted = check_files(&bundle, &tx2, 4).await.unwrap();
    drain_channel(&mut rx2);

    // existing.jar is correct; missing.bin has wrong SHA-1.
    assert_eq!(corrupted.len(), 1, "expected 1 corrupted file, got {corrupted:?}");
    assert!(
        corrupted[0].contains("missing.bin"),
        "unexpected corrupted path: {}",
        corrupted[0]
    );
}

#[tokio::test]
async fn check_bundle_native_asset_queued_when_missing() {
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let (tx, mut rx) = mpsc::channel(16);

    let native_path = dir.path().join("native.so");

    let bundle = vec![AssetItem::NativeAsset {
        path: native_path.to_string_lossy().into_owned(),
        sha1: "aabbcc".into(),
        size: 512,
        url: "https://example.com/native.so".into(),
    }];

    let total = get_total_size(&bundle);
    assert_eq!(total, 512);

    let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
    drain_channel(&mut rx);

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].url, "https://example.com/native.so");
}

// ── Loader dispatcher — comprehensive ────────────────────────────────────────

#[test]
fn dispatcher_creates_distinct_box_for_every_loader_type() {
    let types = [
        LoaderType::Forge,
        LoaderType::NeoForge,
        LoaderType::Fabric,
        LoaderType::LegacyFabric,
        LoaderType::Quilt,
    ];

    // Each call must succeed and produce a Box<dyn ModLoader>.
    let loaders: Vec<Box<dyn ModLoader>> =
        types.into_iter().map(create_loader).collect();

    assert_eq!(loaders.len(), 5);
}

#[test]
fn loader_install_input_roundtrip() {
    let input = LoaderInstallInput {
        mc_version: "1.20.4".into(),
        java_path: "/usr/lib/jvm/java-21/bin/java".into(),
        mc_jar: "/mc/versions/1.20.4/1.20.4.jar".into(),
        mc_json: "/mc/versions/1.20.4/1.20.4.json".into(),
    };

    assert_eq!(input.mc_version, "1.20.4");
    assert!(input.java_path.ends_with("/java"));
    assert!(input.mc_jar.ends_with(".jar"));
    assert!(input.mc_json.ends_with(".json"));
}

#[test]
fn loader_result_carries_expected_fields() {
    let result = LoaderResult {
        libraries: vec![
            AssetItem::Asset {
                path: "/mc/libraries/fabric-loader.jar".into(),
                sha1: "abc".into(),
                size: 1024,
                url: "https://maven.fabricmc.net/fabric-loader.jar".into(),
            },
        ],
        main_class: Some("net.fabricmc.loader.impl.launch.knot.KnotClient".into()),
        loader_version: "fabric-loader-0.15.6-1.20.4".into(),
        loader_type: LoaderType::Fabric,
        extra_game_args: vec![],
        extra_jvm_args: vec![],
    };

    assert_eq!(result.loader_type, LoaderType::Fabric);
    assert_eq!(result.libraries.len(), 1);
    assert!(result.main_class.is_some());
    assert!(result.loader_version.contains("0.15.6"));
}

// ── Launcher construction ─────────────────────────────────────────────────────

#[test]
fn launcher_constructs_and_save_dir_is_correct() {
    use minecraft_java_rs_core::launcher::Launcher;
    use minecraft_java_rs_core::launcher::options::{
        JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig,
    };
    use minecraft_java_rs_core::models::minecraft::Authenticator;
    use std::path::PathBuf;

    let options = minecraft_java_rs_core::launcher::options::LaunchOptions {
        path: PathBuf::from("/mc"),
        version: "1.20.4".into(),
        authenticator: Authenticator {
            access_token: "token".into(),
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
    };

    let launcher = Launcher::new(options);
    assert_eq!(launcher.options().save_dir(), PathBuf::from("/mc"));
}

// ── Network-dependent tests (skipped unless --include-ignored) ────────────────

#[tokio::test]
#[ignore = "requires internet: hits live Mojang API"]
async fn get_version_json_real_network_resolves_latest_release() {
    use minecraft_java_rs_core::launcher::options::{
        JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig,
    };
    use minecraft_java_rs_core::models::minecraft::Authenticator;
    use std::path::PathBuf;

    let options = minecraft_java_rs_core::launcher::options::LaunchOptions {
        path: PathBuf::from("/tmp/mc-integration-test"),
        version: "latest_release".into(),
        authenticator: Authenticator {
            access_token: String::new(),
            name: "TestUser".into(),
            uuid: "test-uuid".into(),
            xbox_account: None,
            user_properties: None,
            client_id: None,
            client_token: None,
        },
        timeout_secs: 15,
        download_concurrency: 2,
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
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    let version_json =
        minecraft_java_rs_core::game::version::get_version_json(&options, &client)
            .await
            .unwrap();

    // The resolved version should be a non-empty string like "1.20.4".
    assert!(!version_json.id.is_empty());
    assert_eq!(version_json.version_type, "release");
    assert!(version_json.main_class.is_some());
    // Modern versions have the structured arguments block.
    assert!(
        version_json.arguments.is_some() || version_json.minecraft_arguments.is_some(),
        "version JSON must have arguments"
    );
    println!("Resolved latest_release to: {}", version_json.id);
}

#[tokio::test]
#[ignore = "requires internet, downloads real game files (~100 MB+)"]
async fn download_game_end_to_end() {
    use minecraft_java_rs_core::launcher::Launcher;
    use minecraft_java_rs_core::launcher::options::{
        JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig,
    };
    use minecraft_java_rs_core::models::minecraft::Authenticator;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();

    let options = minecraft_java_rs_core::launcher::options::LaunchOptions {
        path: dir.path().to_path_buf(),
        version: "1.20.4".into(),
        authenticator: Authenticator {
            access_token: "offline".into(),
            name: "TestUser".into(),
            uuid: "00000000-0000-0000-0000-000000000001".into(),
            xbox_account: None,
            user_properties: None,
            client_id: None,
            client_token: None,
        },
        timeout_secs: 30,
        download_concurrency: 10,
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
    };

    let (tx, mut rx) = mpsc::channel(256);

    // Consume events in a background task so the channel never blocks.
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                minecraft_java_rs_core::launcher::LaunchEvent::Progress {
                    downloaded,
                    total,
                    kind,
                } => {
                    eprintln!("[{kind}] {downloaded}/{total}");
                }
                minecraft_java_rs_core::launcher::LaunchEvent::GameDownloadFinished => {
                    eprintln!("Download complete");
                }
                _ => {}
            }
        }
    });

    let mut launcher = Launcher::new(options);
    launcher.download_game(tx).await.unwrap();

    let game_data = launcher.game_data().unwrap();
    assert_eq!(game_data.minecraft_version, "1.20.4");
    assert!(!game_data.minecraft_java.path.is_empty());
    assert!(dir.path().join("versions").join("1.20.4").exists());
}
