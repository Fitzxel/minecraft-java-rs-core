# minecraft-java-rs-core

A Rust library that provides the core logic for launching Minecraft Java Edition. It handles everything the official launcher does — downloading game files, assets, Java runtimes, mod loaders, and spawning the game process — exposing a clean async API with real-time progress events.

## Features

- **Vanilla & modded** — supports Forge, NeoForge, Fabric, LegacyFabric, and Quilt out of the box
- **Automatic downloads** — game JARs, libraries, assets, and Java runtimes fetched on demand
- **Integrity checks** — SHA-1 verification before every launch
- **Instance isolation** — each instance gets its own game directory
- **Event-driven** — progress, speed, logs, and exit codes delivered over a `tokio` channel
- **Async** — built on `tokio`; non-blocking from download to process management

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
minecraft-java-rs-core = { git = "https://github.com/fitzxel/minecraft-java-rs-core" }
tokio = { version = "1", features = ["full"] }
```

## Quick start

```rust
use minecraft_java_rs_core::{
    launcher::{
        events::LaunchEvent,
        options::{JavaOptions, LaunchOptions, LoaderConfig, MemoryConfig, ScreenConfig},
        Launcher,
    },
    models::{loader::LoaderType, minecraft::Authenticator},
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    // 1. Configure the authenticator (offline mode)
    let auth = Authenticator {
        access_token: "offline".into(),
        name: "Steve".into(),
        uuid: "00000000-0000-0000-0000-000000000001".into(),
        xbox_account: None,
        user_properties: None,
        client_id: None,
        client_token: None,
    };

    // 2. Build launch options
    let options = LaunchOptions {
        path: "./minecraft".into(),
        version: "1.21.1".into(),
        authenticator: auth,
        memory: MemoryConfig {
            min: "2G".into(),
            max: "4G".into(),
        },
        loader: LoaderConfig {
            enable: false,
            ..Default::default()
        },
        timeout_secs: 30,
        download_concurrency: 10,
        java: JavaOptions::default(),
        screen: ScreenConfig::default(),
        verify: false,
        game_args: vec![],
        jvm_args: vec![],
        instance: None,
        url: None,
        mcp: None,
        intel_enabled_mac: false,
        bypass_offline: true,
    };

    // 3. Create launcher and event channel
    let launcher = Launcher::new(options);
    let (tx, mut rx) = mpsc::channel::<LaunchEvent>(512);

    // 4. Spawn event listener
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                LaunchEvent::Progress { downloaded, total, kind } => {
                    println!("[{kind}] {downloaded}/{total}");
                }
                LaunchEvent::Data(line) => println!("[MC] {line}"),
                LaunchEvent::Close(code) => println!("Exited with code {code}"),
                _ => {}
            }
        }
    });

    // 5. Launch
    let mut child = launcher.start(tx).await.expect("Failed to launch");
    child.wait().await.ok();
}
```

## With a mod loader

Set the `loader` field in `LaunchOptions`:

```rust
use minecraft_java_rs_core::models::loader::LoaderType;
use minecraft_java_rs_core::launcher::options::LoaderConfig;

// Fabric — latest build
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::Fabric),
    build: "latest".into(),
    path: None,
    config: None,
};

// NeoForge — specific build
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::NeoForge),
    build: "21.1.231".into(),
    path: None,
    config: None,
};

