use std::time::Duration;

use serde::de::DeserializeOwned;

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// Returns true for errors that are worth retrying (network issues, server errors).
/// 4xx client errors are not retried since they won't change.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// GET `url`, check HTTP status, parse body as JSON.
///
/// Retries up to `MAX_RETRIES` times on network errors or 5xx/429 responses,
/// with exponential backoff starting at `INITIAL_BACKOFF_MS`.
///
/// Returns a descriptive error that always includes the URL, HTTP status,
/// attempt count, and a body preview when JSON parsing fails.
pub async fn fetch_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let mut last_err = String::new();
    let mut backoff = INITIAL_BACKOFF_MS;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(16_000);
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("GET {url}: {e}");
                continue;
            }
        };

        let status = resp.status();
        if is_retryable_status(status) {
            let reason = status.canonical_reason().unwrap_or("unknown");
            last_err = format!("GET {url}: HTTP {status} {reason}");
            continue;
        }
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or("unknown");
            return Err(format!("GET {url}: HTTP {status} {reason}"));
        }

        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                last_err = format!("GET {url}: failed to read response body: {e}");
                continue;
            }
        };

        return serde_json::from_str(&text).map_err(|e| {
            let preview: String = text.chars().take(300).collect();
            format!("GET {url}: failed to parse JSON: {e}\nBody: {preview}")
        });
    }

    Err(format!("{last_err} (failed after {MAX_RETRIES} retries)"))
}

/// GET `url`, check HTTP status, return body as text.
///
/// Retries up to `MAX_RETRIES` times on network errors or 5xx/429 responses,
/// with exponential backoff starting at `INITIAL_BACKOFF_MS`.
pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    let mut last_err = String::new();
    let mut backoff = INITIAL_BACKOFF_MS;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(16_000);
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("GET {url}: {e}");
                continue;
            }
        };

        let status = resp.status();
        if is_retryable_status(status) {
            let reason = status.canonical_reason().unwrap_or("unknown");
            last_err = format!("GET {url}: HTTP {status} {reason}");
            continue;
        }
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or("unknown");
            return Err(format!("GET {url}: HTTP {status} {reason}"));
        }

        return resp
            .text()
            .await
            .map_err(|e| format!("GET {url}: failed to read response body: {e}"));
    }

    Err(format!("{last_err} (failed after {MAX_RETRIES} retries)"))
}
