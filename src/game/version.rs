use crate::error::LaunchError;
use crate::launcher::options::LaunchOptions;
use crate::models::minecraft::{MinecraftVersionJson, MojangVersionManifest};
use crate::net::http::fetch_json;

const MANIFEST_URL: &str =
    "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json";

/// Fetch and return the `MinecraftVersionJson` for the version requested in
/// `options`.
///
/// Steps:
/// 1. Download the Mojang version manifest.
/// 2. Resolve version aliases (`latest_release` / `r` / `lr`, etc.).
/// 3. Locate the version entry and download its per-version JSON.
/// 4. On Linux ARM, patch LWJGL/JInput libraries via `lwjgl_native::process_json`.
pub async fn get_version_json(
    options: &LaunchOptions,
    client: &reqwest::Client,
) -> Result<MinecraftVersionJson, LaunchError> {
    fetch_version_json(MANIFEST_URL, options, client).await
}

/// Inner implementation — accepts an explicit manifest base URL so that tests
/// can point at a local mock server instead of the real Mojang CDN.
pub(crate) async fn fetch_version_json(
    manifest_base: &str,
    options: &LaunchOptions,
    client: &reqwest::Client,
) -> Result<MinecraftVersionJson, LaunchError> {
    // Cache-buster keeps CDN proxies from returning stale manifests.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let manifest_url = format!("{manifest_base}?_t={ts}");

    let manifest: MojangVersionManifest = fetch_json(client, &manifest_url)
        .await
        .map_err(LaunchError::InvalidData)?;

    let version = resolve_alias(&options.version, &manifest);

    let entry = manifest
        .versions
        .iter()
        .find(|v| v.id == version)
        .ok_or_else(|| LaunchError::VersionNotFound(version.clone()))?;

    let mut version_json: MinecraftVersionJson = fetch_json(client, &entry.url)
        .await
        .map_err(LaunchError::InvalidData)?;

    if is_linux_arm() {
        crate::game::lwjgl_native::process_json(&mut version_json)?;
    }

    Ok(version_json)
}

/// Map symbolic aliases to the concrete version ID from the manifest.
fn resolve_alias(version: &str, manifest: &MojangVersionManifest) -> String {
    match version {
        "latest_release" | "r" | "lr" => manifest.latest.release.clone(),
        "latest_snapshot" | "s" | "ls" => manifest.latest.snapshot.clone(),
        other => other.to_string(),
    }
}

