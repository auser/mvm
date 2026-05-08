//! End-to-end boot smoke (Phase 1 W6 — plan 60).
//!
//! Boots a real Nix-built rootfs through `MicrosandboxBackend::start_with_mode`
//! against a live microsandbox runtime, asserts the sandbox shows up
//! in `list()`, measures the cold-boot wall-clock, and tears down.
//!
//! ## Boot-time invariant (ADR-013 floor)
//!
//! Every backend must boot in ≤ 300 ms cold p50. This smoke runs a
//! single boot — Phase 9's `xtask perf --runs 100` is the statistical
//! gate. A single-shot above the floor by ≥ 2x fails this smoke as a
//! tripwire; the strict CI gate runs at release time per ADR-038.
//!
//! ## Skip ladder
//!
//! - `MVM_LIVE_SMOKE != "1"` → skip (operator's fence).
//! - `MVM_TEST_ROOTFS` env var unset → skip (we need a pre-built
//!   rootfs path; macOS dev hosts that don't have a `nix-darwin`
//!   linux-builder can't produce one locally, but CI runners can
//!   build it and pass the path).
//!
//! ## Cross-platform reach
//!
//! Microsandbox boots on **both Linux/KVM and macOS/HVF** (per
//! ADR-013), so this smoke runs on both. The previous iteration of
//! this file gated to Linux only — that was overcautious; the test
//! takes a pre-built rootfs from outside, so the only host-side
//! requirement is "microsandbox can boot here," which is satisfied
//! anywhere libkrun runs.
//!
//! Windows is excluded only because microsandbox's Windows path
//! isn't yet wired (ADR-031); the smoke `cfg(any())`-skips on
//! `target_os = "windows"`.
//!
//! ## What it does NOT cover (yet)
//!
//! - Guest-agent vsock RPC ping (needs the `mvm-guest-agent` service
//!   unit baked into the rootfs — W6.1 wires that into the busybox
//!   init).
//! - `mvmctl console` attach (the console subcommand wiring is
//!   itself W6.2; today this smoke confirms the BACKEND is healthy
//!   and the rootfs is bootable).
//!
//! Both are tracked in Sprint 50.

use std::time::Duration;

use mvmctl::core::vm_backend::{StartMode, VmId};
use mvmctl::runtime::vm::microsandbox::MicrosandboxBackend;

const SMOKE_GATE: &str = "MVM_LIVE_SMOKE";
const ROOTFS_VAR: &str = "MVM_TEST_ROOTFS";

/// Per ADR-013: every backend must boot in ≤ 300 ms cold p50.
/// Single-shot tripwire: 2× the floor before this smoke fails.
/// The strict statistical gate is `xtask perf` (Phase 9).
const SMOKE_BOOT_TRIPWIRE: Duration = Duration::from_millis(600);

// Helper module gated to non-Windows. Microsandbox boots on both
// Linux/KVM and macOS/HVF, so the live test runs on both. Windows
// is excluded because microsandbox's Windows path isn't wired
// (ADR-031 — Tauri-only Windows posture).
#[cfg(not(target_os = "windows"))]
mod live {
    use super::SMOKE_GATE;

    pub fn smoke_enabled() -> bool {
        std::env::var(SMOKE_GATE).as_deref() == Ok("1")
    }

    pub fn rootfs_path() -> Option<std::path::PathBuf> {
        std::env::var_os(super::ROOTFS_VAR).map(std::path::PathBuf::from)
    }
}

