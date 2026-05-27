---
name: minecraft-java-rs-integration
description: >
  Guide and code generator for integrating the minecraft-java-rs-core Rust library
  into external projects. Use this skill whenever someone wants to use
  minecraft-java-rs-core in their own Rust code, asks how to set up a Minecraft
  launcher with this library, wants to add mod loader support (Fabric, Forge,
  NeoForge, Quilt), needs Microsoft (MSA) authentication wired up, wants to listen
  to launch events, or asks questions like "how do I use this library", "show me
  how to launch Minecraft with Rust", "integrate minecraft-java-rs-core into my
  project", or "generate boilerplate for the launcher". Always use this skill for
  any integration question about this library — even if the user only mentions one
  aspect (e.g. "just show me how auth works"), provide the full relevant snippet
  so they have a working starting point.
---

# minecraft-java-rs-core — Integration Guide

This skill generates working Rust code and explains how to integrate
`minecraft-java-rs-core` into an external project.

---

## 1. Dependency setup

Add to `Cargo.toml`. The library is hosted on GitHub (private repo):

```toml
[dependencies]
minecraft-java-rs-core = { git = "https://github.com/fitzxel/minecraft-java-rs-core" }
tokio = { version = "1", features = ["full"] }
```

For Microsoft authentication, add the recommended auth crate:

```toml
minecraft-msa-auth = { git = "https://github.com/minecraft-rs/minecraft-msa-auth" }
```

---

## 2. Core concepts

| Concept | Type | Purpose |
|---|---|---|
| `LaunchOptions` | struct | Full configuration for a launch session |
| `Authenticator` | struct | Player identity (offline or Microsoft) |
| `LoaderConfig` | struct | Mod loader selection and build |
| `Launcher` | struct | Entry point — call `.start()` or `.download_game()` |
| `LaunchEvent` | enum | Progress, logs, and exit code delivered over a channel |

The typical flow is: **build `LaunchOptions` → create `Launcher` → open a `mpsc` channel → call `.start(tx)`** and stream events from the receiver.

---

## 3. Offline mode (no auth required)

```rust
use minecraft_java_rs_core::{
    launcher::{events::LaunchEvent, options::LaunchOptions, Launcher},
    models::minecraft::Authenticator,
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let auth = Authenticator {
        access_token: "offline".into(),
        name: "Steve".into(),
        uuid: "00000000-0000-0000-0000-000000000001".into(),
        xbox_account: None,
        user_properties: None,
        meta: None,
        client_id: None,
        client_token: None,
    };

    let options = LaunchOptions {
        path: "./minecraft".into(),
        version: "1.21.1".into(),
        authenticator: auth,
        bypass_offline: true, // redirects Mojang auth to invalid domain
        ..Default::default()
    };

    let (tx, mut rx) = mpsc::channel::<LaunchEvent>(512);

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                LaunchEvent::Data(line) => println!("[MC] {line}"),
                LaunchEvent::Close(code) => println!("Exited: {code}"),
                _ => {}
            }
        }
    });

    let mut child = Launcher::new(options).start(tx).await.unwrap();
    child.wait().await.ok();
}
```

> `bypass_offline: true` is needed for offline play — it redirects Mojang's
> session servers to an invalid domain so the game doesn't block the login.

---

## 4. Microsoft authentication

