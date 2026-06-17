//! Centralised reqwest client construction and transport-error description.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// A DNS resolver that returns only IPv4 addresses.
///
/// On networks that advertise IPv6 (AAAA records) but whose IPv6 route is
/// broken, reqwest/hyper does not fall back to IPv4 the way browsers do (it has
/// no Happy Eyeballs), so connections hang or fail with the opaque "error
/// sending request for url". This is a classic "works in the browser and over a
/// VPN, fails in the app" symptom. Filtering DNS results to IPv4 before reqwest
/// ever attempts a connection sidesteps the broken IPv6 path entirely.
#[derive(Debug, Default)]
struct Ipv4OnlyResolver;

impl Resolve for Ipv4OnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Port is irrelevant here — reqwest overwrites it with the real one.
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let v4: Vec<SocketAddr> = addrs.filter(SocketAddr::is_ipv4).collect();
            Ok(Box::new(v4.into_iter()) as Addrs)
        })
    }
}

/// Build a reqwest client with the shared launcher configuration.
///
/// When `force_ipv4` is `true`, DNS resolution is restricted to IPv4 addresses
/// (see [`Ipv4OnlyResolver`]).
pub fn build_client(timeout_secs: u64, force_ipv4: bool) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs));
    if force_ipv4 {
        builder = builder.dns_resolver(Arc::new(Ipv4OnlyResolver));
    }
    builder.build()
}

/// Describe a `reqwest::Error`, surfacing the underlying transport cause when
/// the request never reached the server (no HTTP status).
///
/// reqwest's own `Display` for these cases is the unhelpful "error sending
/// request for url (…)". This walks the error source chain down to the root
/// cause (e.g. "Network is unreachable", "Temporary failure in name
/// resolution", "Connection reset by peer") — the information actually needed
/// to tell a DNS problem from a broken IPv6 route from an ISP reset.
pub fn describe_reqwest_error(err: &reqwest::Error) -> String {
    if let Some(status) = err.status() {
        let reason = status.canonical_reason().unwrap_or("unknown");
        return format!("HTTP {status} {reason}");
    }

    let kind = if err.is_timeout() {
        "connection timed out"
    } else if err.is_connect() {
        "could not establish connection"
    } else if err.is_request() {
        "request could not be sent"
    } else {
        "network error"
    };

    // Walk to the deepest cause in the source chain — that is where the real
    // OS-level reason (DNS / unreachable / reset) lives.
    let mut root: Option<String> = None;
    let mut src = std::error::Error::source(err);
    while let Some(e) = src {
        root = Some(e.to_string());
        src = e.source();
    }

    match root {
        Some(cause) => format!("{kind} ({cause})"),
        None => kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_succeeds_in_both_modes() {
        assert!(build_client(10, false).is_ok());
        assert!(build_client(10, true).is_ok());
    }
}