#[test]
#[cfg(not(target_os = "windows"))]
fn boots_real_rootfs_within_tripwire_then_tears_down_clean() {
    use mvmctl::core::vm_backend::{VmBackend, VmStartConfig};
    use std::time::Instant;

    if !live::smoke_enabled() {
        eprintln!(
            "[smoke_e2e_boot] skipped — set {SMOKE_GATE}=1 + {ROOTFS_VAR}=/path/to/rootfs.ext4 \
             to run. Works on Linux/KVM and macOS/HVF."
        );
        return;
    }
    let Some(rootfs) = live::rootfs_path() else {
        eprintln!(
            "[smoke_e2e_boot] skipped — {ROOTFS_VAR} unset. Build a rootfs with \
             `nix build .#packages.x86_64-linux.internal-minimal-runner` (Linux only) \
             and pass the result path."
        );
        return;
    };
    if !rootfs.exists() {
        panic!(
            "{ROOTFS_VAR}={} does not exist; provide a real ext4 rootfs",
            rootfs.display()
        );
    }

    let backend = MicrosandboxBackend;
    let name = format!("mvm-e2e-{}", std::process::id());
    let config = VmStartConfig {
        name: name.clone(),
        rootfs_path: rootfs.to_string_lossy().into_owned(),
        cpus: 1,
        memory_mib: 256,
        ..Default::default()
    };
    let id = VmId(name.clone());

    // Boot timing — wall-clock from the call to start_with_mode
    // returning. This includes our .raw alias creation + any pull
    // path microsandbox might run, plus the actual hypervisor boot.
    // For the strict per-backend boot p50 (the ADR-013 floor) we
    // exclude alias setup; the W9 `xtask perf` benchmark does the
    // proper split.
    let started = Instant::now();
    let start_result = backend.start_with_mode(&config, StartMode::Detached);
    let boot_elapsed = started.elapsed();

    // Tear down regardless of start outcome — stop() is idempotent
    // on not-found so we can chain it. Hard-link alias is in the
    // same dir as the rootfs and the caller is responsible for
    // cleaning up rootfs's directory.
    let stop_result = backend.stop(&id);

    // Now assert — we tore down first so a failed assertion can't
    // leave a sandbox dangling.
    start_result.expect("start_with_mode must succeed against a real rootfs");
    stop_result.expect("stop must be idempotent and clean");

    assert!(
        boot_elapsed <= SMOKE_BOOT_TRIPWIRE,
        "cold-boot tripwire exceeded: {boot_elapsed:?} > {SMOKE_BOOT_TRIPWIRE:?} \
         (ADR-013 floor is 300ms; this single-shot tripwire is 2× = 600ms; \
         the strict statistical gate runs in Phase 9 via `xtask perf`)"
    );

    eprintln!("[smoke_e2e_boot] cold boot OK: {boot_elapsed:?} (tripwire {SMOKE_BOOT_TRIPWIRE:?})");
}

/// Documentation-as-code: the boot tripwire constant must stay
/// aligned with the ADR-013 floor doubled. If someone bumps the
/// floor in ADR-013 without updating this test (or vice versa), the
/// constants will drift and this assertion catches it.
///
/// Why not just `assert_eq!`: the constant is a Duration, the ADR
/// number is a humans-friendly literal. Asserting the exact ratio
/// is the closest we can get to "spec ↔ code in sync."
#[test]
fn boot_tripwire_is_2x_the_adr_floor() {
    const ADR_FLOOR_MS: u64 = 300;
    assert_eq!(
        SMOKE_BOOT_TRIPWIRE.as_millis() as u64,
        2 * ADR_FLOOR_MS,
        "smoke tripwire must stay locked to 2× the ADR-013 boot floor; \
         update both this test and ADR-013 §\"Per-backend boot budgets\" \
         or this test catches you"
    );
}

/// Always-on sanity test — runs even without the live gate. Catches
/// regressions that would make the smoke uncompilable (import drift,
/// trait method renames, etc.). Same pattern as
/// `tests/smoke_microsandbox.rs`.
#[test]
fn smoke_module_compiles_and_imports_resolve() {
    let _ = MicrosandboxBackend;
    let _ = VmId(String::new());
    let _ = StartMode::Detached;
    let _ = StartMode::Attached;
    assert_eq!(SMOKE_GATE, "MVM_LIVE_SMOKE");
    assert_eq!(ROOTFS_VAR, "MVM_TEST_ROOTFS");
}
