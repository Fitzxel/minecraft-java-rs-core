//! Minimal Minecraft launcher example — mirrors the behavior of `test.js`.
//!
//! # Usage
//!
//! ```
//! cargo run --example launch -- [OPTIONS]
//! ```
//!
//! # Options
//!
//! ```
//! --version <VERSION>       Minecraft version (default: 1.20.4)
//! --username <NAME>         Offline username  (default: Player)
//! --path <PATH>             Game directory    (default: ./minecraft)
//! --min-mem <SIZE>          JVM min heap      (default: 2G)
//! --max-mem <SIZE>          JVM max heap      (default: 4G)
//! --loader-type <TYPE>      forge | neoforge | fabric | legacyfabric | quilt
//! --loader-build <BUILD>    latest | recommended | <exact-version> (default: latest)
//! --only-download           Download game files without launching
//! --auto-close <SECS>       Kill Minecraft after N seconds
//! --help                    Print this message
//! ```
//!
//! # Examples
//!
//! ```sh
//! # Download and launch Minecraft 1.20.4 in offline mode
//! cargo run --example launch -- --version 1.20.4 --username Steve
//!
//! # Download only (useful for pre-caching)
//! cargo run --example launch -- --only-download
//!
//! # Fabric with auto-close after 30 s
//! cargo run --example launch -- --loader-type fabric --auto-close 30
//! ```

use std::{io::Write, path::PathBuf, time::Duration};

use minecraft_java_rs_core::{
    launcher::{options::LaunchOptions, LaunchEvent, Launcher},
    models::{loader::LoaderType, minecraft::Authenticator},
    utils::auth::offline_uuid,
};
use tokio::{sync::mpsc, time::sleep};

// ── CLI arg parsing ───────────────────────────────────────────────────────────

struct Args {
    version: String,
    username: String,
    path: PathBuf,
    instance: Option<String>,
    min_mem: String,
    max_mem: String,
    loader_type: Option<LoaderType>,
    loader_build: String,
    only_download: bool,
    verify: bool,
    skip_bundle_check: bool,
    force_ipv4: bool,
    dns: Option<std::net::IpAddr>,
    auto_close: Option<u64>,
}

impl Args {
    fn parse() -> Self {
        let argv: Vec<String> = std::env::args().skip(1).collect();

        if argv.iter().any(|a| a == "--help" || a == "-h") {
            print_help();
            std::process::exit(0);
        }

        fn flag_val(argv: &[String], flag: &str) -> Option<String> {
            argv.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
        }

        let loader_type = flag_val(&argv, "--loader-type").and_then(|s| match s.as_str() {
            "forge" => Some(LoaderType::Forge),
            "neoforge" => Some(LoaderType::NeoForge),
            "fabric" => Some(LoaderType::Fabric),
            "legacyfabric" => Some(LoaderType::LegacyFabric),
            "quilt" => Some(LoaderType::Quilt),
            other => {
                eprintln!("Unknown loader type: {other}. Valid: forge | neoforge | fabric | legacyfabric | quilt");
                std::process::exit(1);
            }
        });

        Args {
            version: flag_val(&argv, "--version").unwrap_or_else(|| "1.20.4".into()),
            username: flag_val(&argv, "--username").unwrap_or_else(|| "Player".into()),
            path: flag_val(&argv, "--path")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./minecraft")),
            instance: flag_val(&argv, "--instance"),
            min_mem: flag_val(&argv, "--min-mem").unwrap_or_else(|| "2G".into()),
            max_mem: flag_val(&argv, "--max-mem").unwrap_or_else(|| "4G".into()),
            loader_type,
            loader_build: flag_val(&argv, "--loader-build").unwrap_or_else(|| "latest".into()),
            only_download: argv.iter().any(|a| a == "--only-download"),
            verify: argv.iter().any(|a| a == "--verify"),
            skip_bundle_check: argv.iter().any(|a| a == "--skip-bundle-check"),
            force_ipv4: argv.iter().any(|a| a == "--force-ipv4"),
            dns: flag_val(&argv, "--dns").and_then(|s| s.parse::<std::net::IpAddr>().ok()),
            auto_close: flag_val(&argv, "--auto-close").and_then(|s| s.parse::<u64>().ok()),
        }
    }
}

