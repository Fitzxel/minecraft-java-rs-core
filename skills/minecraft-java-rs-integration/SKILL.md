---
name: minecraft-java-rs-integration
metadata:
  author: Fitzxel
  version: "0.4.0"
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

Uses [`minecraft-msa-auth`](https://github.com/minecraft-rs/minecraft-msa-auth) (crates.io):

```toml
minecraft-msa-auth = "0.4"
```

**What the library does:** takes a raw Microsoft OAuth2 access token string and handles the
Xbox Live → XSTS → Minecraft login chain. It does **not** do the OAuth2 flow itself —
you handle that (device code or browser redirect) and then pass the MSA access token in.

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
>
> The Minecraft profile API returns the UUID **without dashes** — format it yourself:
> `"550e8400e29b41d4a716446655440000"` → `"550e8400-e29b-41d4-a716-446655440000"`

**reqwest version note:** `minecraft-msa-auth 0.4` depends on reqwest 0.13. If your project
already uses reqwest 0.12 (e.g. alongside `minecraft-java-rs-core`) you must alias the
dependency to avoid type mismatches when passing the `Client` to `MinecraftAuthorizationFlow`:

```toml
reqwest_v13 = { package = "reqwest", version = "0.13", default-features = false, features = ["json", "rustls"] }
```

Then use `reqwest_v13::Client::new()` instead of `reqwest::Client::new()` when constructing
the flow. If your project already uses reqwest 0.13, no alias is needed.

---

### Flow A — Device code (no browser required)

Ideal for CLI tools. The user visits a URL and enters a code; your app polls until done.
Azure app: public client, no redirect URI needed.

```rust
use std::time::Duration;
use minecraft_java_rs_core::{models::minecraft::Authenticator, utils::auth::offline_uuid};
use minecraft_msa_auth::MinecraftAuthorizationFlow;
use serde::Deserialize;

const CLIENT_ID: &str = "YOUR_AZURE_CLIENT_ID";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str      = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

#[derive(Deserialize)]
struct DeviceCodeResponse { device_code: String, user_code: String,
    verification_uri: String, expires_in: u64, interval: u64 }

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenPoll { Success { access_token: String }, Pending { error: String } }

#[derive(Deserialize)]
struct McProfile { id: String, name: String }

async fn microsoft_auth_device_code() -> Result<Authenticator, Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();

    // 1. Request device code
    let dc: DeviceCodeResponse = http.post(DEVICE_CODE_URL)
        .form(&[("client_id", CLIENT_ID), ("scope", "XboxLive.signin offline_access")])
        .send().await?.error_for_status()?.json().await?;

    println!("Open: {}\nEnter code: {}", dc.verification_uri, dc.user_code);

    // 2. Poll until the user completes login
    let interval = Duration::from_secs(dc.interval.max(5));
    let deadline = std::time::Instant::now() + Duration::from_secs(dc.expires_in);
    let msa_token = loop {
        tokio::time::sleep(interval).await;
        if std::time::Instant::now() > deadline { return Err("device code expired".into()); }
        match http.post(TOKEN_URL)
            .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", CLIENT_ID), ("device_code", dc.device_code.as_str())])
            .send().await?.json::<TokenPoll>().await?
        {
            TokenPoll::Success { access_token } => break access_token,
            TokenPoll::Pending { error } if error == "authorization_pending" => continue,
            TokenPoll::Pending { error } => return Err(error.into()),
        }
    };

    // 3. MSA token → Minecraft token
    let mc_flow = MinecraftAuthorizationFlow::new(reqwest::Client::new());
    let mc_token = mc_flow.exchange_microsoft_token(&msa_token).await?;

    // 4. Fetch real player name + UUID
    let profile: McProfile = http.get(MC_PROFILE_URL)
        .bearer_auth(mc_token.access_token().as_ref())
        .send().await?.error_for_status()?.json().await?;

    let id = &profile.id;
    let uuid = format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]);

    Ok(Authenticator {
        access_token: mc_token.access_token().as_ref().to_owned(),
        name: profile.name,
        uuid,
        xbox_account: None, user_properties: None,
        client_id: Some(CLIENT_ID.into()), client_token: None,
    })
}
```

---

### Flow B — Browser redirect (desktop / GUI apps)

Ideal for apps with a UI (e.g. Tauri). Opens the system browser; a local HTTP server catches
the OAuth callback code. Yields a `refresh_token` for silent re-auth.
Azure app: public client, redirect URI `http://localhost:7878/callback`.

```toml
open = "5"          # opens the default browser
tiny_http = "0.12"  # minimal local callback server
```

```rust
use minecraft_msa_auth::MinecraftAuthorizationFlow;
use reqwest::Client;
use serde_json::Value;
use tiny_http::{Header, Response, Server};

const CLIENT_ID: &str    = "YOUR_AZURE_CLIENT_ID";
const REDIRECT_URI: &str = "http://localhost:7878/callback";
const TOKEN_URL: &str    = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

async fn microsoft_auth_browser() -> Result<(String, String, String, String), String> {
    // 1. Open Microsoft login in the system browser
    let auth_url = format!(
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize\
         ?client_id={CLIENT_ID}&response_type=code&redirect_uri={REDIRECT_URI}\
         &scope=XboxLive.signin%20offline_access&prompt=select_account"
    );
    open::that(auth_url).map_err(|e| e.to_string())?;

    // 2. Catch the callback code on a local HTTP server
    let server = Server::http("127.0.0.1:7878").map_err(|e| e.to_string())?;
    let code = loop {
        let req = server.recv().map_err(|e| e.to_string())?;
        let url = req.url().to_string();
        if url.starts_with("/callback") {
            if let Some(code) = url.split("code=").nth(1) {
                let code = code.split('&').next().unwrap_or("").to_string();
                let _ = req.respond(Response::from_string("Login successful. You can close this window.")
                    .with_header(Header::from_bytes(b"Content-Type", b"text/html").unwrap()));
                break code;
            }
        }
    };

    // 3. Exchange code → MSA access token + refresh token
    let http = Client::new();
    let res: Value = http.post(TOKEN_URL)
        .form(&[("client_id", CLIENT_ID), ("code", code.as_str()),
                ("grant_type", "authorization_code"), ("redirect_uri", REDIRECT_URI)])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let msa_access  = res["access_token"].as_str().ok_or("no access_token")?.to_string();
    let msa_refresh = res["refresh_token"].as_str().ok_or("no refresh_token")?.to_string();

    // 4. MSA token → Minecraft token
    let mc_flow = MinecraftAuthorizationFlow::new(Client::new());
    let mc_token = mc_flow.exchange_microsoft_token(&msa_access)
        .await.map_err(|e| e.to_string())?;

    // 5. Fetch real player name + UUID
    let profile: Value = http.get(MC_PROFILE_URL)
        .bearer_auth(mc_token.access_token().as_ref())
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let name = profile["name"].as_str().ok_or("no name")?.to_string();
    let id   = profile["id"].as_str().ok_or("no id")?.to_string();
    let uuid = format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]);

    // Returns (mc_access_token, msa_refresh_token, name, uuid)
    // Store msa_refresh_token to silently re-auth later (see token refresh below).
    Ok((mc_token.access_token().as_ref().to_owned(), msa_refresh, name, uuid))
}
```

### Token refresh (browser flow)

Use the stored `msa_refresh_token` to get a new Minecraft token without re-login:

```rust
async fn refresh_microsoft_token(refresh_token: &str)
    -> Result<(String, String), String>  // (new_mc_access_token, new_msa_refresh_token)
{
    let http = Client::new();
    let res: Value = http.post(TOKEN_URL)
        .form(&[("client_id", CLIENT_ID), ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"), ("scope", "XboxLive.signin offline_access")])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    if let Some(err) = res.get("error") {
        return Err(format!("refresh error: {err}"));
    }

    let new_msa_access  = res["access_token"].as_str().ok_or("no access_token")?.to_string();
    let new_msa_refresh = res["refresh_token"].as_str().ok_or("no refresh_token")?.to_string();

    let mc_flow = MinecraftAuthorizationFlow::new(Client::new());
    let mc_token = mc_flow.exchange_microsoft_token(&new_msa_access)
        .await.map_err(|e| e.to_string())?;

    Ok((mc_token.access_token().as_ref().to_owned(), new_msa_refresh))
}
```

> Register your Azure app at [portal.azure.com](https://portal.azure.com):
> public client, scope `XboxLive.signin offline_access`.
> Flow A: no redirect URI. Flow B: redirect URI `http://localhost:7878/callback`.

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
    bypass_offline: false,      // set true for offline/cracked play
    verify: false,              // re-verify SHA-1 after every download
    skip_bundle_check: false,   // set true to skip integrity check when gameData.json exists
    force_ipv4: false,          // set true to force IPv4 (fixes broken-IPv6 download errors)
    dns: None,                  // Some("1.1.1.1".parse()?) to resolve via DNS-over-HTTPS
    ..Default::default()
}
```

### `force_ipv4` — fixing connection errors that vanish under a VPN

Default `false`. When `true`, all launcher HTTP traffic (downloads **and** metadata
fetches) is restricted to IPv4 by filtering DNS results to A records before any
connection is attempted — AAAA (IPv6) addresses are ignored.

**When to enable it.** Some networks advertise IPv6 (AAAA records) but have a broken
IPv6 route. Browsers paper over this with Happy Eyeballs (they race IPv4 and IPv6 and
use whichever connects first); the underlying HTTP stack here does not, so it can hang
or fail on the dead IPv6 path. The classic symptoms:

- Downloads fail with `download error: could not reach <url> (...)` — a connection
  error with **no HTTP status** (the request never reached the server), often citing
  a cause like *"Network is unreachable"* or *"Temporary failure in name resolution"*.
- The **same URL opens fine in a browser**, and the download **works over a VPN**
  (the VPN provides a working IPv6 route or forces IPv4).

In that situation, set `force_ipv4: true` and the downloads succeed without a VPN.

> Leave it `false` by default. Forcing IPv4 would break users on genuinely
> **IPv6-only** networks, so enable it only as a remedy (or expose it as a toggle /
> "having download problems?" option in your UI).

### `dns` — bypassing a broken or hijacked ISP resolver

Default `None` (system resolver). Set `dns: Some(ip)` — e.g. `Some("1.1.1.1".parse()?)`
for Cloudflare — to resolve **every** hostname through **DNS-over-HTTPS** against that
resolver IP instead of the operating system's. The launcher connects to the resolver by
its **literal IP** (so no system lookup is needed to bootstrap) and issues JSON DoH
queries to `https://<ip>/dns-query`.

**When to enable it.** `force_ipv4` fixes a broken *IPv6 route*; `dns` fixes a broken
*name resolution* path. Because DoH rides HTTPS to a literal IP, it bypasses both:

- **ISP DNS hijacking / poisoning** — the resolver returns wrong, filtered, or `NXDOMAIN`
  answers for Mojang/asset hosts.
- **Port-53 blocking / interception** — networks that drop or rewrite plain UDP/TCP 53
  traffic (so merely *changing* the system nameserver to 1.1.1.1 doesn't help).

Typical symptoms are the same "works over a VPN, fails on this network" pattern, but the
underlying cause shows up as a resolution failure (e.g. *"Temporary failure in name
resolution"*, or a connection to a clearly wrong IP) rather than an unreachable route.

`dns` **composes with `force_ipv4`**: when both are set, only A records are requested, so
you get Cloudflare resolution *and* IPv4-only connections. Any DoH-capable resolver IP
with a valid certificate for its own address works (Cloudflare `1.1.1.1`, Google
`8.8.8.8`, Quad9 `9.9.9.9`); `1.1.1.1` is the typical choice.

> Like `force_ipv4`, leave it `None` by default and surface it as a remedy. Each
> resolution becomes an HTTPS round-trip to the resolver, so only opt in when the
> system resolver is actually the problem.

**Verifying it works.** Resolution is invisible from the outside — a successful download
doesn't prove DoH was used. Set the `MJRS_DNS_DEBUG=1` environment variable to print one
line per resolution to stderr:

```
[dns] DoH via 1.1.1.1 → resources.download.minecraft.net = [13.107.253.33, 13.107.226.33]
[dns] DoH via 1.1.1.1 → libraries.minecraft.net = ERROR (could not establish connection)
```

It is silent unless the variable is set (a single `getenv` per lookup otherwise), so it is
safe to leave shippable — handy as a "run with this and send me the output" support tool.
Only the DoH path logs; the system/`force_ipv4` resolvers don't.

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

### Fast re-launch (skipping bundle check)

`launch()` never runs a bundle check — it only reads `gameData.json` and spawns
the process. **This is the preferred fast-launch path** when the caller knows the
game is already installed: just call `launch()` directly without a prior
`download_game()` call. It loads the persisted cache from disk automatically.

```rust
// Fastest possible re-launch — no network, no SHA-1 checks, no download step.
let mut launcher = Launcher::new(options);
let mut child = launcher.launch(tx).await?;  // reads gameData.json from disk
child.wait().await.ok();
```

`skip_bundle_check: true` on `LaunchOptions` is only relevant when the caller
goes through `download_game()` and still wants to avoid the SHA-1 scan (e.g.
when using `start()` or building a fresh `Launcher` each session). If the cache
is absent it falls through to the full download path silently.

### Corrupt install detection

After the process exits, call `Launcher::is_corrupt_crash` with the exit code and
the collected `LaunchEvent::Data` log lines. It returns `true` when the exit code
is non-zero **and** the logs contain a known corrupt-install pattern
(`NoClassDefFoundError`, `Unable to access jarfile`, `ZipException`, etc.).
Re-run `download_game()` with `skip_bundle_check: false` to trigger a full
re-verification.

After the process exits, call `Launcher::is_corrupt_crash` with the exit code and
the collected `LaunchEvent::Data` log lines. It returns `true` when the exit code
is non-zero **and** the logs contain a known corrupt-install pattern
(`NoClassDefFoundError`, `Unable to access jarfile`, `ZipException`, etc.).
Re-launch with `skip_bundle_check: false` to trigger a full re-verification.

```rust
use minecraft_java_rs_core::launcher::{LaunchEvent, Launcher};
use tokio::sync::mpsc;

// Collect all Data lines while printing them.
let (tx, mut rx) = mpsc::channel::<LaunchEvent>(512);
let log_task = tokio::spawn(async move {
    let mut logs: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        if let LaunchEvent::Data(line) = &event {
            println!("[MC] {line}");
            logs.push(line.clone());
        }
    }
    logs
});

// Fast launch — trust existing gameData.json.
let options_fast = LaunchOptions { skip_bundle_check: true, ..options.clone() };
let mut launcher = Launcher::new(options_fast);
launcher.download_game(tx.clone()).await?;
let mut child = launcher.launch(tx.clone()).await?;
let code = child.wait().await?.code().unwrap_or(-1);
drop(tx); // close channel so log_task can finish
let logs = log_task.await.unwrap_or_default();

if Launcher::is_corrupt_crash(code, &logs) {
    eprintln!("[!] Minecraft crashed and the cause is likely a corrupt installation.");
    eprintln!("[!] Re-launching with a full integrity check...");

    let (tx2, mut rx2) = mpsc::channel::<LaunchEvent>(512);
    tokio::spawn(async move { while rx2.recv().await.is_some() {} }); // drain
    let options_full = LaunchOptions { skip_bundle_check: false, ..options };
    let mut launcher2 = Launcher::new(options_full);
    launcher2.download_game(tx2.clone()).await?;
    let mut child2 = launcher2.launch(tx2).await?;
    child2.wait().await.ok();
}
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
