use crate::error::LaunchError;
use crate::launcher::options::LaunchOptions;
use crate::models::minecraft::{AssetIndexData, AssetItem, MinecraftVersionJson};
use crate::net::http::fetch_text;

const RESOURCES_BASE: &str = "https://resources.download.minecraft.net";

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the full list of asset items that the launcher must have on disk.
///
/// Returns two kinds of [`AssetItem`]:
/// - `CFile` — the asset-index JSON itself (written verbatim to
///   `<path>/assets/indexes/<id>.json`).
/// - `Asset` — each hashed object in the index (downloaded from Mojang CDN).
///
/// All paths are **absolute** (prefixed with `options.path`).
/// If `version_json` has no `asset_index`, an empty `Vec` is returned.
pub async fn get_assets(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
    client: &reqwest::Client,
) -> Result<Vec<AssetItem>, LaunchError> {
    let ai = match &version_json.asset_index {
        Some(ai) => ai,
        None => return Ok(vec![]),
    };

    let raw = fetch_text(client, &ai.url)
        .await
        .map_err(LaunchError::InvalidData)?;
    let data: AssetIndexData = serde_json::from_str(&raw)
        .map_err(|e| LaunchError::InvalidData(format!("GET {}: failed to parse asset index: {e}", &ai.url)))?;

    let base = &options.path;
    let mut items: Vec<AssetItem> = Vec::with_capacity(data.objects.len() + 1);

    // The index JSON is stored as a CFile so the downloader writes it verbatim.
    items.push(AssetItem::CFile {
        path: base
            .join("assets")
            .join("indexes")
            .join(format!("{}.json", ai.id))
            .to_string_lossy()
            .into_owned(),
        content: raw,
    });

    for obj in data.objects.values() {
        let sub = &obj.hash[..2];
        items.push(AssetItem::Asset {
            path: base
                .join("assets")
                .join("objects")
                .join(sub)
                .join(&obj.hash)
                .to_string_lossy()
                .into_owned(),
            sha1: obj.hash.clone(),
            size: obj.size,
            url: format!("{RESOURCES_BASE}/{sub}/{}", obj.hash),
        });
    }

    Ok(items)
}

