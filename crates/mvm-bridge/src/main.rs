//! Plan 113 / ADR-064 — per-VM gateway audit bridge sidecar binary.
//!
//! Unified host of `mvm-supervisor::gateway_bridge::run_bridge_inner`
//! that dispatches between two endpoint variants on a stdin
//! discriminator:
//!
//!   * `EndpointSpec::Passt` — Linux-only `endpoints::run_passt` arm.
//!     Carries the Firecracker substrate today; any future Linux
//!     backend fronting a virtio-net socketpair with passt (e.g.
//!     Cloud Hypervisor) reuses this arm without a new binary.
//!   * `EndpointSpec::VzIngest` — `endpoints::run_vz_ingest` arm.
//!     Closes Plan 112's "Vz carve-out" by binding Swift
//!     `mvm-vz-supervisor`'s NDJSON `FlowEventWire` socket.
//!
//! Spawned by `mvm-backend`'s per-backend spawner between the host
//! networking step (host passt / gvproxy `spawn_detached`) and the VM
//! boot. The parent owns an `AttachedBridgeGuard` that kills this
//! process on early return / panic / VM teardown; the bridge's own
//! `catch_unwind → exit(1)` is the fail-closed signal for the
//! claim-10 substrate.
//!
//! ## Parser surface
//!
//! `BridgeConfigJson`, `EndpointSpec`, `PasstHashesFile`, and
//! `verify_passt_hash` live in [`mvm_bridge::parse`] (the crate's
//! `src/lib.rs` / `src/parse.rs`). Plan 113 §Task 15's `fuzz` CI lane
//! drives `cargo fuzz` against those serde deserializers directly via
//! the crate's lib surface; the binary's `main()` uses the same parser
//! entry points so the fuzzed code path and the production code path
//! are byte-identical.
//!
//! ## Trust model
//!
//! Documented in [`mvm_bridge::endpoints`] — same shape on both arms.

use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result};
use mvm_bridge::endpoints;
use mvm_bridge::parse::{BridgeConfigJson, EndpointSpec};

fn main() -> ExitCode {
    // Stderr-only tracing keeps stdout clean for any future protocol
    // (the parent reads stdin only; we are not expected to print to
    // stdout). Same posture as `mvm-libkrun-supervisor`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "mvm-bridge exiting with error");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // ── Step 1: read + parse stdin contract ─────────────────────────
    let mut json = String::new();
    std::io::stdin()
        .read_to_string(&mut json)
        .context("read BridgeConfigJson from stdin")?;
    let cfg: BridgeConfigJson = serde_json::from_str(&json).context("parse BridgeConfigJson")?;

    // ── Step 2: dispatch on endpoint variant ────────────────────────
    //
    // We pattern-match only on the discriminator so the arm functions
    // can consume the owned config and the per-arm fields directly.
    match &cfg.endpoints {
        EndpointSpec::Passt { .. } => {
            #[cfg(target_os = "linux")]
            {
                endpoints::run_passt(cfg)?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                // The Linux arm is meaningless off Linux —
                // `mvm-jailer-lite::confine_self` returns
                // `Err(SeccompUnavailable)` on macOS/Windows stubs,
                // and the `BridgeEndpoints::Passt` path requires
                // Linux socketpair semantics the macOS Firecracker
                // port does not support. Refusing here keeps the
                // workspace build green on non-Linux contributor
                // hosts while making the runtime refusal explicit.
                drop(cfg);
                anyhow::bail!(
                    "mvm-bridge: EndpointSpec::Passt is Linux-only; this binary \
                     was built for a non-Linux target and refuses to run the \
                     passt arm. Use the VzIngest variant for macOS Vz."
                );
            }
        }
        EndpointSpec::VzIngest { .. } => {
            endpoints::run_vz_ingest(cfg)?;
        }
    }

    // ── Step 3: park forever ────────────────────────────────────────
    //
    // Both arms return after spawning the bridge thread; the parent
    // kills us via SIGTERM/SIGKILL on VM shutdown. Without `park`,
    // the bin would exit immediately and the OS would reap the
    // bridge thread before the first FlowEvent arrives. A loop
    // guards against spurious unparks.
    loop {
        std::thread::park();
    }
}
