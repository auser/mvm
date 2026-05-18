//! In-guest DNS resolver binary for local addons.
//!
//! Listens on `127.0.0.1:53` and `::1:53`, serves exact configured
//! hostnames from a config-disk zone, forwards everything else upstream.

use anyhow::{Context, Result};
use mvm_addon_dns::{
    DnsServerConfig, Zone, load_upstreams_from_resolv_conf, load_zone, run_udp_server,
};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // The config disk mounts addon_dns_zone at a well-known path. The binary
    // accepts an env-var override for testability; the production
    // path is fixed by mvm's init scripts.
    let zone_path: PathBuf = env::var_os("MVM_ADDON_DNS_ZONE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/mvm/addon_dns_zone.json"));
    let upstream_resolv_path: PathBuf = env::var_os("MVM_ADDON_DNS_UPSTREAM_RESOLV_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/mvm/upstream-resolv.conf"));

    let records = if zone_path.exists() {
        load_zone(&zone_path).with_context(|| {
            format!("failed to load addon DNS zone from {}", zone_path.display())
        })?
    } else {
        // No-op mode: the consumer's launch.json declared no addons.
        // Idle and let the supervisor's respawn loop manage us.
        tracing::info!(
            zone_path = %zone_path.display(),
            "no zone file present; idling (no-op mode)"
        );
        vec![]
    };

    let zone = Zone::new(records);
    tracing::info!(records = zone.len(), "loaded addon DNS zone");

    if zone.is_empty() {
        // Idle in no-op mode. This matches the "always-install + no-op
        // when zone empty" pattern declared in
        // `specs/contracts/local-addon-dns.md` so `mkGuest` doesn't
        // need a new distributed-mesh argument.
        loop {
            std::thread::park();
        }
    }

    let mut config = DnsServerConfig::production_default();
    if let Some(bind_addrs) = env::var_os("MVM_ADDON_DNS_BIND_ADDRS") {
        config.bind_addrs = parse_socket_addr_list(&bind_addrs.to_string_lossy())
            .context("failed to parse MVM_ADDON_DNS_BIND_ADDRS")?;
    }
    if let Some(upstream_addrs) = env::var_os("MVM_ADDON_DNS_UPSTREAM_ADDRS") {
        config.upstream_addrs = parse_socket_addr_list(&upstream_addrs.to_string_lossy())
            .context("failed to parse MVM_ADDON_DNS_UPSTREAM_ADDRS")?;
    } else if upstream_resolv_path.exists() {
        config.upstream_addrs = load_upstreams_from_resolv_conf(&upstream_resolv_path)
            .with_context(|| {
                format!(
                    "failed to load addon DNS upstream resolvers from {}",
                    upstream_resolv_path.display()
                )
            })?;
    }

    run_udp_server(zone, config)
        .await
        .context("addon DNS UDP server failed")
}

fn parse_socket_addr_list(value: &str) -> Result<Vec<SocketAddr>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<SocketAddr>()
                .with_context(|| format!("invalid socket address {part:?}"))
        })
        .collect()
}