/// Copy legacy assets from the object store into a flat `resources/` tree.
///
/// Only meaningful for old Minecraft versions (assets `"legacy"` /
/// `"pre-1.6"`).  The caller is responsible for deciding when to invoke this
/// based on [`crate::utils::version_check::is_old`].
///
/// If the local index file does not yet exist (assets not downloaded),
/// this is a no-op.
pub async fn copy_assets(
    options: &LaunchOptions,
    version_json: &MinecraftVersionJson,
) -> Result<(), LaunchError> {
    let assets_id = match &version_json.assets {
        Some(a) => a.clone(),
        None => return Ok(()),
    };

    let index_path = options
        .path
        .join("assets")
        .join("indexes")
        .join(format!("{assets_id}.json"));

    if !index_path.exists() {
        return Ok(());
    }

    let raw = tokio::fs::read_to_string(&index_path).await?;
    let data: AssetIndexData = serde_json::from_str(&raw)?;

    let legacy_dir = match &options.instance {
        Some(inst) => options
            .path
            .join("instances")
            .join(inst)
            .join("resources"),
        None => options.path.join("resources"),
    };

    for (file_path, obj) in &data.objects {
        let sub = &obj.hash[..2];
        let source = options
            .path
            .join("assets")
            .join("objects")
            .join(sub)
            .join(&obj.hash);

        if !source.exists() {
            continue;
        }

        let target = legacy_dir.join(file_path);

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if !target.exists() {
            tokio::fs::copy(&source, &target).await?;
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn opts(path: PathBuf) -> LaunchOptions {
        use crate::models::minecraft::Authenticator;
        use crate::launcher::options::{JavaOptions, LoaderConfig, MemoryConfig, ScreenConfig};
        LaunchOptions {
            path,
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
            instance: None,
            url: None,
            mcp: None,
            intel_enabled_mac: false,
            bypass_offline: false,
            skip_bundle_check: false,
        }
    }

    fn version_json_no_assets() -> MinecraftVersionJson {
        MinecraftVersionJson {
            id: "1.20.4".into(),
            version_type: "release".into(),
            assets: None,
            asset_index: None,
            downloads: None,
            libraries: vec![],
            arguments: None,
            minecraft_arguments: None,
            java_version: None,
            main_class: None,
            has_natives: false,
        }
    }

    #[tokio::test]
    async fn get_assets_returns_empty_without_asset_index() {
        let dir = TempDir::new().unwrap();
        let client = reqwest::Client::new();
        let result = get_assets(&opts(dir.path().to_path_buf()), &version_json_no_assets(), &client)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn copy_assets_noop_when_no_assets_field() {
        let dir = TempDir::new().unwrap();
        let vj = version_json_no_assets();
        copy_assets(&opts(dir.path().to_path_buf()), &vj).await.unwrap();
    }

    #[tokio::test]
    async fn copy_assets_noop_when_index_missing() {
        let dir = TempDir::new().unwrap();
        let mut vj = version_json_no_assets();
        vj.assets = Some("legacy".into());
        // index file doesn't exist → should return Ok(()) without creating anything
        copy_assets(&opts(dir.path().to_path_buf()), &vj).await.unwrap();
        assert!(!dir.path().join("resources").exists());
    }

    #[tokio::test]
    async fn copy_assets_copies_objects_to_resources() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        // Write a fake asset object
        let hash = "aabbccddee112233445566778899001122334455";
        let sub = &hash[..2];
        let obj_dir = base.join("assets").join("objects").join(sub);
        tokio::fs::create_dir_all(&obj_dir).await.unwrap();
        tokio::fs::write(obj_dir.join(hash), b"fake asset content").await.unwrap();

        // Write the asset index pointing to it
        let index_json = format!(
            r#"{{"objects": {{"sounds/ambient/cave.ogg": {{"hash": "{hash}", "size": 18}}}}}}"#
        );
        let idx_dir = base.join("assets").join("indexes");
        tokio::fs::create_dir_all(&idx_dir).await.unwrap();
        tokio::fs::write(idx_dir.join("legacy.json"), &index_json).await.unwrap();

        let mut vj = version_json_no_assets();
        vj.assets = Some("legacy".into());

        copy_assets(&opts(base.to_path_buf()), &vj).await.unwrap();

        let copied = base.join("resources").join("sounds").join("ambient").join("cave.ogg");
        assert!(copied.exists(), "asset should have been copied to resources/");
        assert_eq!(std::fs::read(&copied).unwrap(), b"fake asset content");
    }

    #[tokio::test]
    async fn copy_assets_skips_existing_target() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        let hash = "aabbccddee112233445566778899001122334455";
        let sub = &hash[..2];
        let obj_dir = base.join("assets").join("objects").join(sub);
        tokio::fs::create_dir_all(&obj_dir).await.unwrap();
        tokio::fs::write(obj_dir.join(hash), b"new content").await.unwrap();

        let index_json = format!(
            r#"{{"objects": {{"file.txt": {{"hash": "{hash}", "size": 11}}}}}}"#
        );
        let idx_dir = base.join("assets").join("indexes");
        tokio::fs::create_dir_all(&idx_dir).await.unwrap();
        tokio::fs::write(idx_dir.join("legacy.json"), &index_json).await.unwrap();

        // Pre-create target with different content
        let resources_dir = base.join("resources");
        tokio::fs::create_dir_all(&resources_dir).await.unwrap();
        tokio::fs::write(resources_dir.join("file.txt"), b"original").await.unwrap();

        let mut vj = version_json_no_assets();
        vj.assets = Some("legacy".into());

        copy_assets(&opts(base.to_path_buf()), &vj).await.unwrap();

        // Existing file must NOT be overwritten
        let content = std::fs::read(resources_dir.join("file.txt")).unwrap();
        assert_eq!(content, b"original");
    }

    #[tokio::test]
    async fn copy_assets_uses_instance_resources_dir() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        let hash = "aabbccddee112233445566778899001122334455";
        let sub = &hash[..2];
        let obj_dir = base.join("assets").join("objects").join(sub);
        tokio::fs::create_dir_all(&obj_dir).await.unwrap();
        tokio::fs::write(obj_dir.join(hash), b"sound").await.unwrap();

        let index_json = format!(
            r#"{{"objects": {{"a.ogg": {{"hash": "{hash}", "size": 5}}}}}}"#
        );
        let idx_dir = base.join("assets").join("indexes");
        tokio::fs::create_dir_all(&idx_dir).await.unwrap();
        tokio::fs::write(idx_dir.join("legacy.json"), &index_json).await.unwrap();

        let mut options = opts(base.to_path_buf());
        options.instance = Some("myworld".into());

        let mut vj = version_json_no_assets();
        vj.assets = Some("legacy".into());

        copy_assets(&options, &vj).await.unwrap();

        let target = base.join("instances").join("myworld").join("resources").join("a.ogg");
        assert!(target.exists());
    }
}
