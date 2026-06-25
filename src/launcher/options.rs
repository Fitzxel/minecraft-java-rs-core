use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::models::{loader::LoaderType, minecraft::Authenticator};

/// Complete configuration for a launcher session.
///
/// Pass to `Launcher::new()`. Every field except `path`, `version`, and
/// `authenticator` has a sensible default so callers only need to set what
/// differs from the defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LaunchOptions {
    /// Absolute base path for all launcher data
    /// (libraries/, assets/, versions/, runtime/, …).
    pub path: PathBuf,

    /// Minecraft version: concrete (`"1.20.4"`) or alias
    /// (`"latest_release"` / `"r"` / `"lr"` / `"latest_snapshot"` / `"s"` / `"ls"`).
    pub version: String,

    /// Authentication credentials — required.
    pub authenticator: Authenticator,

    /// HTTP request timeout in seconds (default: 10).
    #[serde(default = "defaults::timeout_secs")]
    pub timeout_secs: u64,

    /// Concurrent download workers, clamped to 1–30 (default: 5).
    #[serde(default = "defaults::download_concurrency")]
    pub download_concurrency: u32,

    /// Concurrent SHA-1 verify workers, clamped to 1–16 (default: 4).
    /// Lower than `download_concurrency` to avoid disk seek thrashing on HDDs.
    #[serde(default = "defaults::verify_concurrency")]
    pub verify_concurrency: u32,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub java: JavaOptions,

    #[serde(default)]
    pub loader: LoaderConfig,

    #[serde(default)]
    pub screen: ScreenConfig,

    /// Re-verify SHA-1 integrity of every file after download (default: false).
    #[serde(default)]
    pub verify: bool,

    /// Extra arguments appended after the vanilla game arg list.
    #[serde(default)]
    pub game_args: Vec<String>,

    /// Extra arguments prepended to the JVM arg list.
    #[serde(default)]
    pub jvm_args: Vec<String>,

    /// Named instance for multi-instance support.
    /// When set, data lives under `<path>/instances/<instance>/`.
    #[serde(default)]
    pub instance: Option<String>,

    /// URL for custom additional assets (optional).
    #[serde(default)]
    pub url: Option<String>,

    /// Path to a custom Minecraft JAR (mod compatibility parameter).
    #[serde(default)]
    pub mcp: Option<String>,

    /// macOS only: force x64 Java even on Apple Silicon (Rosetta 2).
    #[serde(default)]
    pub intel_enabled_mac: bool,

    /// Redirect Mojang auth endpoints to an invalid domain so offline
    /// multiplayer works without a valid session (default: false).
    #[serde(default)]
    pub bypass_offline: bool,

    /// When `true` and `gameData.json` already exists on disk, skip the
    /// bundle integrity check and load directly from cache (fast launch).
    /// Falls through to the normal download path when the cache is absent.
    /// Default: `false` (always verify — current behaviour preserved).
    #[serde(default)]
    pub skip_bundle_check: bool,

    /// Force all HTTP traffic over IPv4, ignoring DNS AAAA (IPv6) records.
    ///
    /// Enable when downloads fail with connection errors ("error sending
    /// request") on networks whose IPv6 route is broken even though IPv4 works
    /// — a frequent cause of failures that vanish under a VPN or in a browser
    /// (which does Happy Eyeballs; reqwest does not). Default: `false`.
    #[serde(default)]
    pub force_ipv4: bool,

    /// Resolve every hostname through DNS-over-HTTPS against this resolver IP
    /// instead of the system resolver (e.g. `1.1.1.1` for Cloudflare).
    ///
    /// Connects to the resolver by its literal IP, so it bypasses both ISP DNS
    /// hijacking/poisoning **and** port-53 blocking — failure modes that a plain
    /// nameserver change cannot fix and that typically present as "downloads
    /// work over a VPN but fail on this network". Composes with [`force_ipv4`]:
    /// when both are set, only A records are requested. Default: `None` (use the
    /// system resolver).
    ///
    /// [`force_ipv4`]: Self::force_ipv4
    #[serde(default)]
    pub dns: Option<std::net::IpAddr>,
}

impl LaunchOptions {
    /// Directory where `gameData.json` is stored.
    /// Returns `<path>/instances/<instance>` when instanced, otherwise `<path>`.
    pub fn save_dir(&self) -> PathBuf {
        match &self.instance {
            Some(inst) => self.path.join("instances").join(inst),
            None => self.path.clone(),
        }
    }

    /// Root directory for a specific mod loader's files.
    ///
    /// Returns `<path>/loader/<name>` unless `loader.path` is set explicitly.
    pub fn loader_dir(&self, name: &str) -> PathBuf {
        match &self.loader.path {
            Some(p) => PathBuf::from(p),
            None => self.path.join("loader").join(name),
        }
    }

