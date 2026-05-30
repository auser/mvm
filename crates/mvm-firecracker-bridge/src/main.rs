//! Plan 113 §Task 12 / ADR-064 — per-VM Firecracker bridge sidecar.
//!
//! Linux-only A2 process that runs alongside every Firecracker microVM
//! the host launches via `mvm-backend::firecracker`. Reads a
//! [`BridgeConfigJson`] document from stdin, verifies the operator-
//! pinned `passt` binary hash, applies `mvm-jailer-lite` confinement
//! (seccomp + Landlock — Plan 113 §Task 8), reconstructs the parent-
//! inherited socketpair fds into a [`BridgeEndpoints::Passt`] pair, and
//! hands the packet loop to `mvm_supervisor::gateway_bridge::spawn_bridge_thread`.
//!
//! Spawned by Plan 113 §Task 13's `FirecrackerBackend::start` between
//! the host passt `spawn_detached` step and the Firecracker VM boot.
//! The parent owns an `AttachedBridgeGuard` that kills this process on
//! early return / panic / VM teardown; the bridge's own `catch_unwind →
//! exit(1)` is the fail-closed signal for the claim-10 substrate.
//!
//! ## Trust model
//!
//! The bridge's stdin contract is identical to `mvm-vz-drainer`'s and
//! `mvm-libkrun-supervisor`'s: the producer (Task 13's
//! `FirecrackerBackend`) is trusted and has already verified the
//! signed plan envelope via `mvm-cli`'s `admit_for_run` path before
//! launch. The bridge parses the plan JSON directly into an
//! [`ExecutionPlan`] without an additional envelope check — mirroring
//! `mvm-vz-drainer`'s pattern (PR a51fbc7f / Task 10) and
//! `mvm-libkrun-supervisor`'s. Re-verification of the plan envelope at
//! this leaf would require host signer state (`mvm-cli::host_signer`)
//! which the bridge cannot reach without closing a dependency cycle
//! (`mvm-cli → mvm-supervisor → mvm-cli`). ADR-002 names the host as
//! in-scope; the bridge runs in the same TCB as the supervisor.
//!
//! ## File-descriptor inheritance contract
//!
//! `gateway_fd_raw` + `supervisor_fd_raw` in [`BridgeConfigJson`] are
//! raw fd numbers (`i32`) that name file descriptors already open in
//! this process's fd table. **Standard Rust `std::process::Command`
//! only inherits stdin/stdout/stderr;** Task 13's `FirecrackerBackend`
//! honours the bridge contract via `CommandExt::pre_exec` — it
//! `dup2`s the socketpair fds into known raw positions, clears
//! `O_CLOEXEC` on each, and then `exec`s this binary. By the time
//! `main` runs, the fds are inheritied and owned by this process; the
//! bridge takes ownership via `OwnedFd::from_raw_fd` (the only
//! `unsafe` block in the file) and never duplicates them.
//!
//! ## Capability profile
//!
//! ADR-064 §Decision 8 — Firecracker leaves report
//! `payload_tap: true`. The bridge sits directly on the virtio-net
//! byte stream between the guest and host passt, so payload-tap
//! observers (a future SNI inspector / L7 MITM) can plug into the same
//! `FlowPolicy` seam libkrun uses today.

#[cfg(target_os = "linux")]
use anyhow::{Context, Result, anyhow};
#[cfg(target_os = "linux")]
use ed25519_dalek::SigningKey;
#[cfg(target_os = "linux")]
use mvm_jailer_lite::{ConfinementSpec, confine_self};
#[cfg(target_os = "linux")]
use mvm_plan::ExecutionPlan;
#[cfg(target_os = "linux")]
use mvm_policy::PolicyBundle;
#[cfg(target_os = "linux")]
use mvm_supervisor::audit::AuditSigner;
#[cfg(target_os = "linux")]
use mvm_supervisor::audit_file::FileAuditSigner;
#[cfg(target_os = "linux")]
use mvm_supervisor::gateway_bridge::{
    AllowAll, BridgeConfig, BridgeEndpoints, spawn_bridge_thread,
};
#[cfg(target_os = "linux")]
use mvm_supervisor::network::{ObserverAllowlist, ProviderCapabilities, from_admitted};
#[cfg(target_os = "linux")]
use serde::Deserialize;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::sync::Arc;