Uses [`minecraft-msa-auth`](https://github.com/minecraft-rs/minecraft-msa-auth).
The flow is: device code → Microsoft token → Xbox → Minecraft token → profile.

```rust
use minecraft_java_rs_core::models::minecraft::Authenticator;
use minecraft_msa_auth::MinecraftAuthorizationFlow;
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, DeviceAuthorizationUrl, TokenUrl};

async fn microsoft_auth() -> Authenticator {
    // Register an Azure app at https://portal.azure.com to get a client ID.
    let client = BasicClient::new(
        ClientId::new("YOUR_AZURE_CLIENT_ID".into()),
        None,
        AuthUrl::new("https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize".into()).unwrap(),
        Some(TokenUrl::new("https://login.microsoftonline.com/consumers/oauth2/v2.0/token".into()).unwrap()),
    )
    .set_device_authorization_url(
        DeviceAuthorizationUrl::new("https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode".into()).unwrap(),
    );

    let http = reqwest::Client::new();
    let flow = MinecraftAuthorizationFlow::new(http.clone());

    // 1. Get device code — show the URL and code to the user
    let details = flow.get_device_code(&client).await.unwrap();
    println!("Open {} and enter code: {}", details.verification_uri(), details.user_code().secret());

    // 2. Poll until the user logs in
    let msa_token = flow.poll_device_code(details, &client).await.unwrap();

    // 3. Exchange MSA token → Minecraft profile
    let mc_profile = flow.get_minecraft_profile(&msa_token).await.unwrap();

    Authenticator {
        access_token: mc_profile.access_token,
        name: mc_profile.username,
        uuid: mc_profile.uuid.to_string(),
        xbox_account: None,
        user_properties: None,
        meta: None,
        client_id: Some("YOUR_AZURE_CLIENT_ID".into()),
        client_token: None,
    }
}
```

---

## 5. Mod loaders

Set `loader` in `LaunchOptions`. All loaders auto-download and install on first launch.

```rust
use minecraft_java_rs_core::{
    launcher::options::LoaderConfig,
    models::loader::LoaderType,
};

// Fabric — latest build
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::Fabric),
    build: "latest".into(),
    ..Default::default()
};

// NeoForge — specific version
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::NeoForge),
    build: "21.1.231".into(),
    ..Default::default()
};

// Forge — recommended stable build
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::Forge),
    build: "recommended".into(),
    ..Default::default()
};

// Quilt — latest
let loader = LoaderConfig {
    enable: true,
    loader_type: Some(LoaderType::Quilt),
    build: "latest".into(),
    ..Default::default()
};
```

Available types: `Forge`, `NeoForge`, `Fabric`, `LegacyFabric`, `Quilt`.  
Valid `build` values: `"latest"`, `"recommended"`, or an exact version string.

---

## 6. LaunchEvent — full event reference

Receive all events over the `mpsc::Receiver<LaunchEvent>`:

```rust
while let Some(event) = rx.recv().await {
    match event {
        LaunchEvent::Progress { downloaded, total, kind } => {
            // kind = "libraries" | "assets" | "java" | ...
            let pct = downloaded * 100 / total.max(1);
            println!("[{kind}] {pct}%");
        }
        LaunchEvent::Speed(bps) => {
            println!("Speed: {:.1} MB/s", bps / 1_048_576.0);
        }
        LaunchEvent::Estimated(secs) => {
            println!("ETA: {secs:.0}s");
        }
        LaunchEvent::Check { current, total, kind } => {
            // File integrity check in progress
            println!("[verify/{kind}] {current}/{total}");
        }
        LaunchEvent::Extract(name) => {
            println!("[extract] {name}");
        }
        LaunchEvent::Patch(msg) => {
            // Forge processor step
            println!("[patch] {msg}");
        }
        LaunchEvent::JavaProgress { downloaded, total } => {
            println!("[java] {downloaded}/{total}");
        }
        LaunchEvent::GameDownloadFinished => {
            println!("All files ready.");
        }
        LaunchEvent::Data(line) => {
            // stdout/stderr line from the running Minecraft process
            println!("[MC] {line}");
        }
        LaunchEvent::Close(code) => {
            println!("Minecraft exited with code {code}");
            break;
        }
        LaunchEvent::Error(msg) => {
            eprintln!("[warn] {msg}");
        }
    }
}
```

---

## 7. Common options reference

```rust
LaunchOptions {
    path: "./minecraft".into(),          // root data directory
    version: "1.21.1".into(),            // or "latest_release" / "lr"
    authenticator: auth,

    // Memory
    memory: MemoryConfig {
        min: "2G".into(),
        max: "4G".into(),
    },

    // Java — leave default to auto-download the right JRE
    java: JavaOptions {
        path: None,           // Some(PathBuf::from("/usr/bin/java")) to use system Java
        version: None,        // force e.g. Some("21".into())
        image_type: "jre".into(),
    },

    // Window size
    screen: ScreenConfig {
        width: Some(1280),
        height: Some(720),
        fullscreen: false,
    },

    // Named instance — saves game data to <path>/instances/<name>/
    instance: Some("survival-world".into()),

    // Extra JVM / game args
    jvm_args: vec!["-XX:+UseZGC".into()],
    game_args: vec!["--server".into(), "play.example.com".into()],

    // Other
    timeout_secs: 30,
    download_concurrency: 10,
    bypass_offline: false,   // set true for offline/cracked play
    verify: false,           // re-verify SHA-1 after every download
    ..Default::default()
}
```

---

## 8. Download only (no launch)

Pre-cache all game files without spawning the process:

```rust
launcher.download_game(tx).await?;
```

---

## 9. Version aliases

| Alias | Resolves to |
|---|---|
| `"latest_release"` / `"lr"` / `"r"` | Latest stable release |
| `"latest_snapshot"` / `"ls"` / `"s"` | Latest snapshot |
| Any other string | Treated as an exact version (e.g. `"1.20.4"`) |
