//! Per-variant arm bodies for `mvm-bridge` (Plan 113, ADR-064).
//!
//! `main()` parses [`crate::parse::BridgeConfigJson`] from stdin and
//! then dispatches on the [`crate::parse::EndpointSpec`] discriminator
//! to one of:
//!
//!   * [`run_passt`] (Linux-only) — verify the operator-pinned `passt`
//!     binary hash against the allowlist, install `mvm-jailer-lite`
//!     confinement, reconstruct the parent-inherited socketpair fds,
//!     and hand the loop to
//!     `mvm_supervisor::gateway_bridge::spawn_bridge_thread` under
//!     `BridgeEndpoints::Passt`.
//!   * [`run_vz_ingest`] — bind the Swift `mvm-vz-supervisor`'s
//!     NDJSON `FlowEventWire` socket and dispatch under
//!     `BridgeEndpoints::VzIngest`.
//!
//! Both arms END by calling `spawn_bridge_thread`; the binary's
//! `main()` is responsible for the final `loop { park() }`.
//!
//! ## Trust model (shared)
//!
//! The bridge's stdin contract mirrors `mvm-libkrun-supervisor`'s:
//! the producer (`mvm-backend`) is trusted and has already verified
//! the signed plan envelope via `mvm-cli`'s `admit_for_run` path
//! before launch. Either arm parses the inner [`ExecutionPlan`] body
//! directly without an additional envelope check. Re-verifying the
//! envelope at this leaf would require host signer state
//! (`mvm-cli::host_signer`) which the bridge cannot reach without
//! closing a dependency cycle (`mvm-cli → mvm-supervisor → mvm-cli`).
//! ADR-002 names the host as in-scope; the bridge runs in the same
//! TCB as the supervisor.
//!
//! ## Capability profile (per arm)
//!
//! ADR-064 §Decision 8:
//!   * Passt arm reports `payload_tap: true` (direct virtio-net
//!     byte-stream access via parent-inherited socketpair fds).
//!   * VzIngest arm reports `payload_tap: false` (Swift emits flow
//!     open/close events only). Observers requiring `payload_tap`
//!     refuse at `Pipeline::observe` time with
//!     `BuildError::CapabilityMismatch`. A future plan extends Swift
//!     `Config.swift` to close that gap.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::SigningKey;
use mvm_plan::ExecutionPlan;
use mvm_policy::PolicyBundle;
use mvm_supervisor::audit::AuditSigner;
use mvm_supervisor::audit_file::FileAuditSigner;
use mvm_supervisor::gateway_bridge::{
    AllowAll, BridgeConfig, BridgeEndpoints, spawn_bridge_thread,
};
use mvm_supervisor::network::{ObserverAllowlist, ProviderCapabilities, from_admitted};

use crate::parse::BridgeConfigJson;

// ─── Shared helpers ────────────────────────────────────────────────

/// Read the host signer secret bytes from `signing_key_path` and
/// wrap them in a `FileAuditSigner` rooted at `audit_dir`. Shared by
/// both arms — the per-tenant cross-process flock inside
/// `FileAuditSigner` (Plan 102 W6.A commit 2) keeps concurrent
/// supervisors safe.
fn build_file_signer(
    signing_key_path: &std::path::Path,
    audit_dir: &std::path::Path,
) -> Result<Arc<dyn AuditSigner>> {
    let key_bytes = std::fs::read(signing_key_path)
        .with_context(|| format!("read signing key {}", signing_key_path.display()))?;
    let key_array: [u8; 32] = key_bytes.as_slice().try_into().with_context(|| {
        format!(
            "signing key {} is {} bytes, expected 32",
            signing_key_path.display(),
            key_bytes.len()
        )
    })?;
    let signing_key = SigningKey::from_bytes(&key_array);
    let file_signer = FileAuditSigner::open(signing_key, audit_dir)
        .with_context(|| format!("open FileAuditSigner at {}", audit_dir.display()))?;
    Ok(Arc::new(file_signer))
}

