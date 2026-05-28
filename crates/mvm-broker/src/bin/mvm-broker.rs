//! `mvm-broker` binary — the general-broker subprocess entry point
//! (Plan 104 §H-L1.3, ADR-061 §"Decision").
//!
//! Spawn contract (W1a):
//!
//! 1. The supervisor cosign-verifies this binary at spawn (§H-L3.1 —
//!    supervisor side, lands in W1b).
//! 2. The supervisor spawns this process under a workload-specific
//!    cgroup + PID/mount namespace + seccomp + setpriv (§H-L1.4,
//!    §H-L3.3, §H-L3.9 — supervisor side, lands in W1b).
//! 3. The supervisor writes a JSON [`SubprocessConfig`] to this
//!    process's stdin, then closes stdin. W1a parses unsigned;
//!    W1b will require a signed envelope (§H-L3.6, G1).
//! 4. This process binds a UDS at `cfg.uds_path` (mode 0600 set by the
//!    supervisor on the parent dir) and enters the dispatch loop.
//! 5. It exits when the supervisor dies (parent-death signal — Linux
//!    `PR_SET_PDEATHSIG`, macOS kqueue parent-pid watcher — wired by
//!    the supervisor side in W1b; defensive double-attach in this
//!    binary lands at the same time).
//!
//! No handlers are registered in W1a, so every call returns
//! `Err(NotBound)`. That's the W1a acceptance criterion per Plan 104
//! §Build sequence W1.

use std::io::Read;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UnixListener;
use tracing::{error, info};

use mvm_broker::config::{SubprocessConfig, parse as parse_config};
use mvm_broker::registry::Registry;
use mvm_broker::server::serve_on_listener;

fn read_stdin_blocking() -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    std::io::stdin()
        .lock()
        .read_to_end(&mut buf)
        .context("mvm-broker stdin read failed")?;
    Ok(buf)
}

fn main() -> Result<()> {
    // The supervisor spawns with stdout/stderr captured for the audit
    // log; structured logging is the only useful trace.
    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_writer(std::io::stderr)
        .json()
        .init();

    let raw = read_stdin_blocking()?;
    let cfg: SubprocessConfig = parse_config(&raw).context("mvm-broker config parse failed")?;
    info!(
        workload_id = %cfg.workload_id,
        tenant_id = %cfg.tenant_id,
        uds_path = %cfg.uds_path.display(),
        max_frame_bytes = cfg.max_frame_bytes,
        "mvm-broker config loaded"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("mvm-broker")
        .build()
        .context("mvm-broker tokio runtime build failed")?;

    runtime.block_on(async move {
        let listener = UnixListener::bind(&cfg.uds_path)
            .with_context(|| format!("mvm-broker UDS bind failed on {}", cfg.uds_path.display()))?;
        let registry = Arc::new(Registry::new());
        info!(
            uds_path = %cfg.uds_path.display(),
            "mvm-broker listening; no handlers registered (W1a — every call returns NotBound)"
        );
        if let Err(e) = serve_on_listener(
            listener,
            registry,
            cfg.workload_id,
            cfg.tenant_id,
            cfg.max_frame_bytes,
        )
        .await
        {
            error!(error = %e, "mvm-broker serve loop exited with error");
            return Err::<(), _>(e);
        }
        Ok(())
    })?;

    Ok(())
}
