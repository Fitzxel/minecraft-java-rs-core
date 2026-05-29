use serde::de::DeserializeOwned;

/// GET `url`, check HTTP status, parse body as JSON.
///
/// Returns a descriptive `String` error that always includes the URL,
/// the HTTP status (when non-2xx), and a body preview (when JSON fails to parse).
pub async fn fetch_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("unknown");
        return Err(format!("GET {url}: HTTP {status} {reason}"));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("GET {url}: failed to read response body: {e}"))?;

    serde_json::from_str(&text).map_err(|e| {
        let preview: String = text.chars().take(300).collect();
        format!("GET {url}: failed to parse JSON: {e}\nBody: {preview}")
    })
}

/// GET `url`, check HTTP status, return body as text.
///
/// Returns a descriptive `String` error that always includes the URL and HTTP status.
pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("unknown");
        return Err(format!("GET {url}: HTTP {status} {reason}"));
    }

    resp.text()
        .await
        .map_err(|e| format!("GET {url}: failed to read response body: {e}"))
}
