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

/// Default response-body cap for search-provider impls. 1 MiB is
/// the working budget for "search result JSON" — real-world Brave /
/// Tavily / Google responses run ~10-50 KB. Providers can override
/// when their upstream returns larger payloads (e.g. an embedded
/// thumbnail) but should always carry *some* cap; uncapped reads
/// expose the supervisor to "send-gigabytes-of-JSON" DoS.
pub const DEFAULT_RESPONSE_BODY_CAP: usize = 1 << 20;

/// Read a `reqwest::Response`'s body, refusing to accumulate more
/// than `max_bytes`. Implementation mirrors
/// [`crate::tools::web_fetch::ReqwestHttpFetcher`]'s chunk loop —
/// the cap is enforced *before* a chunk that would overflow lands
/// in the accumulator, so the returned `Vec<u8>` is exactly
/// `≤ max_bytes` on success.
///
/// Returns `Ok(bytes)` on success or an error string when the
/// upstream wanted to send more. Callers wrap the string into their
/// own provider-specific error type.
pub async fn read_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("reading response chunk: {e}"))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!(
                "response body exceeded max_bytes ({max_bytes}); upstream wanted to send more \
                 (refusing; plan 65 follow-on)"
            ));
        }
        body.extend_from_slice(&chunk);
        debug_assert!(body.len() <= max_bytes);
    }
    Ok(body)
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

    // ──────────────────────────────────────────────────────────────
    // read_capped — live HTTP via one-shot 127.0.0.1 server
    //
    // Same harness pattern as `web_fetch::tests`: we bind a TCP
    // listener on 127.0.0.1:0, accept one connection, write a
    // hardcoded HTTP/1.1 response, and assert reqwest's behavior.
    // The SsrfFilteringResolver does NOT participate — these tests
    // build a stock reqwest::Client and feed it a real Response. We
    // only test the chunk-loop accumulator, not the client.
    // ──────────────────────────────────────────────────────────────

    async fn spawn_one_shot_server(response: String) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        port
    }

    async fn fetch_from_test_server(
        response_body: String,
        max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        let port = spawn_one_shot_server(response_body).await;
        // Bare client — bypassing SsrfFilteringResolver because the
        // test server binds to loopback. The accumulator under test
        // is the body of read_capped, which doesn't care about the
        // resolver.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        read_capped(response, max_bytes).await
    }

    #[tokio::test]
    async fn read_capped_returns_full_body_when_under_cap() {
        let payload = "hello world";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let body = fetch_from_test_server(response, 1024).await.unwrap();
        assert_eq!(body, payload.as_bytes());
    }

    #[tokio::test]
    async fn read_capped_refuses_oversize_response() {
        let payload = "A".repeat(40);
        let response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{payload}");
        let err = fetch_from_test_server(response, 16).await.unwrap_err();
        assert!(err.contains("exceeded max_bytes"), "{err}");
        assert!(err.contains("16"), "{err}");
    }

    #[tokio::test]
    async fn read_capped_succeeds_at_exact_boundary() {
        let payload = "B".repeat(16);
        let response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{payload}");
        let body = fetch_from_test_server(response, 16).await.unwrap();
        assert_eq!(body.len(), 16);
    }
}
