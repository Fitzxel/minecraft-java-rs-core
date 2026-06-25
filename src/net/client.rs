//! Centralised reqwest client construction and transport-error description.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::Deserialize;

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

/// A DNS-over-HTTPS resolver pointed at a fixed resolver IP (e.g. `1.1.1.1`).
///
/// Names are resolved with JSON DoH queries to `https://<ip>/dns-query`,
/// connecting to the resolver by its *literal IP* so no system DNS lookup is
/// needed to bootstrap. This bypasses both ISP DNS hijacking/poisoning **and**
/// port-53 blocking — the failure modes behind "works over a VPN, fails on this
/// network" that a plain change of nameserver cannot fix.
///
/// When `ipv4_only` is set, only A records are requested (composes with
/// `force_ipv4`).
#[derive(Debug)]
struct DohResolver {
    /// Bootstrap client. It MUST NOT use a custom resolver itself: it connects
    /// to the DoH endpoint by literal IP, so wiring it through `DohResolver`
    /// would recurse forever.
    client: reqwest::Client,
    /// Fully-formed endpoint, e.g. `https://1.1.1.1/dns-query`.
    endpoint: String,
    /// Resolver IP, kept for human-readable diagnostics (`MJRS_DNS_DEBUG`).
    resolver: IpAddr,
    ipv4_only: bool,
}

/// Subset of the Cloudflare/Google JSON DoH response we care about.
#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

#[derive(Deserialize)]
struct DohAnswer {
    /// Record payload. For A/AAAA it is an IP literal; for CNAME etc. it is a
    /// hostname, which simply fails to parse and is skipped.
    data: String,
}

impl DohResolver {
    async fn query(
        client: reqwest::Client,
        endpoint: String,
        host: String,
        rtype: &'static str,
    ) -> Result<Vec<IpAddr>, Box<dyn std::error::Error + Send + Sync>> {
        let resp = client
            .get(&endpoint)
            .query(&[("name", host.as_str()), ("type", rtype)])
            .header("accept", "application/dns-json")
            .send()
            .await?
            .error_for_status()?
            .json::<DohResponse>()
            .await?;
        Ok(resp
            .answer
            .into_iter()
            .filter_map(|a| a.data.parse::<IpAddr>().ok())
            .collect())
    }
}

impl Resolve for DohResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let resolver = self.resolver;
        let ipv4_only = self.ipv4_only;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Opt-in diagnostics: silent unless MJRS_DNS_DEBUG is set. Lets users
            // (and support) confirm resolution actually flows through DoH without
            // a packet capture.
            let debug = std::env::var_os("MJRS_DNS_DEBUG").is_some();

            let mut ips =
                match DohResolver::query(client.clone(), endpoint.clone(), host.clone(), "A").await
                {
                    Ok(v) => v,
                    Err(e) => {
                        if debug {
                            eprintln!("[dns] DoH via {resolver} → {host} = ERROR ({e})");
                        }
                        return Err(e);
                    }
                };
            if !ipv4_only {
                // A missing/failing AAAA lookup is non-fatal: most hosts that
                // resolve over IPv4 simply have no IPv6 record.
                if let Ok(v6) = DohResolver::query(client, endpoint, host.clone(), "AAAA").await {
                    ips.extend(v6);
                }
            }
            if debug {
                eprintln!("[dns] DoH via {resolver} → {host} = {ips:?}");
            }
            let addrs: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Build a reqwest client with the shared launcher configuration.
///
/// DNS behaviour, in precedence order:
/// - `dns = Some(ip)` → resolve every name via DNS-over-HTTPS against that
///   resolver IP (see [`DohResolver`]); honours `force_ipv4` by requesting only
///   A records.
/// - `dns = None` and `force_ipv4 = true` → use the system resolver but keep
///   only IPv4 results (see [`Ipv4OnlyResolver`]).
/// - otherwise → reqwest's default system resolver.
pub fn build_client(
    timeout_secs: u64,
    force_ipv4: bool,
    dns: Option<IpAddr>,
) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs));
    match dns {
        Some(ip) => {
            let boot = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()?;
            let endpoint = match ip {
                IpAddr::V4(v4) => format!("https://{v4}/dns-query"),
                IpAddr::V6(v6) => format!("https://[{v6}]/dns-query"),
            };
            builder = builder.dns_resolver(Arc::new(DohResolver {
                client: boot,
                endpoint,
                resolver: ip,
                ipv4_only: force_ipv4,
            }));
        }
        None if force_ipv4 => {
            builder = builder.dns_resolver(Arc::new(Ipv4OnlyResolver));
        }
        None => {}
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
    fn build_client_succeeds_in_all_modes() {
        assert!(build_client(10, false, None).is_ok());
        assert!(build_client(10, true, None).is_ok());
        assert!(build_client(10, false, Some("1.1.1.1".parse().unwrap())).is_ok());
        assert!(build_client(10, true, Some("1.1.1.1".parse().unwrap())).is_ok());
    }

    /// Live check that the DoH path resolves real names end-to-end: rustls must
    /// accept Cloudflare's cert when connecting to the literal IP `1.1.1.1`, and
    /// the resolver must hand reqwest at least one usable address.
    #[tokio::test]
    #[ignore = "requires internet: queries Cloudflare DoH at 1.1.1.1"]
    async fn doh_resolver_resolves_real_host() {
        // Also exercises the MJRS_DNS_DEBUG diagnostics path; run with
        // `--ignored --nocapture` to see the `[dns] DoH via …` line.
        std::env::set_var("MJRS_DNS_DEBUG", "1");

        let resolver = DohResolver {
            client: reqwest::Client::builder().build().unwrap(),
            endpoint: "https://1.1.1.1/dns-query".to_owned(),
            resolver: "1.1.1.1".parse().unwrap(),
            ipv4_only: true,
        };
        let name: Name = "resources.download.minecraft.net".parse().unwrap();
        let addrs: Vec<SocketAddr> = resolver.resolve(name).await.unwrap().collect();
        assert!(!addrs.is_empty(), "DoH returned no addresses");
        assert!(addrs.iter().all(SocketAddr::is_ipv4), "ipv4_only honoured");

        std::env::remove_var("MJRS_DNS_DEBUG");
    }
}