/// Stdin JSON contract. Producer is Task 13's `FirecrackerBackend`.
/// All paths are absolute and already-canonicalised by the parent.
#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfigJson {
    /// VM name; used to label the bridge thread + the audit chain
    /// `vm` field.
    vm_name: String,

    /// `~/.mvm/audit/` — destination of the chain-signed JSONL log
    /// that `FileAuditSigner` appends to. Shared with sibling VMs;
    /// the cross-process flock inside `FileAuditSigner` serialises
    /// writes per tenant.
    audit_dir: PathBuf,

    /// `~/.mvm/audit/gateway-<vm>.sock` — subscriber socket the
    /// bridge binds at startup so `nc -U <path>` consumers see the
    /// live NDJSON flow-event tail. Same shape as the libkrun /
    /// Vz drainer paths.
    audit_socket: PathBuf,

    /// `~/.mvm/keys/` — directory containing the host signer key.
    /// Used to scope the Landlock confinement spec; the actual
    /// secret bytes are read from `signing_key_path` below before
    /// confinement clamps the bridge to the spec.
    keys_dir: PathBuf,

    /// `~/.mvm/keys/host-signer.ed25519` — mode 0600 file owned by
    /// the calling user. The bridge re-reads it on each launch to
    /// seed the `FileAuditSigner`'s `SigningKey`.
    signing_key_path: PathBuf,

    /// Path to the operator-installed `passt` binary the bridge
    /// relays packets to. The bridge verifies its SHA256 against
    /// `passt_hashes_path` before installing confinement and before
    /// touching the inherited fds.
    passt_path: PathBuf,

    /// `~/.mvm/passt-hashes.toml` — operator-curated allowlist of
    /// accepted `passt` binary SHA256 hashes. See
    /// [`PasstHashesFile`].
    passt_hashes_path: PathBuf,

    /// Raw fd number of the parent half of the passt socketpair.
    /// Task 13's `pre_exec` `dup2`s the parent socketpair fd into
    /// this slot and clears `O_CLOEXEC` so the kernel preserves
    /// the fd across exec. The bridge takes ownership via
    /// `OwnedFd::from_raw_fd`.
    gateway_fd_raw: i32,

    /// Raw fd number of the supervisor half of the inner virtio-net
    /// socketpair whose other half is plumbed into Firecracker.
    /// Same `pre_exec` contract as `gateway_fd_raw`.
    supervisor_fd_raw: i32,

    /// Serialised `SignedExecutionPlan` envelope as produced by
    /// `mvm-cli::plan_admission::populate_audit_substrate`. Trust
    /// model in the module doc: the bridge parses the inner
    /// `ExecutionPlan` body directly without re-verifying the
    /// envelope.
    plan_json: String,

    /// Optional serialised `PolicyBundle` (the resolved bundle pin
    /// rather than the bundle archive itself; the bridge uses it
    /// to label flow-event audit entries with the bundle digest).
    #[serde(default)]
    bundle_json: Option<String>,
}

/// On-disk format of `~/.mvm/passt-hashes.toml`. Operator-managed —
/// the `passt` binary is operator-installed (apt / dnf / nix /
/// source build), so the SHA256 is operator-pinned per-host. CI
/// fixtures use `tempfile` to construct a valid file; never falls
/// back to a default-trust posture.
#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PasstHashesFile {
    /// Lowercase hex SHA256 strings (64 hex chars each, no `0x`
    /// prefix). An empty list is a hard failure — the bridge cannot
    /// admit any `passt` binary because there is nothing to match
    /// against.
    sha256: Vec<String>,
}

/// Verify the `passt` binary at `passt_path` SHA256-matches one of
/// the entries in `hashes_path`. Defence in depth (Cardoso minimum-
/// viable-policy): the operator-pinned allowlist is checked *before*
/// the bridge installs Landlock + seccomp, so the error path can
/// still read the binary and produce a clean remediation hint.
///
/// Fails closed on:
///   * `hashes_path` missing or unreadable
///   * `hashes_path` parses but `sha256 = []` (empty allowlist)
///   * `passt_path` missing or unreadable
///   * computed SHA256 not present in the allowlist
///
/// All failure messages include the offending path and a concrete
/// remediation hint (the exact `sha256sum` command to pin the
/// in-use binary).
#[cfg(target_os = "linux")]
fn verify_passt_hash(passt_path: &Path, hashes_path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(hashes_path).with_context(|| {
        format!(
            "read passt-hashes allowlist at {} (operator must pre-populate \
             this file with `sha256 = [\"<sha256sum {}>\", ...]` before \
             starting the bridge)",
            hashes_path.display(),
            passt_path.display(),
        )
    })?;
    let parsed: PasstHashesFile = toml::from_str(&raw)
        .with_context(|| format!("parse passt-hashes TOML at {}", hashes_path.display()))?;
    if parsed.sha256.is_empty() {
        return Err(anyhow!(
            "passt-hashes allowlist at {} contains zero SHA256 entries; \
             fail-closed. Populate with `sha256 = [\"$(sha256sum {} | \
             cut -d' ' -f1)\"]`.",
            hashes_path.display(),
            passt_path.display(),
        ));
    }

    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(passt_path)
        .with_context(|| format!("read passt binary at {} for SHA256", passt_path.display()))?;
    let mut buf = [0u8; 8192];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read passt binary at {} for SHA256", passt_path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let computed = hex_encode(&hasher.finalize());

    let mut accepted = false;
    for entry in &parsed.sha256 {
        if entry.eq_ignore_ascii_case(&computed) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        return Err(anyhow!(
            "passt binary {} SHA256 mismatch: computed {}, allowlist {} \
             admits {:?}. Add the computed hash with `sha256 += [\"{}\"]` \
             if this binary is trusted (verify with `sha256sum {}`).",
            passt_path.display(),
            computed,
            hashes_path.display(),
            parsed.sha256,
            computed,
            passt_path.display(),
        ));
    }
    Ok(())
}