// Forge — recommended build
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::Forge),
    build: "recommended".into(),
    path: None,
    config: None,
};
```

Available loader types: `Forge`, `NeoForge`, `Fabric`, `LegacyFabric`, `Quilt`.

Valid `build` values: `"latest"`, `"recommended"`, or an exact version string (e.g. `"0.19.2"`).

## Download only (no launch)

Download and verify all game files without spawning the game process:

```rust
launcher.download_game(tx).await.expect("Download failed");
```

## Listening to events

All progress and game output is delivered as `LaunchEvent` variants over a `tokio::sync::mpsc` channel:

| Event | Description |
|---|---|
| `Progress { downloaded, total, kind }` | Download batch progress. `kind` is e.g. `"libraries"`, `"assets"`, `"java"` |
| `Speed(f64)` | Current download speed in bytes/sec |
| `Estimated(f64)` | Estimated seconds remaining |
| `Check { current, total, kind }` | File integrity check progress |
| `Extract(String)` | A file is being extracted from an archive |
| `Patch(String)` | A Forge processor step is running |
| `GameDownloadFinished` | All files downloaded and verified |
| `Data(String)` | A line of stdout/stderr from the Minecraft process |
| `Close(i32)` | Minecraft exited; carries the exit code |
| `Error(String)` | Non-fatal warning from the launcher |

```rust
while let Some(event) = rx.recv().await {
    match event {
        LaunchEvent::Progress { downloaded, total, kind } => {
            let pct = downloaded * 100 / total.max(1);
            println!("[{kind}] {pct}%");
        }
        LaunchEvent::Speed(bps) => {
            println!("Speed: {:.1} MB/s", bps / 1_048_576.0);
        }
        LaunchEvent::GameDownloadFinished => {
            println!("All files ready!");
        }
        LaunchEvent::Data(line) => {
            println!("[MC] {line}");
        }
        LaunchEvent::Close(code) => {
            println!("Game exited: {code}");
            break;
        }
        _ => {}
    }
}
```

## Running the built-in example

The repo ships an example launcher at `examples/launch.rs`:

```sh
# Vanilla 1.21.1
cargo run --example launch -- --version 1.21.1 --username Steve

# With Fabric (latest)
cargo run --example launch -- --version 1.21.1 --username Steve --loader-type fabric

# With NeoForge (specific build)
cargo run --example launch -- --version 1.21.1 --username Steve --loader-type neoforge --loader-build 21.1.231

# With Forge (recommended)
cargo run --example launch -- --version 1.21.1 --username Steve --loader-type forge --loader-build recommended

# With Quilt (latest)
cargo run --example launch -- --version 1.21.1 --username Steve --loader-type quilt

# Use a named instance (saves to ./minecraft/instances/myworld)
cargo run --example launch -- --version 1.21.1 --username Steve --instance myworld

# Download only (no game launch)
cargo run --example launch -- --version 1.21.1 --only-download

# Custom memory and auto-close after 60 seconds
cargo run --example launch -- --version 1.21.1 --username Steve --min-mem 2G --max-mem 6G --auto-close 60
```

### Example options

| Flag | Default | Description |
|---|---|---|
| `--version <VERSION>` | `1.20.4` | Minecraft version |
| `--username <NAME>` | `Player` | Offline username |
| `--path <PATH>` | `./minecraft` | Root game directory |
| `--instance <NAME>` | — | Instance name (`<path>/instances/<name>`) |
| `--min-mem <SIZE>` | `2G` | JVM minimum heap |
| `--max-mem <SIZE>` | `4G` | JVM maximum heap |
| `--loader-type <TYPE>` | — | `forge` \| `neoforge` \| `fabric` \| `legacyfabric` \| `quilt` |
| `--loader-build <BUILD>` | `latest` | `latest` \| `recommended` \| exact version |
| `--only-download` | — | Download files without launching |
| `--auto-close <SECS>` | — | Kill Minecraft after N seconds |

## A note on AI-assisted development

My programming background is mainly web development. While I've been picking up Rust along the way, this project was born out of necessity rather than passion for the language or a desire to practice it. I needed a solid Minecraft Java launcher core in Rust and there are very few options that actually cover what this project aims to do.

Because of that, the code is — in principle — 100% AI-generated. If this ends up public it isn't a "look what I can build" statement. It's closer to "I built this, it's useful to me, and maybe it'll be useful to someone else too."

Bug reports, improvements, and pull requests are genuinely welcome.

## License

MIT
