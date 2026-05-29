use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::Sender;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::error::LaunchError;
use crate::launcher::events::LaunchEvent;
use crate::models::minecraft::AssetItem;
use crate::net::downloader::DownloadItem;
use crate::utils::hash::{get_file_hash, HashAlgorithm};

// ── Public API ────────────────────────────────────────────────────────────────

/// Sum of `size` fields for all `Asset` and `NativeAsset` items.
pub fn get_total_size(bundle: &[AssetItem]) -> u64 {
    bundle
        .iter()
        .map(|item| match item {
            AssetItem::Asset { size, .. } | AssetItem::NativeAsset { size, .. } => *size,
            AssetItem::CFile { .. } => 0,
        })
        .sum()
}

/// Write every `CFile` to disk and return a `Vec<DownloadItem>` for every
/// `Asset` / `NativeAsset` that is either missing from disk or whose SHA-1
/// does not match the expected value.
///
/// Emits `LaunchEvent::Check` progress events as each file is evaluated.
/// Up to `concurrency` files are checked in parallel.
pub async fn check_bundle(
    bundle: &[AssetItem],
    event_tx: &Sender<LaunchEvent>,
    concurrency: u32,
) -> Result<Vec<DownloadItem>, LaunchError> {
    let total = bundle.len();
    let semaphore = Arc::new(Semaphore::new(concurrency as usize));
    let counter = Arc::new(AtomicUsize::new(0));
    let mut tasks: JoinSet<Result<Option<DownloadItem>, LaunchError>> = JoinSet::new();

    for item in bundle.iter().cloned() {
        let sem = Arc::clone(&semaphore);
        let tx = event_tx.clone();
        let counter = Arc::clone(&counter);

        tasks.spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let current = counter.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = tx
                .send(LaunchEvent::Check {
                    current,
                    total,
                    kind: "bundle".into(),
                })
                .await;

            match item {
                AssetItem::CFile { path, content } => {
                    let dest = PathBuf::from(&path);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    if !dest.exists() {
                        tokio::fs::write(&dest, content).await?;
                    }
                    Ok(None)
                }

                AssetItem::Asset { ref path, ref sha1, size, ref url }
                | AssetItem::NativeAsset { ref path, ref sha1, size, ref url } => {
                    let dest = PathBuf::from(path);
                    let needs_download = if dest.exists() {
                        if sha1.is_empty() {
                            false
                        } else {
                            let dest_clone = dest.clone();
                            let expected = sha1.clone();
                            tokio::task::spawn_blocking(move || -> bool {
                                match get_file_hash(&dest_clone, HashAlgorithm::Sha1) {
                                    Ok(actual) => actual != expected,
                                    Err(_) => true,
                                }
                            })
                            .await
                            .unwrap_or(true)
                        }
                    } else {
                        true
                    };

                    if needs_download {
                        let folder = dest
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from("."));

                        let kind = match item {
                            AssetItem::NativeAsset { .. } => "natives",
                            _ => "assets",
                        };

                        Ok(Some(DownloadItem {
                            url: url.clone(),
                            path: dest.clone(),
                            folder,
                            name: dest
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            size,
                            r#type: Some(kind.into()),
                            sha1: Some(sha1.clone()),
                        }))
                    } else {
                        Ok(None)
                    }
                }
            }
        });
    }

    let mut pending: Vec<DownloadItem> = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(Some(item))) => pending.push(item),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(LaunchError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        }
    }

    Ok(pending)
}