/// Lowercase hex encoder. Local instead of pulling `hex` as a dep
/// since this is the only consumer in this crate; matches the
/// `sha256sum` output format byte-for-byte.
#[cfg(target_os = "linux")]
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    // Stderr-only tracing keeps stdout clean for any future protocol
    // (the parent reads stdin only; we are not expected to print to
    // stdout). Same posture as `mvm-libkrun-supervisor` and
    // `mvm-vz-drainer`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "mvm-firecracker-bridge exiting with error");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run() -> Result<()> {
    // ── Step 1: read + parse stdin contract ─────────────────────────
    let mut json = String::new();
    std::io::stdin()
        .read_to_string(&mut json)
        .context("read BridgeConfigJson from stdin")?;
    let cfg: BridgeConfigJson = serde_json::from_str(&json).context("parse BridgeConfigJson")?;

    // ── Step 2: verify passt binary hash BEFORE confinement ─────────
    //
    // Landlock clamps reads to `cfg.passt_path` + `cfg.keys_dir`
    // after `confine_self`; if we ran the hash check after
    // confinement, a misconfigured `passt_hashes_path` would surface
    // as a confusing EACCES instead of "operator forgot to populate
    // the allowlist". Cardoso minimum-viable-policy: the operator-
    // pinned allowlist is the supply-chain gate; this is the right
    // place for it.
    verify_passt_hash(&cfg.passt_path, &cfg.passt_hashes_path)
        .context("verify passt binary hash against operator allowlist")?;

    // ── Step 3: apply mvm-jailer-lite confinement ───────────────────
    //
    // After this call, the process can only:
    //   * read from `cfg.passt_path` + `cfg.keys_dir`
    //   * read/write under `cfg.audit_dir`
    //   * invoke the allowlisted syscalls (see
    //     `mvm_jailer_lite::seccomp::BRIDGE_SYSCALLS`)
    //
    // Per `confine_self`'s partial-confinement contract: any error
    // here MUST cause hard exit. We propagate up to `main()` which
    // turns the error into `ExitCode::FAILURE`; Task 13's watchdog
    // sees the nonzero exit and tears down the VM.
    let spec = ConfinementSpec::firecracker_bridge(
        cfg.audit_dir.clone(),
        cfg.keys_dir.clone(),
        cfg.passt_path.clone(),
    );
    confine_self(&spec).context("apply mvm-jailer-lite confinement")?;

    // ── Step 4: parse trusted plan + bundle ─────────────────────────
    //
    // Trust model (see module doc): the producer (Task 13's
    // `FirecrackerBackend`) has already verified the signed envelope
    // via `mvm-cli::admit_for_run`; we parse the inner ExecutionPlan
    // body directly.
    let plan: ExecutionPlan = serde_json::from_str(&cfg.plan_json)
        .context("decode BridgeConfigJson.plan_json into ExecutionPlan")?;
    let bundle: Option<PolicyBundle> = match &cfg.bundle_json {
        Some(s) => Some(
            serde_json::from_str(s)
                .context("decode BridgeConfigJson.bundle_json into PolicyBundle")?,
        ),
        None => None,
    };

    // ── Step 5: load host signer key + build FileAuditSigner ────────
    //
    // The file is mode 0600 and was written by `mvm-cli::host_signer::
    // load_or_init_at` at admit time. Landlock granted read on
    // `cfg.keys_dir`; this read succeeds inside the ruleset.
    let key_bytes = std::fs::read(&cfg.signing_key_path)
        .with_context(|| format!("read signing key {}", cfg.signing_key_path.display()))?;
    let key_array: [u8; 32] = key_bytes.as_slice().try_into().with_context(|| {
        format!(
            "signing key {} is {} bytes, expected 32",
            cfg.signing_key_path.display(),
            key_bytes.len()
        )
    })?;
    let signing_key = SigningKey::from_bytes(&key_array);
    let file_signer = FileAuditSigner::open(signing_key, &cfg.audit_dir)
        .with_context(|| format!("open FileAuditSigner at {}", cfg.audit_dir.display()))?;
    let signer: Arc<dyn AuditSigner> = Arc::new(file_signer);

    // ── Step 6: resolve observer chain from admitted plan ───────────
    //
    // Plan 113 §Task 4 — observer chain from admitted plan + host
    // allowlist. Firecracker reports `payload_tap: true` (ADR-064
    // §Decision 8) so payload-tap observers admit at the
    // `from_admitted` gate.
    let leaf_caps = ProviderCapabilities {
        flow_events: true,
        payload_tap: true,
    };
    let allowlist = ObserverAllowlist::load_from_host_config()
        .map_err(|e| anyhow!("load ObserverAllowlist from ~/.mvm/observers/allowlist.toml: {e}"))?;
    let observers = from_admitted(&plan, leaf_caps, &allowlist)
        .map_err(|e| anyhow!("resolve observer chain from admitted plan: {e}"))?;

    tracing::info!(
        vm = %cfg.vm_name,
        tenant = %plan.tenant.0,
        audit_socket = %cfg.audit_socket.display(),
        audit_dir = %cfg.audit_dir.display(),
        passt_path = %cfg.passt_path.display(),
        gateway_fd = cfg.gateway_fd_raw,
        supervisor_fd = cfg.supervisor_fd_raw,
        observers = observers.len(),
        "starting mvm-firecracker-bridge; reconstructing socketpair fds"
    );

    // ── Step 7: reconstruct parent-inherited fds + build endpoints ──
    //
    // SAFETY: the caller MUST guarantee that
    //   1. `cfg.gateway_fd_raw` and `cfg.supervisor_fd_raw` name
    //      valid, open file descriptors in this process's fd table,
    //   2. those fds were duped (or socketpair'd) by the parent
    //      before exec and inherited across exec with `O_CLOEXEC`
    //      cleared,
    //   3. no other code in this process holds owning references to
    //      those fds (ownership transfers to the returned `OwnedFd`).
    // Task 13's `FirecrackerBackend` honours this via the
    // `CommandExt::pre_exec` dup2 + fcntl(FD_CLOEXEC clear) path
    // documented in its module header. There is no validation we
    // can perform on the host side (the kernel will EBADF on first
    // use if the fd is bad, which the bridge thread surfaces via
    // `tokio::io::copy_bidirectional` → exit(1)).
    let (gateway_fd, supervisor_fd) = unsafe {
        (
            OwnedFd::from_raw_fd(cfg.gateway_fd_raw),
            OwnedFd::from_raw_fd(cfg.supervisor_fd_raw),
        )
    };

    let bridge_cfg = BridgeConfig {
        vm_name: cfg.vm_name.clone(),
        plan: Arc::new(plan),
        bundle: bundle.map(Arc::new),
        audit_socket: cfg.audit_socket,
        signer,
        policy: Arc::new(AllowAll),
        observers,
    };

    let endpoints = BridgeEndpoints::Passt {
        gateway_fd,
        supervisor_fd,
    };

    // ── Step 8: spawn the bridge thread + park ──────────────────────
    //
    // JoinHandle is intentionally dropped. The parent
    // (`FirecrackerBackend`) holds an `AttachedBridgeGuard` that
    // kills this process on early return / panic / VM teardown; the
    // bridge's own `catch_unwind → exit(1)` is the fail-closed
    // signal for the claim-10 substrate.
    let _join = spawn_bridge_thread(endpoints, bridge_cfg);

    tracing::info!(vm = %cfg.vm_name, "bridge thread spawned; parking main thread");

    // Park indefinitely. The parent kills us via SIGTERM/SIGKILL on
    // VM shutdown; without a `park`, the bin would exit immediately
    // and the OS would reap the bridge thread before the first
    // FlowEvent arrives. A loop guards against spurious unparks.
    loop {
        std::thread::park();
    }
}