/// Resolve the observer chain from the admitted plan + host
/// allowlist. `leaf_caps` is backend-specific and supplied by the
/// caller arm.
fn resolve_observers(
    plan: &ExecutionPlan,
    leaf_caps: ProviderCapabilities,
) -> Result<Vec<Arc<dyn mvm_supervisor::network::Observer>>> {
    let allowlist = ObserverAllowlist::load_from_host_config()
        .map_err(|e| anyhow!("load ObserverAllowlist from ~/.mvm/observers/allowlist.toml: {e}"))?;
    from_admitted(plan, leaf_caps, &allowlist)
        .map_err(|e| anyhow!("resolve observer chain from admitted plan: {e}"))
}

// ─── Passt arm (Linux only) ────────────────────────────────────────

/// Linux/passt arm body. Verifies the operator-pinned `passt` SHA256
/// against `passt_hashes_path`, applies `mvm-jailer-lite`
/// confinement, reconstructs the parent-inherited socketpair fds via
/// `OwnedFd::from_raw_fd` (the only `unsafe` block in this arm), and
/// hands the packet loop to
/// `mvm_supervisor::gateway_bridge::spawn_bridge_thread` under
/// `BridgeEndpoints::Passt`.
///
/// Returns once the bridge thread is spawned; the caller (`main`)
/// parks the main thread forever.
///
/// ## File-descriptor inheritance contract
///
/// `gateway_fd_raw` + `supervisor_fd_raw` name file descriptors
/// already open in this process's fd table. Standard Rust
/// `std::process::Command` only inherits stdin/stdout/stderr;
/// `mvm-backend`'s spawner honours the bridge contract via
/// `CommandExt::pre_exec` — it `dup2`s the socketpair fds into
/// known raw positions, clears `O_CLOEXEC` on each, and then `exec`s
/// this binary. By the time `main` runs, the fds are inherited and
/// owned by this process; the arm takes ownership via
/// `OwnedFd::from_raw_fd` and never duplicates them.
#[cfg(target_os = "linux")]
pub fn run_passt(cfg: BridgeConfigJson) -> Result<()> {
    use mvm_jailer_lite::{ConfinementSpec, confine_self};
    use std::os::fd::{FromRawFd, OwnedFd};

    use crate::parse::{EndpointSpec, verify_passt_hash};

    // Split the envelope from the per-arm discriminator so the arm
    // body can consume the JSON strings (`plan_json` / `bundle_json`)
    // and the destructured passt fields independently. The
    // discriminator is moved out by `match endpoints { … }` below.
    let BridgeConfigJson {
        vm_name,
        audit_dir,
        audit_socket,
        signing_key_path,
        plan_json,
        bundle_json,
        endpoints,
    } = cfg;
    let (passt_path, passt_hashes_path, keys_dir, gateway_fd_raw, supervisor_fd_raw) =
        match endpoints {
            EndpointSpec::Passt {
                passt_path,
                passt_hashes_path,
                keys_dir,
                gateway_fd_raw,
                supervisor_fd_raw,
            } => (
                passt_path,
                passt_hashes_path,
                keys_dir,
                gateway_fd_raw,
                supervisor_fd_raw,
            ),
            EndpointSpec::VzIngest { .. } => {
                // Dispatched-to-wrong-arm — `main` selects on the
                // variant before calling here, so this is a logic
                // bug if it ever fires.
                return Err(anyhow!(
                    "run_passt called with EndpointSpec::VzIngest — dispatch bug"
                ));
            }
        };

    // ── Step 1: verify passt binary hash BEFORE confinement ─────────
    //
    // Landlock clamps reads to `passt_path` + `keys_dir` after
    // `confine_self`; if we ran the hash check after confinement, a
    // misconfigured `passt_hashes_path` would surface as a confusing
    // EACCES instead of "operator forgot to populate the allowlist".
    // Cardoso minimum-viable-policy: the operator-pinned allowlist
    // is the supply-chain gate; this is the right place for it.
    verify_passt_hash(&passt_path, &passt_hashes_path)
        .context("verify passt binary hash against operator allowlist")?;

    // ── Step 2: apply mvm-jailer-lite confinement ───────────────────
    //
    // After this call, the process can only:
    //   * read from `passt_path` + `keys_dir`
    //   * read/write under `audit_dir`
    //   * invoke the allowlisted syscalls (see
    //     `mvm_jailer_lite::seccomp::BRIDGE_SYSCALLS`)
    //
    // Per `confine_self`'s partial-confinement contract: any error
    // here MUST cause hard exit. We propagate up to `main()` which
    // turns the error into `ExitCode::FAILURE`; the parent's
    // watchdog sees the nonzero exit and tears down the VM.
    let spec = ConfinementSpec::firecracker_bridge(
        audit_dir.clone(),
        keys_dir.clone(),
        passt_path.clone(),
    );
    confine_self(&spec).context("apply mvm-jailer-lite confinement")?;

    // ── Step 3: decode trusted plan + bundle ────────────────────────
    let plan: mvm_plan::ExecutionPlan = serde_json::from_str(&plan_json)
        .context("decode BridgeConfigJson.plan_json into ExecutionPlan")?;
    let bundle: Option<PolicyBundle> = match bundle_json.as_deref() {
        Some(s) => Some(
            serde_json::from_str(s)
                .context("decode BridgeConfigJson.bundle_json into PolicyBundle")?,
        ),
        None => None,
    };

    // ── Step 4: load host signer key + build FileAuditSigner ────────
    //
    // The file is mode 0600 and was written by `mvm-cli::host_signer::
    // load_or_init_at` at admit time. Landlock granted read on
    // `keys_dir`; this read succeeds inside the ruleset.
    let signer = build_file_signer(&signing_key_path, &audit_dir)?;

    // ── Step 5: resolve observer chain from admitted plan ───────────
    //
    // Plan 113 §Task 4 — observer chain from admitted plan + host
    // allowlist. Passt reports `payload_tap: true` (ADR-064
    // §Decision 8) so payload-tap observers admit at the
    // `from_admitted` gate.
    let leaf_caps = ProviderCapabilities {
        flow_events: true,
        payload_tap: true,
    };
    let observers = resolve_observers(&plan, leaf_caps)?;

    tracing::info!(
        vm = %vm_name,
        tenant = %plan.tenant.0,
        audit_socket = %audit_socket.display(),
        audit_dir = %audit_dir.display(),
        passt_path = %passt_path.display(),
        gateway_fd = gateway_fd_raw,
        supervisor_fd = supervisor_fd_raw,
        observers = observers.len(),
        "starting mvm-bridge (passt arm); reconstructing socketpair fds"
    );

    // ── Step 6: reconstruct parent-inherited fds + build endpoints ──
    //
    // SAFETY: the caller MUST guarantee that
    //   1. `gateway_fd_raw` and `supervisor_fd_raw` name valid, open
    //      file descriptors in this process's fd table,
    //   2. those fds were duped (or socketpair'd) by the parent
    //      before exec and inherited across exec with `O_CLOEXEC`
    //      cleared,
    //   3. no other code in this process holds owning references to
    //      those fds (ownership transfers to the returned `OwnedFd`).
    // `mvm-backend`'s spawner honours this via the
    // `CommandExt::pre_exec` dup2 + fcntl(FD_CLOEXEC clear) path
    // documented in its module header. There is no validation we
    // can perform on the host side (the kernel will EBADF on first
    // use if the fd is bad, which the bridge thread surfaces via
    // `tokio::io::copy_bidirectional` → exit(1)).
    let (gateway_fd, supervisor_fd) = unsafe {
        (
            OwnedFd::from_raw_fd(gateway_fd_raw),
            OwnedFd::from_raw_fd(supervisor_fd_raw),
        )
    };

    let bridge_cfg = BridgeConfig {
        vm_name: vm_name.clone(),
        plan: Arc::new(plan),
        bundle: bundle.map(Arc::new),
        audit_socket,
        signer,
        policy: Arc::new(AllowAll),
        observers,
    };

    let endpoints = BridgeEndpoints::Passt {
        gateway_fd,
        supervisor_fd,
    };

    // ── Step 7: spawn the bridge thread ─────────────────────────────
    //
    // JoinHandle is intentionally dropped. The parent holds an
    // `AttachedBridgeGuard` that kills this process on early return /
    // panic / VM teardown; the bridge's own `catch_unwind → exit(1)`
    // is the fail-closed signal for the claim-10 substrate.
    let _join = spawn_bridge_thread(endpoints, bridge_cfg);

    tracing::info!(vm = %vm_name, "bridge thread spawned (passt arm); returning to main for park");
    Ok(())
}

