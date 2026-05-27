use std::time::Duration;

/// Returns `true` if an outgoing HTTP connection can be established.
///
/// Makes a GET request to Google's generate_204 endpoint (returns 204 No
/// Content instantly, so there's no body to parse and the round-trip is fast).
/// Falls back to `false` on any error including timeout.
pub async fn check_internet() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    client
        .get("https://connectivitycheck.gstatic.com/generate_204")
        .send()
        .await
        .is_ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_host_returns_false() {
        // A syntactically valid but unreachable URL: should return false quickly.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let result = client
            .get("http://192.0.2.1/generate_204") // TEST-NET-1, RFC 5737
            .send()
            .await;
        assert!(result.is_err());
    }
}
