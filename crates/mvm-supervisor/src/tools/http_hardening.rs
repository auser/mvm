//! Shared HTTP-client hardening for Phase 7 tools.
//!
//! Until this module existed, only [`crate::tools::web_fetch::ReqwestHttpFetcher`]
//! had the full plan-65 hardening posture. Each search provider
//! (`BraveSearchProvider`, `TavilySearchProvider`,
//! `GoogleSearchProvider`) built its own bare `reqwest::Client` with
//! `timeout` set but neither `Policy::none()` redirects nor any
//! SSRF guarding — meaning DNS poisoning of `api.search.brave.com`
//! to a private IP would have routed credentials at a local
//! attacker before plan 65 caught it.
//!
//! This module exports a [`hardened_client_builder`] that every
//! reqwest-using tool surface goes through. It carries:
//!
//! - **W1 — no auto-redirect**: `reqwest::redirect::Policy::none()`.
//!   An upstream that responds 3xx surfaces the status code +
//!   headers to the caller; nothing follows silently.
//! - **W2 — SSRF / DNS-rebinding defense**: a
//!   [`SsrfFilteringResolver`] that wraps the system resolver
//!   ([`tokio::net::lookup_host`]) and discards every returned IP
//!   that [`SsrfGuard::classify`] rejects — RFC1918, loopback,
//!   link-local, cloud metadata (169.254.169.254), CGNAT,
//!   IPv6 unique-local, etc. If *every* resolved IP is blocked,
//!   `resolve()` returns an error mentioning the SSRF guard so
//!   the operator sees the cause; if *any* IPs survive, only the
//!   safe set is handed to reqwest.
//!
//! ## Difference from [`crate::tools::web_fetch::ReqwestHttpFetcher`]'s pre-resolve
//!
//! The fetcher does a *separate* `tokio::net::lookup_host` in the
//! tool layer before constructing the per-call client, and uses
//! a `PinnedDnsResolver` that only knows the pre-validated IPs.
//! That gives the clearest possible error message ("DNS for
//! api.allowed.example resolves to blocked address(es)...") but
//! requires a fresh client per fetch.
//!
//! Providers reuse one long-lived `reqwest::Client` per provider
//! instance, so the lazy [`SsrfFilteringResolver`] is the right
//! fit — DNS lookups happen during reqwest's connect phase and
//! the filtering runs there too. The error message is slightly
//! less specific (reqwest wraps the resolver error) but the
//! security posture is the same.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::ssrf_guard::SsrfGuard;

/// Build a `reqwest::ClientBuilder` pre-configured with W1 + W2
/// hardening. Callers add their own per-tool config (headers,
/// user-agent, etc.) before `.build()`.
pub fn hardened_client_builder(timeout_secs: u64) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(SsrfFilteringResolver))
}

/// reqwest `Resolve` impl that delegates to the system resolver
/// and filters every returned IP through
/// [`SsrfGuard::classify`]. Stateless — one instance per program
/// is fine.
#[derive(Debug, Default)]
pub struct SsrfFilteringResolver;

impl Resolve for SsrfFilteringResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // The port we pass to `lookup_host` is a placeholder —
            // reqwest reads the IP out of each `SocketAddr` and
            // uses the URL's port for the actual connect. 443 is
            // a reasonable default since most callers are HTTPS.
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 443u16))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            let filtered = filter_ssrf_addrs(resolved)
                .map_err(|msg| -> Box<dyn std::error::Error + Send + Sync> { msg.into() })?;
            Ok(Box::new(filtered.into_iter()) as Addrs)
        })
    }
}

/// Filter a list of resolved addresses through the SSRF guard.
///
/// Returns the safe subset on success. Returns an error if **every**
/// input address was rejected (so the caller can surface a clear
/// "all addresses are SSRF-blocked" message instead of a confusing
/// "no addresses to connect to"). If some IPs are safe + some are
/// blocked, the blocked ones are silently dropped — defense in
/// depth, not an audit signal, so a partial-block scenario doesn't
/// fail the whole call.
pub fn filter_ssrf_addrs(
    addrs: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    let mut blocked: Vec<String> = Vec::new();
    let mut safe: Vec<SocketAddr> = Vec::new();
    for sa in addrs {
        match SsrfGuard::classify(sa.ip()) {
            Some(reason) => blocked.push(format!("{} ({reason})", sa.ip())),
            None => safe.push(sa),
        }
    }
    if safe.is_empty() && !blocked.is_empty() {
        return Err(format!(
            "SSRF guard rejected all resolved addresses: {} \
             (refusing to fetch; plan 65 W2)",
            blocked.join(", ")
        ));
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn sa(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)
    }

    #[test]
    fn filter_passes_public_ip() {
        let out = filter_ssrf_addrs([sa([8, 8, 8, 8], 443)]).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn filter_rejects_when_only_loopback() {
        let err = filter_ssrf_addrs([sa([127, 0, 0, 1], 443)]).unwrap_err();
        assert!(err.contains("SSRF guard"), "{err}");
        assert!(err.contains("loopback"), "{err}");
        assert!(err.contains("127.0.0.1"), "{err}");
    }

    #[test]
    fn filter_rejects_when_only_imds() {
        let err = filter_ssrf_addrs([sa([169, 254, 169, 254], 80)]).unwrap_err();
        assert!(err.contains("metadata"), "{err}");
    }

    #[test]
    fn filter_rejects_when_only_rfc1918() {
        let err = filter_ssrf_addrs([sa([10, 0, 0, 1], 443)]).unwrap_err();
        assert!(err.contains("RFC1918"), "{err}");
    }

    #[test]
    fn filter_drops_blocked_keeps_safe_when_mixed() {
        // Two upstream addresses; one public + one private. The
        // public one survives; the private is silently dropped.
        // (Defense in depth — we don't fail the whole call just
        // because one of several IPs is bad. The audit signal lives
        // at the per-call layer in ReqwestHttpFetcher, not here.)
        let out = filter_ssrf_addrs([sa([8, 8, 8, 8], 443), sa([10, 0, 0, 1], 443)]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ip(), IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn filter_passes_empty_input() {
        // An empty resolution result isn't a security failure; let
        // reqwest surface "no addresses" through its own error path.
        let out = filter_ssrf_addrs(std::iter::empty()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn hardened_client_builds_successfully() {
        // Smoke: the builder returns a real ClientBuilder we can
        // turn into a Client. Catches a future refactor that
        // accidentally breaks the chain.
        let client = hardened_client_builder(15).build();
        assert!(client.is_ok());
    }
}
