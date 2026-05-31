---
name: minecraft-java-rs-integration
metadata:
  author: Fitzxel
  version: "0.1.0"
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
| `Launcher` | struct | Entry point — owns `game_data` state after download |
| `LaunchEvent` | enum | Progress, logs, and exit code delivered over a channel |

`Launcher` always needs a `mut` binding. There are two usage patterns:

**All-in-one:** `download_game` + `launch` in a single call.
```
build LaunchOptions → mut Launcher::new → mpsc channel → launcher.start(tx)
```

**Split:** download once, re-launch without re-downloading (e.g. restart after crash).
```
launcher.download_game(tx).await?   // stores GameData internally
launcher.launch(tx).await?          // reads from self.game_data or disk cache
```

`launch` can also be called without a prior `download_game` in the same session — it will load the persisted cache from disk. If no cache exists it returns `LaunchError::GameDataNotReady`.

---

## 3. Offline mode (no auth required)

```rust
use minecraft_java_rs_core::{
    launcher::{events::LaunchEvent, options::LaunchOptions, Launcher},
    models::minecraft::Authenticator,
    utils::auth::offline_uuid,
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let uuid = offline_uuid("Steve");
    let auth = Authenticator {
        access_token: "offline".into(),
        name: "Steve".into(),
        uuid,
        xbox_account: None,
        user_properties: None,
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

    let mut launcher = Launcher::new(options);
    let mut child = launcher.start(tx).await.unwrap();
    child.wait().await.ok();
}
```

> `bypass_offline: true` is needed for offline play — it redirects Mojang's
> session servers to an invalid domain so the game doesn't block the login.

---

## 4. Microsoft authentication