/// Runtime check: are we on Linux ARM?
///
/// Uses `std::env::consts` so that a cross-compiled binary running on ARM
/// still detects its actual execution environment.
fn is_linux_arm() -> bool {
    std::env::consts::OS == "linux"
        && matches!(std::env::consts::ARCH, "aarch64" | "arm")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::minecraft::{LatestVersions, MojangVersionManifest, VersionEntry};

    fn manifest(release: &str, snapshot: &str) -> MojangVersionManifest {
        MojangVersionManifest {
            latest: LatestVersions {
                release: release.to_string(),
                snapshot: snapshot.to_string(),
            },
            versions: vec![
                make_entry("1.20.4"),
                make_entry("24w14a"),
            ],
        }
    }

    fn make_entry(id: &str) -> VersionEntry {
        VersionEntry {
            id: id.to_string(),
            version_type: "release".to_string(),
            url: format!("https://example.com/{id}.json"),
            time: String::new(),
            release_time: String::new(),
        }
    }

    #[test]
    fn alias_latest_release() {
        let m = manifest("1.20.4", "24w14a");
        assert_eq!(resolve_alias("latest_release", &m), "1.20.4");
        assert_eq!(resolve_alias("r", &m), "1.20.4");
        assert_eq!(resolve_alias("lr", &m), "1.20.4");
    }

    #[test]
    fn alias_latest_snapshot() {
        let m = manifest("1.20.4", "24w14a");
        assert_eq!(resolve_alias("latest_snapshot", &m), "24w14a");
        assert_eq!(resolve_alias("s", &m), "24w14a");
        assert_eq!(resolve_alias("ls", &m), "24w14a");
    }

    #[test]
    fn concrete_version_passes_through() {
        let m = manifest("1.20.4", "24w14a");
        assert_eq!(resolve_alias("1.19.4", &m), "1.19.4");
    }

    // ── Mock HTTP tests (wiremock) ────────────────────────────────────────────

    /// Minimal manifest JSON template; `VERSION_URL` is replaced at test time
    /// with the actual mock server URL.
    const MOCK_MANIFEST_TEMPLATE: &str = r#"{
        "latest": { "release": "1.20.4", "snapshot": "1.20.4" },
        "versions": [
            {
                "id": "1.20.4",
                "type": "release",
                "url": "VERSION_URL",
                "time": "2024-01-22T10:00:00+00:00",
                "releaseTime": "2024-01-22T10:00:00+00:00"
            }
        ]
    }"#;

    const MOCK_VERSION_JSON: &str = r#"{
        "id": "1.20.4",
        "type": "release",
        "assets": "16",
        "libraries": [],
        "mainClass": "net.minecraft.client.main.Main",
        "assetIndex": {
            "id": "16",
            "sha1": "abc123",
            "size": 100,
            "url": "https://resources.example.com/16.json"
        },
        "downloads": {
            "client": {
                "sha1": "def456",
                "size": 1000,
                "url": "https://resources.example.com/client.jar"
            }
        }
    }"#;

    fn mock_options(version: &str) -> LaunchOptions {
        use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
        use crate::models::minecraft::Authenticator;
        LaunchOptions {
            path: std::path::PathBuf::from("/tmp/mc-test"),
            version: version.to_string(),
            authenticator: Authenticator {
                access_token: String::new(),
                name: "TestUser".into(),
                uuid: "test-uuid".into(),
                xbox_account: None,
                user_properties: None,
                client_id: None,
                client_token: None,
            },
            timeout_secs: 5,
            download_concurrency: 1,
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
        }
    }

    #[tokio::test]
    async fn fetch_version_json_from_mock_server_concrete_version() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Register manifest mock.
        let version_url = format!("{}/versions/1.20.4.json", server.uri());
        let manifest_body = MOCK_MANIFEST_TEMPLATE.replace("VERSION_URL", &version_url);
        Mock::given(method("GET"))
            .and(path("/manifest.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(manifest_body),
            )
            .mount(&server)
            .await;

        // Register version JSON mock.
        Mock::given(method("GET"))
            .and(path("/versions/1.20.4.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(MOCK_VERSION_JSON),
            )
            .mount(&server)
            .await;

        let manifest_url = format!("{}/manifest.json", server.uri());
        let client = reqwest::Client::new();
        let options = mock_options("1.20.4");

        let result = fetch_version_json(&manifest_url, &options, &client)
            .await
            .unwrap();

        assert_eq!(result.id, "1.20.4");
        assert_eq!(result.version_type, "release");
        assert_eq!(result.main_class.as_deref(), Some("net.minecraft.client.main.Main"));
        assert!(result.libraries.is_empty());
    }

    #[tokio::test]
    async fn fetch_version_json_resolves_latest_release_alias() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let version_url = format!("{}/versions/1.20.4.json", server.uri());
        let manifest_body = MOCK_MANIFEST_TEMPLATE.replace("VERSION_URL", &version_url);
        Mock::given(method("GET"))
            .and(path("/manifest.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(manifest_body),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/versions/1.20.4.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(MOCK_VERSION_JSON),
            )
            .mount(&server)
            .await;

        let manifest_url = format!("{}/manifest.json", server.uri());
        let client = reqwest::Client::new();
        // "latest_release" should resolve to "1.20.4" via the mock manifest.
        let options = mock_options("latest_release");

        let result = fetch_version_json(&manifest_url, &options, &client)
            .await
            .unwrap();

        assert_eq!(result.id, "1.20.4");
    }

    #[tokio::test]
    async fn fetch_version_json_returns_error_for_missing_version() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let version_url = format!("{}/versions/1.20.4.json", server.uri());
        let manifest_body = MOCK_MANIFEST_TEMPLATE.replace("VERSION_URL", &version_url);
        Mock::given(method("GET"))
            .and(path("/manifest.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(manifest_body),
            )
            .mount(&server)
            .await;

        let manifest_url = format!("{}/manifest.json", server.uri());
        let client = reqwest::Client::new();
        let options = mock_options("99.99.99");

        let result = fetch_version_json(&manifest_url, &options, &client).await;

        assert!(matches!(result, Err(LaunchError::VersionNotFound(_))));
    }

    #[tokio::test]
    async fn fetch_version_json_returns_http_error_on_500() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manifest.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let manifest_url = format!("{}/manifest.json", server.uri());
        let client = reqwest::Client::new();
        let options = mock_options("1.20.4");

        let result = fetch_version_json(&manifest_url, &options, &client).await;
        assert!(result.is_err());
    }
}