/// Verify the SHA-1 of every `Asset` / `NativeAsset` that is present on disk.
///
/// Emits `LaunchEvent::Check` events and returns the paths of any files whose
/// digest does not match the expected value. Up to `concurrency` files are
/// hashed in parallel; use a lower value than `download_concurrency` to avoid
/// seek thrashing on HDDs.
pub async fn check_files(
    bundle: &[AssetItem],
    event_tx: &Sender<LaunchEvent>,
    concurrency: u32,
) -> Result<Vec<String>, LaunchError> {
    let items: Vec<(PathBuf, String)> = bundle
        .iter()
        .filter_map(|item| match item {
            AssetItem::Asset { path, sha1, .. } | AssetItem::NativeAsset { path, sha1, .. } => {
                let p = PathBuf::from(path);
                if p.exists() && !sha1.is_empty() {
                    Some((p, sha1.clone()))
                } else {
                    None
                }
            }
            AssetItem::CFile { .. } => None,
        })
        .collect();

    let total = items.len();
    let semaphore = Arc::new(Semaphore::new(concurrency as usize));
    let counter = Arc::new(AtomicUsize::new(0));
    let mut tasks: JoinSet<Result<Option<String>, LaunchError>> = JoinSet::new();

    for (path, expected_sha1) in items {
        let sem = Arc::clone(&semaphore);
        let tx = event_tx.clone();
        let counter = Arc::clone(&counter);

        tasks.spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let current = counter.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = tx
                .send(LaunchEvent::Check {
                    current,
                    total,
                    kind: "verify".into(),
                })
                .await;

            let path_clone = path.clone();
            let actual = tokio::task::spawn_blocking(move || {
                get_file_hash(&path_clone, HashAlgorithm::Sha1)
            })
            .await
            .map_err(|e| LaunchError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))??;

            if actual != expected_sha1 {
                Ok(Some(path.to_string_lossy().into_owned()))
            } else {
                Ok(None)
            }
        });
    }

    let mut bad: Vec<String> = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(Some(path))) => bad.push(path),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(LaunchError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                )))
            }
        }
    }

    Ok(bad)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn asset(path: &str, sha1: &str, size: u64, url: &str) -> AssetItem {
        AssetItem::Asset {
            path: path.into(),
            sha1: sha1.into(),
            size,
            url: url.into(),
        }
    }

    fn cfile(path: &str, content: &str) -> AssetItem {
        AssetItem::CFile {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn get_total_size_sums_assets() {
        let bundle = vec![
            asset("/a", "aa", 100, "http://x"),
            asset("/b", "bb", 200, "http://x"),
            cfile("/c", "data"),
        ];
        assert_eq!(get_total_size(&bundle), 300);
    }

    #[test]
    fn get_total_size_empty() {
        assert_eq!(get_total_size(&[]), 0);
    }

    #[tokio::test]
    async fn check_bundle_writes_cfile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("indexes").join("test.json");
        let bundle = vec![cfile(
            &path.to_string_lossy(),
            r#"{"objects":{}}"#,
        )];
        let (tx, _rx) = mpsc::channel(16);
        let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
        assert!(pending.is_empty());
        assert!(path.exists());
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, r#"{"objects":{}}"#);
    }

    #[tokio::test]
    async fn check_bundle_skips_existing_cfile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.json");
        tokio::fs::write(&path, b"original").await.unwrap();

        let bundle = vec![cfile(&path.to_string_lossy(), "new content")];
        let (tx, _rx) = mpsc::channel(16);
        check_bundle(&bundle, &tx, 4).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "original");
    }

    #[tokio::test]
    async fn check_bundle_missing_asset_added_to_pending() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.jar");
        let bundle = vec![asset(&path.to_string_lossy(), "deadbeef", 42, "http://example.com/a.jar")];

        let (tx, _rx) = mpsc::channel(16);
        let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].url, "http://example.com/a.jar");
    }

    #[tokio::test]
    async fn check_bundle_correct_hash_skips_download() {
        use sha1::{Digest, Sha1};

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("asset.dat");
        let content = b"hello world";
        tokio::fs::write(&path, content).await.unwrap();

        let mut hasher = Sha1::new();
        hasher.update(content);
        let sha1 = format!("{:x}", hasher.finalize());

        let bundle = vec![asset(&path.to_string_lossy(), &sha1, 11, "http://example.com/x")];
        let (tx, _rx) = mpsc::channel(16);
        let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn check_bundle_wrong_hash_queues_download() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("asset.dat");
        tokio::fs::write(&path, b"stale content").await.unwrap();

        let bundle = vec![asset(&path.to_string_lossy(), "0000000000000000000000000000000000000000", 13, "http://example.com/x")];
        let (tx, _rx) = mpsc::channel(16);
        let pending = check_bundle(&bundle, &tx, 4).await.unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn check_files_returns_empty_for_correct_files() {
        use sha1::{Digest, Sha1};

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.jar");
        let content = b"jar content";
        tokio::fs::write(&path, content).await.unwrap();

        let mut hasher = Sha1::new();
        hasher.update(content);
        let sha1 = format!("{:x}", hasher.finalize());

        let bundle = vec![asset(&path.to_string_lossy(), &sha1, 11, "http://x")];
        let (tx, _rx) = mpsc::channel(16);
        let bad = check_files(&bundle, &tx, 4).await.unwrap();
        assert!(bad.is_empty());
    }

    #[tokio::test]
    async fn check_files_reports_corrupted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.jar");
        tokio::fs::write(&path, b"corrupted").await.unwrap();

        let bundle = vec![asset(&path.to_string_lossy(), "0000000000000000000000000000000000000000", 9, "http://x")];
        let (tx, _rx) = mpsc::channel(16);
        let bad = check_files(&bundle, &tx, 4).await.unwrap();
        assert_eq!(bad.len(), 1);
    }

    #[tokio::test]
    async fn check_files_skips_missing_files() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.jar");
        let bundle = vec![asset(&path.to_string_lossy(), "abc", 0, "http://x")];

        let (tx, _rx) = mpsc::channel(16);
        let bad = check_files(&bundle, &tx, 4).await.unwrap();
        assert!(bad.is_empty());
    }
}
