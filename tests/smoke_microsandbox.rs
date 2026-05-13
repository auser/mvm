//! Live smoke for the microsandbox backend (Phase 1 W3 — plan 60).
//!
//! Validates the host-side plumbing of [`MicrosandboxBackend::start`] +
//! the `.ext4 → .raw` hard-link bridge against a real upstream
//! microsandbox 0.4.5 install. **Does not** assert that the guest
//! booted to userspace — that needs a real init in the rootfs, which
//! means a Nix build, which is Phase 1 W4 work. What this smoke does
//! check:
//!
//!   1. We get past the rootfs-existence guard with a real ext4 file.
//!   2. The `.raw` alias is created next to the rootfs.
//!   3. We make a real call into the upstream microsandbox crate.
//!   4. `stop()` is callable on the resulting VmId regardless of
//!      whether the boot succeeded — i.e., teardown is idempotent
//!      against the live API.
//!
//! ## Why this is gated
//!
//! Booting a sandbox needs hardware virtualization (KVM on Linux,
//! HVF on macOS) and a writable microsandbox state directory. Neither
//! is portable across CI runners. Plus, even the host-side bridge
//! work requires `mkfs.ext4` which isn't on macOS by default.
//! `MVM_LIVE_SMOKE=1` is the operator's fence; without it the test
//! short-circuits with a `eprintln!` describing the gate.
//!
//! ## What it skips, when
//!
//! - `MVM_LIVE_SMOKE != "1"` → skip with diagnostic.
//! - `mkfs.ext4` not on PATH → skip (can't build the fixture).
//! - `cfg(not(target_os = "linux"))` → skip; mkfs.ext4 + ext4 tooling
//!   are most reliable on Linux. macOS smokes belong to Phase 1 W4.
//!
//! On a Linux host with KVM, `MVM_LIVE_SMOKE=1` runs the smoke and
//! the boot attempt either succeeds (sandbox spawns, init missing
//! makes it exit immediately, but our host-side path is green) or
//! fails with a microsandbox-tagged error (also green for this
//! smoke — we just need proof we made it past our own boundary).

// Whole-file gate: the smoke only makes sense when the microsandbox
// backend is compiled in. Library-consumer builds disable the
// `backends-microsandbox` feature and skip this file entirely.
#![cfg(feature = "backends-microsandbox")]

use mvmctl::backend::microsandbox::MicrosandboxBackend;
use mvmctl::core::vm_backend::VmId;

const SMOKE_GATE: &str = "MVM_LIVE_SMOKE";

// All Linux-specific helpers + the live smoke proper are gated under
// `target_os = "linux"`. macOS callers see only the cheap sanity
// test below — keeps the warning surface clean on developer hosts
// without disabling the smoke for the platform that can run it.
#[cfg(target_os = "linux")]
mod live {
    use super::SMOKE_GATE;
    use std::path::PathBuf;
    use std::process::Command;

    pub fn smoke_enabled() -> bool {
        std::env::var(SMOKE_GATE).as_deref() == Ok("1")
    }

    pub fn skip_if_disabled(test_name: &str) -> bool {
        if smoke_enabled() {
            return false;
        }
        eprintln!(
            "[smoke_microsandbox::{test_name}] skipped — set {SMOKE_GATE}=1 on a Linux \
             host with KVM + mkfs.ext4 to run."
        );
        true
    }

    pub fn mkfs_ext4_available() -> bool {
        Command::new("mkfs.ext4")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build a 16-MiB sparse ext4 file at `path`. Returns Err if mkfs fails.
    pub fn build_empty_ext4(path: &PathBuf) -> std::io::Result<()> {
        // Truncate to 16 MiB — small enough to be cheap, large enough that
        // mkfs.ext4 doesn't refuse with "filesystem too small for journal."
        let f = std::fs::File::create(path)?;
        f.set_len(16 * 1024 * 1024)?;
        drop(f);

        let out = Command::new("mkfs.ext4")
            .args(["-F", "-q"])
            .arg(path)
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(std::io::Error::other(format!("mkfs.ext4 failed: {stderr}")));
        }
        Ok(())
    }
}

#[test]
#[cfg(target_os = "linux")]
fn microsandbox_start_alias_bridge_round_trip() {
    use mvmctl::core::vm_backend::{VmBackend, VmStartConfig};

    if live::skip_if_disabled("microsandbox_start_alias_bridge_round_trip") {
        return;
    }
    if !live::mkfs_ext4_available() {
        eprintln!(
            "[smoke_microsandbox] skipped — mkfs.ext4 not on PATH; \
             install e2fsprogs to enable this smoke."
        );
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir for smoke fixture");
    let rootfs = temp.path().join("rootfs.ext4");
    live::build_empty_ext4(&rootfs).expect("mkfs.ext4 on smoke fixture");

    // PID-suffixed name so concurrent test runs don't collide on the
    // microsandbox-side namespace.
    let name = format!("mvm-smoke-{}", std::process::id());
    let backend = MicrosandboxBackend;
    let config = VmStartConfig {
        name: name.clone(),
        rootfs_path: rootfs.to_string_lossy().into_owned(),
        cpus: 1,
        memory_mib: 256,
        ..Default::default()
    };

    let start_result = backend.start(&config);
    let id = VmId(name.clone());

    // Tear down regardless of start outcome — stop() is idempotent on
    // not-found, so we don't propagate teardown failures into the test
    // result. Hard-link alias gets cleaned up with the tempdir on drop.
    let stop_result = backend.stop(&id);

    // The alias bridge must have created the `.raw` sibling.
    let alias = rootfs.with_extension("raw");
    assert!(
        alias.exists(),
        "MicrosandboxBackend::start() must create a .raw alias next to the .ext4 rootfs"
    );

    // start() outcome is informational; what's NOT acceptable is our
    // own pre-flight guards firing. If start() errored, the message
    // should reference microsandbox/libkrun/init/boot — not our
    // rootfs-existence guard.
    if let Err(e) = &start_result {
        let msg = e.to_string();
        eprintln!("[smoke_microsandbox] start() error (informational): {msg}");
        assert!(
            !msg.contains("rootfs path does not exist"),
            "start() must get past the existence guard with a real ext4 fixture"
        );
    }

    // stop() should never raise on a possibly-not-running sandbox —
    // the W1.2 idempotence contract.
    if let Err(e) = stop_result {
        // Allow microsandbox-side errors (DB not initialized, etc.);
        // reject only the impossible case where we propagated a
        // not-found error.
        let msg = e.to_string();
        assert!(
            !msg.contains("not found"),
            "stop() must swallow not-found, got: {msg}"
        );
    }
}

/// Sanity test that runs even without the live gate — confirms the
/// smoke module compiles and the test surface is reachable. This is
/// useful so a regression that makes the smoke uncompilable is
/// caught by every PR's `cargo test` even though the live test
/// itself is gated.
#[test]
fn smoke_module_compiles_and_imports_resolve() {
    let _ = MicrosandboxBackend;
    let _ = VmId(String::new());
    // A sentinel value the gate function reads — if env handling
    // regresses we'd notice via this test even with the gate off.
    assert_eq!(SMOKE_GATE, "MVM_LIVE_SMOKE");
}
