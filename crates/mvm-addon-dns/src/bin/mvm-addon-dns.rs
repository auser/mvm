//! In-guest DNS resolver binary for local addons.
//!
//! Listens on `127.0.0.1:53` and `::1:53`, serves `*.addon.local`
//! from a config-disk zone, forwards everything else upstream.
//! SIGHUP reloads the zone without dropping in-flight queries.
//!
//! v1 implementation note: this binary is a scaffold today.
//! Zone-loading and record matching are functional (see `lib.rs`
//! tests). Wiring up the actual hickory-dns request handler +
//! upstream-forwarding chain + SIGHUP loop lands as the issue's
//! follow-up implementation.

use anyhow::{Context, Result};
use mvm_addon_dns::{Zone, load_zone};
use std::env;
use std::path::PathBuf;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // The config disk mounts addon_dns_zone at a well-known path. v1
    // accepts an env-var override for testability; the production
    // path is fixed by mvm's init scripts.
    let zone_path: PathBuf = env::var_os("MVM_ADDON_DNS_ZONE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/mvm/addon_dns_zone.json"));

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

    // Real resolver wiring — hickory-dns request handler bound to
    // 127.0.0.1:53 + ::1:53 — lands as the implementation phase of
    // a follow-up issue. The Zone and load_zone primitives in `lib.rs`
    // are unit-tested and ready for that wire-up.
    tracing::error!("resolver wire-up not yet implemented");
    std::process::exit(1);
}
