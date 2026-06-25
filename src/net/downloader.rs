use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use sha1::Digest;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::error::{DownloadError, LaunchError};
use crate::launcher::events::LaunchEvent;

const DOWNLOAD_MAX_RETRIES: u32 = 3;
const DOWNLOAD_INITIAL_BACKOFF_MS: u64 = 500;

// ── DownloadItem ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DownloadItem {
    /// Full URL to fetch.
    pub url: String,
    /// Absolute path to write the file to.
    pub path: PathBuf,
    /// Parent directory; created with `create_dir_all` before writing.
    /// When empty the parent of `path` is used instead.
    pub folder: PathBuf,
    /// Human-readable name used in error messages and progress events.
    pub name: String,
    /// Expected file size in bytes (used for progress totals; 0 = unknown).
    pub size: u64,
    /// Category label emitted with `LaunchEvent::Progress` (e.g. "assets").
    #[allow(clippy::pub_with_shorthand)]
    pub r#type: Option<String>,
    /// Expected SHA-1 hex digest.  When `Some`, the file is verified after
    /// download; `DownloadError::ChecksumMismatch` is returned on mismatch.
    pub sha1: Option<String>,
}

// ── Downloader ────────────────────────────────────────────────────────────────

pub struct Downloader {
    client: reqwest::Client,
    /// Effective concurrency after applying the adaptive cap.
    concurrency: usize,
}

impl Downloader {
    pub fn new(
        timeout_secs: u64,
        concurrency: u32,
        force_ipv4: bool,
        dns: Option<std::net::IpAddr>,
    ) -> Self {
        let client = crate::net::client::build_client(timeout_secs, force_ipv4, dns)
            .expect("failed to build reqwest client");
        Self {
            client,
            concurrency: adaptive_concurrency(concurrency),
        }
    }

    /// Download a single file.  No progress events are emitted.
    pub async fn download_file(&self, item: &DownloadItem) -> Result<(), LaunchError> {
        let counter = Arc::new(AtomicU64::new(0));
        fetch_one(self.client.clone(), item, &counter)
            .await
            .map_err(LaunchError::Download)
    }

