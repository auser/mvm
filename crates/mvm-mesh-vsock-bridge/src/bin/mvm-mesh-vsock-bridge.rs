//! In-guest TCP↔vsock bridge binary (ADR-0018 / ADR-0020).
//!
//! Loads `mesh_loopback_bindings` from the config disk, binds a TCP
//! listener per binding, and (for each accepted TCP connection) opens
//! a vsock stream to mvmd-agent on the host, writes the
//! length-prefixed peer header, then proxies bytes both ways.
//!
//! Capability tokens NEVER appear here — they're attached by
//! mvmd-agent on the host side. This binary is iroh-free; `cargo tree
//! -p mvm-mesh-vsock-bridge` MUST NOT contain any `iroh*` crate.
//!
//! v1 implementation note: scaffold today. The peer-header wire
//! format and load_bindings primitive are functional + unit-tested
//! (see `lib.rs`). The actual TCP listeners + vsock dial + bytes-
//! proxy loop land as the implementation phase of tinylabscom/mvm#95.

use anyhow::{Context, Result};
use mvm_mesh_vsock_bridge::load_bindings;
use std::env;
use std::path::PathBuf;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let bindings_path: PathBuf = env::var_os("MVM_MESH_LOOPBACK_BINDINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/mvm/mesh_loopback_bindings.json"));

    let bindings = if bindings_path.exists() {
        load_bindings(&bindings_path).with_context(|| {
            format!(
                "failed to load mesh loopback bindings from {}",
                bindings_path.display()
            )
        })?
    } else {
        // No-op mode: launch.json declared no addons. Idle and let
        // the supervisor's respawn loop manage us. Matches the
        // "always-install + no-op when bindings empty" pattern from
        // `specs/contracts/in-guest-mesh-dns.md`.
        tracing::info!(
            bindings_path = %bindings_path.display(),
            "no bindings file present; idling (no-op mode)"
        );
        loop {
            std::thread::park();
        }
    };

    tracing::info!(bindings = bindings.len(), "loaded mesh loopback bindings");

    if bindings.is_empty() {
        loop {
            std::thread::park();
        }
    }

    // Real listener wiring — TCP listener per binding, vsock dial via
    // libc AF_VSOCK, bidirectional proxy loop with half-close
    // semantics — lands as the implementation phase of
    // tinylabscom/mvm#95. The peer-header encode/decode + binding
    // loader in `lib.rs` are unit-tested and ready for that wire-up.
    tracing::error!("bridge wire-up not yet implemented; tracked by tinylabscom/mvm#95");
    std::process::exit(1);
}