// ─── Vz NDJSON ingest arm ──────────────────────────────────────────

/// macOS Vz NDJSON ingest arm body. Closes Plan 112's "Vz carve-out":
/// binds the `events_socket_path` (Swift `mvm-vz-supervisor`'s NDJSON
/// `FlowEventWire` output socket per PR #487 commit 6), reads NDJSON
/// lines, and threads the events through the same chain-signing audit
/// pipeline that `mvm-libkrun-supervisor` uses for the libkrun path.
///
/// Returns once the bridge thread is spawned; the caller (`main`)
/// parks the main thread forever.
pub fn run_vz_ingest(cfg: BridgeConfigJson) -> Result<()> {
    use crate::parse::EndpointSpec;

    // Split the envelope from the per-arm discriminator so the arm
    // body can consume both halves independently.
    let BridgeConfigJson {
        vm_name,
        audit_dir,
        audit_socket,
        signing_key_path,
        plan_json,
        bundle_json,
        endpoints,
    } = cfg;
    let events_socket_path = match endpoints {
        EndpointSpec::VzIngest { events_socket_path } => events_socket_path,
        EndpointSpec::Passt { .. } => {
            return Err(anyhow!(
                "run_vz_ingest called with EndpointSpec::Passt — dispatch bug"
            ));
        }
    };

    // Trust model (see module doc): producer has already verified the
    // envelope; parse the inner ExecutionPlan body directly.
    let plan: mvm_plan::ExecutionPlan = serde_json::from_str(&plan_json)
        .context("decode BridgeConfigJson.plan_json into ExecutionPlan")?;
    let bundle: Option<PolicyBundle> = match bundle_json.as_deref() {
        Some(s) => Some(
            serde_json::from_str(s)
                .context("decode BridgeConfigJson.bundle_json into PolicyBundle")?,
        ),
        None => None,
    };

    // Re-read the host signer secret bytes. The file is mode 0600
    // and was written by `mvm-cli::host_signer::load_or_init_at` at
    // admit time; the bridge trusts the path produced by the parent.
    let signer = build_file_signer(&signing_key_path, &audit_dir)?;

    // Plan 113 §Task 4 — observer chain from admitted plan + host
    // allowlist. ADR-064 §Decision 8: Vz reports `payload_tap:
    // false`. Observers that require payload tap refuse via
    // `BuildError::CapabilityMismatch` at the `from_admitted` call
    // below.
    let leaf_caps = ProviderCapabilities {
        flow_events: true,
        payload_tap: false,
    };
    let observers = resolve_observers(&plan, leaf_caps)?;

    tracing::info!(
        vm = %vm_name,
        tenant = %plan.tenant.0,
        audit_socket = %audit_socket.display(),
        audit_dir = %audit_dir.display(),
        events_socket = %events_socket_path.display(),
        observers = observers.len(),
        "starting mvm-bridge (vz_ingest arm); binding Swift NDJSON FlowEventWire socket"
    );

    let bridge_cfg = BridgeConfig {
        vm_name: vm_name.clone(),
        plan: Arc::new(plan),
        bundle: bundle.map(Arc::new),
        audit_socket,
        signer,
        policy: Arc::new(AllowAll),
        observers,
    };

    let endpoints = BridgeEndpoints::VzIngest { events_socket_path };

    // Bridge thread JoinHandle is intentionally dropped. The parent
    // holds an `AttachedBridgeGuard` that kills this process on
    // early return / panic / VM teardown; the bridge's own
    // `catch_unwind → exit(1)` is the fail-closed signal for the
    // claim-10 substrate.
    let _join = spawn_bridge_thread(endpoints, bridge_cfg);

    tracing::info!(vm = %vm_name, "bridge thread spawned (vz_ingest arm); returning to main for park");
    Ok(())
}