    /// `download_concurrency` clamped to the valid range 1–64.
    ///
    /// The upper bound matches the ceiling in [`adaptive_concurrency`], which
    /// further reduces the effective value based on available CPU cores.
    pub fn clamped_concurrency(&self) -> u32 {
        self.download_concurrency.clamp(1, 64)
    }

    /// `verify_concurrency` clamped to the valid range 1–16.
    pub fn clamped_verify_concurrency(&self) -> u32 {
        self.verify_concurrency.clamp(1, 16)
    }
}

// ── Memory ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    /// JVM minimum heap (`-Xms`), e.g. `"1G"`, `"512M"` (default: `"1G"`).
    #[serde(default = "defaults::memory_min")]
    pub min: String,
    /// JVM maximum heap (`-Xmx`), e.g. `"2G"` (default: `"2G"`).
    #[serde(default = "defaults::memory_max")]
    pub max: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            min: defaults::memory_min(),
            max: defaults::memory_max(),
        }
    }
}

// ── Screen ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ScreenConfig {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Launch in fullscreen mode (default: false).
    #[serde(default)]
    pub fullscreen: bool,
}

// ── Java ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JavaOptions {
    /// Path to a pre-installed `java` executable — skips automatic download.
    #[serde(default)]
    pub path: Option<PathBuf>,

    /// Force a specific Java major version, e.g. `"21"`.
    #[serde(default)]
    pub version: Option<String>,

    /// Adoptium image type: `"jre"` or `"jdk"` (default: `"jre"`).
    #[serde(default = "defaults::java_image_type")]
    pub image_type: String,
}

impl Default for JavaOptions {
    fn default() -> Self {
        Self {
            path: None,
            version: None,
            image_type: defaults::java_image_type(),
        }
    }
}

// ── Loader ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LoaderConfig {
    /// Which mod loader to install (`None` = no loader).
    pub loader_type: Option<LoaderType>,

    /// Build selector: `"latest"`, `"recommended"`, or an exact version string
    /// (default: `"latest"`).
    #[serde(default = "defaults::loader_build")]
    pub build: String,

    /// Whether to run the loader installer (default: false).
    #[serde(default)]
    pub enable: bool,

    /// Loader-local directory prefix, e.g. `"./loader/forge"`.
    /// Auto-set to `"./loader/<type>"` if not provided.
    #[serde(default)]
    pub path: Option<String>,

    /// Paths populated by the installer after a successful install.
    /// Passed back to the argument builder.
    #[serde(default)]
    pub config: Option<LoaderInnerConfig>,
}

/// File paths set by the mod loader installer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoaderInnerConfig {
    pub java_path: String,
    pub minecraft_jar: String,
    pub minecraft_json: String,
}

// ── Defaults (free functions required by serde's `default = "..."`) ─────────

mod defaults {
    pub fn timeout_secs() -> u64 {
        10
    }
    pub fn download_concurrency() -> u32 {
        5
    }
    pub fn verify_concurrency() -> u32 {
        4
    }
    pub fn memory_min() -> String {
        "1G".into()
    }
    pub fn memory_max() -> String {
        "2G".into()
    }
    pub fn java_image_type() -> String {
        "jre".into()
    }
    pub fn loader_build() -> String {
        "latest".into()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_dir_without_instance() {
        let opts = make_opts(None);
        assert_eq!(opts.save_dir(), PathBuf::from("/mc"));
    }

    #[test]
    fn save_dir_with_instance() {
        let opts = make_opts(Some("test-world".into()));
        assert_eq!(opts.save_dir(), PathBuf::from("/mc/instances/test-world"));
    }

    #[test]
    fn concurrency_clamp() {
        let mut opts = make_opts(None);
        opts.download_concurrency = 0;
        assert_eq!(opts.clamped_concurrency(), 1);
        opts.download_concurrency = 99;
        assert_eq!(opts.clamped_concurrency(), 64);
        opts.download_concurrency = 5;
        assert_eq!(opts.clamped_concurrency(), 5);
    }

    #[test]
    fn verify_concurrency_clamp() {
        let mut opts = make_opts(None);
        opts.verify_concurrency = 0;
        assert_eq!(opts.clamped_verify_concurrency(), 1);
        opts.verify_concurrency = 99;
        assert_eq!(opts.clamped_verify_concurrency(), 16);
        opts.verify_concurrency = 4;
        assert_eq!(opts.clamped_verify_concurrency(), 4);
    }

    #[test]
    fn memory_defaults() {
        let m = MemoryConfig::default();
        assert_eq!(m.min, "1G");
        assert_eq!(m.max, "2G");
    }

    fn make_opts(instance: Option<String>) -> LaunchOptions {
        use crate::models::minecraft::Authenticator;
        LaunchOptions {
            path: PathBuf::from("/mc"),
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
            instance,
            url: None,
            mcp: None,
            intel_enabled_mac: false,
            bypass_offline: false,
            skip_bundle_check: false,
            force_ipv4: false,
            dns: None,
        }
    }
}