Uses [`minecraft-msa-auth`](https://github.com/minecraft-rs/minecraft-msa-auth).

```toml
minecraft-msa-auth = { git = "https://github.com/minecraft-rs/minecraft-msa-auth" }
```

**What the library does:** takes a raw Microsoft OAuth2 access token string and handles the
Xbox Live → XSTS → Minecraft login chain. It does **not** do the OAuth2 device-code step —
you handle that yourself with plain HTTP calls.

**API surface:**
```rust
MinecraftAuthorizationFlow::new(http_client: reqwest::Client) -> Self
flow.exchange_microsoft_token(ms_access_token: impl AsRef<str>)
    -> Result<MinecraftAuthenticationResponse, MinecraftAuthorizationError>

// MinecraftAuthenticationResponse fields (via getters):
mc_token.access_token()  // &MinecraftAccessToken — use .as_ref() for the raw &str
mc_token.username()      // Xbox UUID string — NOT the Minecraft display name or UUID
mc_token.expires_in()    // u32 seconds
```

> `mc_token.username()` is the **Xbox UUID**, not the Minecraft player name or UUID.
> After `exchange_microsoft_token` you must separately call the Minecraft profile API
> to get the actual display name and UUID.

### reqwest version conflict

`minecraft-msa-auth` depends on **reqwest 0.13**, which is a different crate version than
the 0.12 used by `minecraft-java-rs-core`. If your project already depends on reqwest 0.12
you must alias reqwest 0.13 under a different name to avoid type mismatches:

```toml
# your existing reqwest (0.12) stays unchanged
reqwest = { version = "0.12", ... }

# alias for passing a Client to MinecraftAuthorizationFlow
reqwest_v13 = { package = "reqwest", version = "0.13", default-features = false, features = ["json", "rustls"] }
```

Then in code: `use reqwest_v13::Client as MsaClient;`

### Full flow (device code — no browser redirect needed)

Register your Azure app at [portal.azure.com](https://portal.azure.com): public client,
no redirect URI needed for device code, scope: `XboxLive.signin offline_access`.

```rust
use std::time::Duration;
use minecraft_java_rs_core::{models::minecraft::Authenticator, utils::auth::offline_uuid};
use minecraft_msa_auth::MinecraftAuthorizationFlow;
use serde::Deserialize;

const CLIENT_ID: &str = "YOUR_AZURE_CLIENT_ID";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str     = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String, user_code: String, verification_uri: String,
    expires_in: u64, interval: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenPoll { Success { access_token: String }, Pending { error: String } }

#[derive(Deserialize)]
struct McProfile { id: String, name: String }

async fn microsoft_auth() -> Result<Authenticator, Box<dyn std::error::Error>> {
    let http = reqwest::Client::new(); // reqwest 0.12 for OAuth HTTP calls

    // 1. Request a device code
    let dc: DeviceCodeResponse = http.post(DEVICE_CODE_URL)
        .form(&[("client_id", CLIENT_ID), ("scope", "XboxLive.signin offline_access")])
        .send().await?.error_for_status()?.json().await?;

    println!("Open: {}\nEnter code: {}", dc.verification_uri, dc.user_code);

    // 2. Poll until the user logs in
    let interval = Duration::from_secs(dc.interval.max(5));
    let deadline = std::time::Instant::now() + Duration::from_secs(dc.expires_in);

    let msa_token = loop {
        tokio::time::sleep(interval).await;
        if std::time::Instant::now() > deadline {
            return Err("device code expired".into());
        }
        let resp: TokenPoll = http.post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", dc.device_code.as_str()),
            ])
            .send().await?.json().await?;
        match resp {
            TokenPoll::Success { access_token } => break access_token,
            TokenPoll::Pending { error } if error == "authorization_pending" => continue,
            TokenPoll::Pending { error } => return Err(error.into()),
        }
    };

    // 3. MSA token → Minecraft token (Xbox Live + XSTS handled internally)
    // Note: MinecraftAuthorizationFlow requires a reqwest 0.13 Client.
    // If your project uses reqwest 0.12, alias it (see "reqwest version conflict" above).
    let mc_flow = MinecraftAuthorizationFlow::new(reqwest_v13::Client::new());
    let mc_token = mc_flow.exchange_microsoft_token(&msa_token).await?;
    let minecraft_access_token = mc_token.access_token().as_ref().to_owned();

    // 4. Fetch real Minecraft name + UUID via profile API
    // mc_token.username() is the Xbox UUID — do NOT use it as the player UUID.
    // Mojang returns the UUID without dashes; add them back.
    let profile: McProfile = http.get(MC_PROFILE_URL)
        .bearer_auth(&minecraft_access_token)
        .send().await?.error_for_status()?.json().await?;

    let raw = &profile.id;
    let uuid = if raw.len() == 32 {
        format!("{}-{}-{}-{}-{}", &raw[..8], &raw[8..12], &raw[12..16], &raw[16..20], &raw[20..])
    } else { raw.clone() };

    Ok(Authenticator {
        access_token: minecraft_access_token,
        name: profile.name,
        uuid,
        xbox_account: None,
        user_properties: None,
        client_id: Some(CLIENT_ID.into()),
        client_token: None,
    })
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

## 8. Launcher flow patterns

### All-in-one (download + launch)

```rust
let mut launcher = Launcher::new(options);
let mut child = launcher.start(tx).await?;
let code = child.wait().await?.code().unwrap_or(-1);
let _ = close_tx.send(LaunchEvent::Close(code)).await;
```

### Download only (pre-cache, no launch)

```rust
let mut launcher = Launcher::new(options);
launcher.download_game(tx).await?;
// All files are on disk; launcher.game_data() now returns Some(&GameData)
```

### Launch without re-downloading (split flow)

Useful for restarting after a crash, or launching from a pre-downloaded installation:

```rust
// First run — download and launch
let mut launcher = Launcher::new(options.clone());
launcher.download_game(tx.clone()).await?;
let mut child = launcher.launch(tx).await?;
child.wait().await.ok();

// Second run — reuse the same launcher (game_data already in memory)
let mut child2 = launcher.launch(tx2).await?;
child2.wait().await.ok();
```

Or across separate sessions (reads persisted cache from disk automatically):

```rust
// New session — no download_game call needed if files are already present
let mut launcher = Launcher::new(options);
// launcher.game_data() is None here, but launch() loads from disk cache
let mut child = launcher.launch(tx).await?;  // errors with GameDataNotReady if no cache
```

### Error: GameDataNotReady

`launch` returns `LaunchError::GameDataNotReady` when neither `self.game_data` is set
nor a persisted cache file exists on disk. Always call `download_game` (or `start`)
at least once before calling `launch` standalone.

---

## 9. Version aliases

| Alias | Resolves to |
|---|---|
| `"latest_release"` / `"lr"` / `"r"` | Latest stable release |
| `"latest_snapshot"` / `"ls"` / `"s"` | Latest snapshot |
| Any other string | Treated as an exact version (e.g. `"1.20.4"`) |