/// Non-Linux guard. Print a clear error and exit nonzero. This binary
/// is meaningless off Linux — `mvm-jailer-lite::confine_self` returns
/// `Err(SeccompUnavailable)` on macOS/Windows stubs, and the
/// `BridgeEndpoints::Passt` path requires Linux socketpair semantics
/// the macOS Firecracker port does not support. The cfg-gate keeps
/// workspace builds green on contributor hosts.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "mvm-firecracker-bridge is a Linux-only sidecar; this binary \
         was built for a non-Linux target and refuses to run. The \
         Vz drainer (`mvm-vz-drainer`) is the macOS equivalent for the \
         gateway audit bridge."
    );
    std::process::ExitCode::FAILURE
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tiny passt-like binary fixture under a tempdir and a
    /// matching `passt-hashes.toml`. Returns both paths.
    fn fixture(passt_bytes: &[u8], hashes_toml: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt_path = dir.path().join("passt");
        let hashes_path = dir.path().join("passt-hashes.toml");
        {
            let mut f = std::fs::File::create(&passt_path).expect("create passt fixture");
            f.write_all(passt_bytes).expect("write passt fixture");
        }
        std::fs::write(&hashes_path, hashes_toml).expect("write hashes file");
        (dir, passt_path, hashes_path)
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex_encode(&h.finalize())
    }

    #[test]
    fn verify_passt_hash_accepts_pinned_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let hash = sha256_hex(body);
        let toml = format!("sha256 = [\"{hash}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        verify_passt_hash(&passt, &hashes).expect("hash matches allowlist");
    }

    #[test]
    fn verify_passt_hash_accepts_uppercase_pinned_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let hash = sha256_hex(body).to_uppercase();
        let toml = format!("sha256 = [\"{hash}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        verify_passt_hash(&passt, &hashes)
            .expect("hash compare is case-insensitive (sha256sum output is lowercase by convention but operators sometimes paste uppercase)");
    }

    #[test]
    fn verify_passt_hash_rejects_wrong_hash() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let wrong = "0".repeat(64);
        let toml = format!("sha256 = [\"{wrong}\"]\n");
        let (_dir, passt, hashes) = fixture(body, &toml);
        let err = verify_passt_hash(&passt, &hashes).expect_err("wrong hash must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SHA256 mismatch"),
            "error must name mismatch: {msg}"
        );
        assert!(
            msg.contains(&sha256_hex(body)),
            "error must include computed hash so operator can pin it: {msg}"
        );
        assert!(
            msg.contains(passt.to_str().unwrap()),
            "error must include passt path: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_empty_allowlist() {
        let body = b"#!/bin/sh\necho fake-passt\n";
        let (_dir, passt, hashes) = fixture(body, "sha256 = []\n");
        let err = verify_passt_hash(&passt, &hashes).expect_err("empty allowlist must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero SHA256 entries"),
            "error must explain empty allowlist: {msg}"
        );
        assert!(
            msg.contains("sha256sum"),
            "error must give a remediation hint: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_missing_hashes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt = dir.path().join("passt");
        std::fs::write(&passt, b"binary").expect("write passt");
        let hashes = dir.path().join("does-not-exist.toml");
        let err =
            verify_passt_hash(&passt, &hashes).expect_err("missing hashes file must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read passt-hashes allowlist"),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("pre-populate"),
            "error must give remediation hint: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_missing_passt_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let passt = dir.path().join("nonexistent-passt");
        let hashes = dir.path().join("passt-hashes.toml");
        std::fs::write(&hashes, "sha256 = [\"deadbeef\"]\n").expect("write hashes");
        let err =
            verify_passt_hash(&passt, &hashes).expect_err("missing passt binary must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read passt binary"),
            "error must name the missing binary path: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_malformed_toml() {
        let body = b"binary";
        let (_dir, passt, hashes) = fixture(body, "this is not toml = = =\n");
        let err = verify_passt_hash(&passt, &hashes).expect_err("malformed TOML must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse passt-hashes TOML"),
            "error must name the parse failure: {msg}"
        );
    }

    #[test]
    fn verify_passt_hash_rejects_unknown_field_in_hashes_file() {
        let body = b"binary";
        let toml = "sha256 = [\"deadbeef\"]\nattacker_injected = \"value\"\n";
        let (_dir, passt, hashes) = fixture(body, toml);
        let err = verify_passt_hash(&passt, &hashes)
            .expect_err("unknown TOML field must reject (deny_unknown_fields)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse passt-hashes TOML") || msg.contains("unknown field"),
            "error must name the parse rejection: {msg}"
        );
    }

    #[test]
    fn hex_encode_matches_sha256sum_format() {
        // sha256sum emits lowercase hex, no `0x`, no separators. Our
        // encoder must match byte-for-byte.
        let h = sha256_hex(b"hello");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