fn print_help() {
    println!(
        r#"minecraft-java-rs-core launcher example

USAGE:
  cargo run --example launch -- [OPTIONS]

OPTIONS:
  --version <VERSION>       Minecraft version            [default: 1.20.4]
  --username <NAME>         Offline username             [default: Player]
  --path <PATH>             Game directory               [default: ./minecraft]
  --instance <NAME>         Instance name (saves to <path>/instances/<name>)
  --min-mem <SIZE>          JVM minimum heap (e.g. 2G)  [default: 2G]
  --max-mem <SIZE>          JVM maximum heap (e.g. 4G)  [default: 4G]
  --loader-type <TYPE>      Mod loader type:
                              forge | neoforge | fabric | legacyfabric | quilt
  --loader-build <BUILD>    Loader build:
                              latest | recommended | <exact-version>
                                                         [default: latest]
  --only-download           Download game files, don't launch
  --skip-bundle-check       Skip integrity check if gameData.json exists (fast re-launch)
  --force-ipv4              Force IPv4 for downloads (fixes broken-IPv6 connection errors)
  --dns <IP>                Resolve via DNS-over-HTTPS to this IP, e.g. 1.1.1.1
                              (bypasses ISP DNS hijacking / port-53 blocking)
  --verify                  Re-verify SHA-1 of all files after download
  --auto-close <SECS>       Kill Minecraft after N seconds
  -h, --help                Print this message

EXAMPLES:
  cargo run --example launch -- --version 1.20.4 --username Steve
  cargo run --example launch -- --instance myworld --version 1.20.4
  cargo run --example launch -- --only-download
  cargo run --example launch -- --loader-type fabric --auto-close 30
  cargo run --example launch -- --loader-type forge --loader-build 47.4.10
"#
    );
}

// ── Event printer ─────────────────────────────────────────────────────────────