    /// Download many files concurrently, emitting `LaunchEvent` progress
    /// notifications on `event_tx`.
    ///
    /// Events emitted:
    /// - `Progress { downloaded, total, kind }` — file-count progress after
    ///   each file completes, where `downloaded` = files done, `total` = total
    ///   files.
    /// - `Speed(bytes_per_sec)` — rolling 5-second average.
    /// - `Estimated(secs)` — ETA in seconds at the current speed.
    pub async fn download_multiple(
        &self,
        items: Vec<DownloadItem>,
        event_tx: tokio::sync::mpsc::Sender<LaunchEvent>,
    ) -> Result<(), LaunchError> {
        if items.is_empty() {
            return Ok(());
        }

        let total_bytes: u64 = items.iter().map(|i| i.size).sum();
        let total_count = items.len() as u64;
        let downloaded = Arc::new(AtomicU64::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut join_set: JoinSet<Result<(), LaunchError>> = JoinSet::new();

        for item in items {
            let sem = Arc::clone(&semaphore);
            let dl = Arc::clone(&downloaded);
            let comp = Arc::clone(&completed);
            let client = self.client.clone();
            let tx = event_tx.clone();

            join_set.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| LaunchError::Archive(e.to_string()))?;

                fetch_one(client, &item, &dl)
                    .await
                    .map_err(LaunchError::Download)?;

                let done = comp.fetch_add(1, Ordering::Relaxed) as u64 + 1;
                tx.send(LaunchEvent::Progress {
                    downloaded: done,
                    total: total_count,
                    kind: item.r#type.clone().unwrap_or_default(),
                })
                .await
                .ok();

                Ok(())
            });
        }

        // Sliding-window speed tracker (pure coordinator state — no sharing needed).
        let mut speed_window: VecDeque<(Instant, u64)> = VecDeque::new();

        while let Some(result) = join_set.join_next().await {
            result.map_err(|e| LaunchError::Archive(e.to_string()))??;

            let now = Instant::now();
            let dl = downloaded.load(Ordering::Relaxed);
            speed_window.push_back((now, dl));

            // Evict samples older than 5 seconds.
            while speed_window
                .front()
                .map_or(false, |(t, _)| now.duration_since(*t).as_secs_f64() > 5.0)
            {
                speed_window.pop_front();
            }

            if let Some((t0, b0)) = speed_window.front() {
                let dt = now.duration_since(*t0).as_secs_f64();
                if dt > 0.1 {
                    let speed = dl.saturating_sub(*b0) as f64 / dt;
                    event_tx.send(LaunchEvent::Speed(speed)).await.ok();
                    if speed > 0.0 && total_bytes > 0 {
                        let remaining = total_bytes.saturating_sub(dl) as f64 / speed;
                        event_tx.send(LaunchEvent::Estimated(remaining)).await.ok();
                    }
                }
            }
        }

        Ok(())
    }

    /// Returns `true` if a HEAD request to `url` succeeds with a 2xx status.
    pub async fn check_url(&self, url: &str) -> bool {
        self.client
            .head(url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Iterate `mirrors` in order, appending `path`, and return the first URL
    /// that responds successfully to a HEAD request.  Returns `None` if all
    /// mirrors are unreachable.
    pub async fn check_mirror(&self, mirrors: &[&str], path: &str) -> Option<String> {
        let path = path.trim_start_matches('/');
        for mirror in mirrors {
            let url = format!("{}/{}", mirror.trim_end_matches('/'), path);
            if self.check_url(&url).await {
                return Some(url);
            }
        }
        None
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Clamp user-requested concurrency to a system-aware upper bound.
///
/// Each active download holds roughly one TCP connection, a ~64 KB read buffer,
/// and a Tokio task. High values (e.g. 400) exhaust file descriptors and network
/// stack memory without any throughput gain. The cap is:
///
///   min(requested, cpu_cores × 8, 64).max(1)
///
/// This allows a 4-core machine to run up to 32 simultaneous downloads and an
/// 8-core machine up to 64, while still honouring smaller values the caller sets.
fn adaptive_concurrency(requested: u32) -> usize {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let cap = (cpu_count * 8).min(64).max(4);
    (requested as usize).clamp(1, cap)
}

/// Returns true for HTTP status codes worth retrying.
/// 4xx client errors are not retried since they won't change.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Map a `reqwest::Error` to a `DownloadError`, distinguishing transport-level
/// failures (no HTTP status — DNS, unreachable, reset, timeout) from
/// server-side ones. Transport failures get a human-readable cause via
/// [`describe_reqwest_error`](crate::net::client::describe_reqwest_error) so the
/// user sees the real reason instead of "error sending request for url".
fn classify_error(url: &str, e: reqwest::Error) -> DownloadError {
    if e.status().is_some() {
        DownloadError::Http(e)
    } else {
        DownloadError::Connection {
            url: url.to_owned(),
            detail: crate::net::client::describe_reqwest_error(&e),
        }
    }
}

/// Download `item` to disk, updating `dl_counter` with each received chunk.
///
/// Uses a temporary file (`<path>.tmp`) and an atomic rename so a failed or
/// interrupted download never leaves a corrupt file at the final path.
///
/// Retries up to `DOWNLOAD_MAX_RETRIES` times on network errors, 5xx, and 429,
/// with exponential backoff starting at `DOWNLOAD_INITIAL_BACKOFF_MS`.
/// Checksum mismatches and I/O errors are not retried.
async fn fetch_one(
    client: reqwest::Client,
    item: &DownloadItem,
    dl_counter: &Arc<AtomicU64>,
) -> Result<(), DownloadError> {
    let dir = if item.folder.as_os_str().is_empty() {
        item.path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        item.folder.clone()
    };
    tokio::fs::create_dir_all(&dir).await?;

    // Temporary path: `foo.jar` → `foo.jar.tmp`
    let tmp_path = {
        let mut s = item.path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };

    let mut last_err: Option<DownloadError> = None;
    let mut backoff = DOWNLOAD_INITIAL_BACKOFF_MS;

    for attempt in 0..=DOWNLOAD_MAX_RETRIES {
        if attempt > 0 {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(8_000);
        }

        // ── Send request ──────────────────────────────────────────────────────
        let response = match client.get(&item.url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(classify_error(&item.url, e));
                continue;
            }
        };

        let status = response.status();
        if is_retryable_status(status) {
            last_err = Some(DownloadError::Http(
                response.error_for_status().unwrap_err(),
            ));
            continue;
        }
        if !status.is_success() {
            // 4xx — don't retry
            return Err(DownloadError::Http(
                response.error_for_status().unwrap_err(),
            ));
        }

        // ── Stream body to temp file ──────────────────────────────────────────
        let mut file = match tokio::fs::File::create(&tmp_path).await {
            Ok(f) => f,
            Err(e) => return Err(DownloadError::Io(e)),
        };

        let mut stream = response.bytes_stream();
        let mut hasher = sha1::Sha1::new();
        let verify = item.sha1.is_some();
        let mut stream_err: Option<DownloadError> = None;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if let Err(e) = file.write_all(&chunk).await {
                        return Err(DownloadError::Io(e));
                    }
                    if verify {
                        hasher.update(&chunk);
                    }
                    dl_counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    stream_err = Some(classify_error(&item.url, e));
                    break;
                }
            }
        }

        if let Some(e) = stream_err {
            last_err = Some(e);
            continue;
        }

        if let Err(e) = file.flush().await {
            return Err(DownloadError::Io(e));
        }

        // ── Checksum ──────────────────────────────────────────────────────────
        if let Some(expected) = &item.sha1 {
            let actual: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if actual != *expected {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(DownloadError::ChecksumMismatch {
                    file: item.name.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }

        // ── Atomic rename ─────────────────────────────────────────────────────
        if let Err(e) = tokio::fs::rename(&tmp_path, &item.path).await {
            return Err(DownloadError::Io(e));
        }

        return Ok(());
    }

    let _ = tokio::fs::remove_file(&tmp_path).await;
    Err(last_err.unwrap_or(DownloadError::Timeout))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn make_downloader() -> Downloader {
        Downloader::new(5, 4, false, None)
    }

    #[test]
    fn adaptive_concurrency_clamps_high_value() {
        // Whatever the CPU count is, 400 must be reduced.
        assert!(adaptive_concurrency(400) <= 64);
    }

    #[test]
    fn adaptive_concurrency_preserves_low_value() {
        assert_eq!(adaptive_concurrency(2), 2);
        assert_eq!(adaptive_concurrency(1), 1);
    }

    #[test]
    fn adaptive_concurrency_floors_at_one() {
        assert_eq!(adaptive_concurrency(0), 1);
    }

    #[tokio::test]
    async fn download_multiple_empty_list() {
        let d = make_downloader();
        let (tx, _rx) = mpsc::channel(16);
        d.download_multiple(vec![], tx).await.unwrap();
    }

    #[tokio::test]
    async fn download_file_bad_url_returns_error() {
        let dir = TempDir::new().unwrap();
        let item = DownloadItem {
            url: "http://127.0.0.1:1/nonexistent".into(),
            path: dir.path().join("out.bin"),
            folder: dir.path().to_path_buf(),
            name: "out.bin".into(),
            size: 0,
            r#type: None,
            sha1: None,
        };
        let d = Downloader::new(1, 1, false, None); // 1-second timeout
        let result = d.download_file(&item).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn check_url_unreachable_returns_false() {
        let d = Downloader::new(1, 1, false, None);
        assert!(!d.check_url("http://127.0.0.1:1/test").await);
    }

    #[tokio::test]
    async fn check_mirror_all_bad_returns_none() {
        let d = Downloader::new(1, 1, false, None);
        let result = d
            .check_mirror(&["http://127.0.0.1:1"], "/some/path.jar")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn download_multiple_bad_url_propagates_error() {
        let dir = TempDir::new().unwrap();
        let item = DownloadItem {
            url: "http://127.0.0.1:1/nonexistent".into(),
            path: dir.path().join("out.bin"),
            folder: dir.path().to_path_buf(),
            name: "out.bin".into(),
            size: 0,
            r#type: Some("test".into()),
            sha1: None,
        };
        let d = Downloader::new(1, 1, false, None);
        let (tx, _rx) = mpsc::channel(16);
        let result = d.download_multiple(vec![item], tx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_tmp_file_left_after_failed_download() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.bin");
        let item = DownloadItem {
            url: "http://127.0.0.1:1/nonexistent".into(),
            path: path.clone(),
            folder: dir.path().to_path_buf(),
            name: "out.bin".into(),
            size: 0,
            r#type: None,
            sha1: None,
        };
        let d = Downloader::new(1, 1, false, None);
        let _ = d.download_file(&item).await;

        let tmp = {
            let mut s = path.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };
        assert!(
            !tmp.exists(),
            ".tmp file should be cleaned up after failure"
        );
    }
}