async fn print_events(mut rx: mpsc::Receiver<LaunchEvent>) -> Vec<String> {
    let mut logs: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            LaunchEvent::Progress {
                downloaded,
                total,
                kind,
            } => {
                let pct = if total > 0 {
                    (downloaded as f64 / total as f64 * 100.0) as u32
                } else {
                    0
                };
                print!("\r[{kind}]: {downloaded}/{total} ({pct}%)   ");
                let _ = std::io::stdout().flush();
            }
            LaunchEvent::Check {
                current,
                total,
                kind,
            } => {
                print!("\r[check/{kind}]: {current}/{total}   ");
                let _ = std::io::stdout().flush();
            }
            LaunchEvent::Speed(bps) => {
                let kb = bps / 1024.0;
                if kb > 1024.0 {
                    print!("  [{:.1} MB/s]", kb / 1024.0);
                } else {
                    print!("  [{kb:.0} KB/s]");
                }
                let _ = std::io::stdout().flush();
            }
            LaunchEvent::GameDownloadFinished => {
                println!("\n[#] All game files ready.");
            }
            LaunchEvent::Extract(name) => {
                println!("\r[extract]: {name}");
            }
            LaunchEvent::Patch(msg) => {
                print!("[patch]: {msg}");
                let _ = std::io::stdout().flush();
            }
            LaunchEvent::Data(line) => {
                println!("[MC]: {line}");
                logs.push(line);
            }
            LaunchEvent::Error(msg) => {
                eprintln!("\n[error]: {msg}");
            }
            LaunchEvent::Close(code) => {
                println!("\n[#] Minecraft exited with code {code}.");
            }
            LaunchEvent::Estimated(_) => {}
        }
    }
    logs
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // ── Offline authenticator ─────────────────────────────────────────────────
    let uuid = offline_uuid(&args.username);
    let auth = Authenticator {
        access_token: "offline".into(),
        name: args.username.clone(),
        uuid: uuid.clone(),
        xbox_account: None,
        user_properties: None,
        client_id: None,
        client_token: None,
    };

    // ── LaunchOptions ─────────────────────────────────────────────────────────
    use minecraft_java_rs_core::launcher::options::{
        JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig,
    };

    let loader = LoaderConfig {
        loader_type: args.loader_type.clone(),
        build: args.loader_build.clone(),
        enable: args.loader_type.is_some(),
        path: None,
        config: None,
    };

    let options = LaunchOptions {
        path: args.path.clone(),
        version: args.version.clone(),
        authenticator: auth,
        timeout_secs: 30,
        download_concurrency: 10,
        verify_concurrency: 4,
        memory: MemoryConfig {
            min: args.min_mem.clone(),
            max: args.max_mem.clone(),
        },
        java: JavaOptions::default(),
        loader,
        screen: ScreenConfig::default(),
        verify: args.verify,
        game_args: vec![],
        jvm_args: vec![],
        instance: args.instance.clone(),
        url: None,
        mcp: None,
        intel_enabled_mac: false,
        bypass_offline: true,
        skip_bundle_check: args.skip_bundle_check,
        force_ipv4: args.force_ipv4,
        dns: args.dns,
    };

    // ── Banner ────────────────────────────────────────────────────────────────
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  minecraft-java-rs-core launcher");
    println!("  version  : {}", args.version);
    println!("  username : {} ({})", args.username, uuid);
    println!("  path     : {}", args.path.display());
    if let Some(inst) = &args.instance {
        println!("  instance : {inst}");
    }
    if let Some(lt) = &args.loader_type {
        println!("  loader   : {lt} @ {}", args.loader_build);
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let mut launcher = Launcher::new(options);

    // ── Event channel ─────────────────────────────────────────────────────────
    let (tx, rx) = mpsc::channel::<LaunchEvent>(512);
    let printer = tokio::spawn(print_events(rx));

    // ── Download only ─────────────────────────────────────────────────────────
    if args.only_download {
        println!("[#] Starting download...");
        if let Err(e) = launcher.download_game(tx).await {
            eprintln!("[error] download_game failed: {e}");
            std::process::exit(1);
        }
        printer.await.ok();
        println!("[#] Done. Exiting.");
        return;
    }

    let close_tx = tx.clone(); // keep a clone to send Close after child exits

    // ── Start ─────────────────────────────────────────────────────────────────
    println!("[#] Starting download + launch...");
    let mut child = match launcher.start(tx).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[error] start failed: {e}");
            std::process::exit(1);
        }
    };

    let pid = child.id().unwrap_or(0);
    println!();
    println!("┌─────────────────────────────────────┐");
    println!("│  Minecraft launched!  PID: {pid:<9} │");
    println!("└─────────────────────────────────────┘");

    // ── Auto-close ────────────────────────────────────────────────────────────
    if let Some(secs) = args.auto_close {
        println!("[#] Auto-closing in {secs}s...");
        sleep(Duration::from_secs(secs)).await;
        println!("[#] Killing Minecraft (auto-close).");
        child.kill().await.ok();
    }

    // ── Wait for exit ─────────────────────────────────────────────────────────
    let code = match child.wait().await {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => {
            eprintln!("[error] wait failed: {e}");
            -1
        }
    };

    let _ = close_tx.send(LaunchEvent::Close(code)).await;
    drop(close_tx);

    let logs = printer.await.unwrap_or_default();

    if args.skip_bundle_check && Launcher::is_corrupt_crash(code, &logs) {
        eprintln!();
        eprintln!("[!] Minecraft crashed and the cause is likely a corrupt installation.");
        eprintln!(
            "[!] Re-launch without --skip-bundle-check to force a full file integrity check."
        );
    }
}
