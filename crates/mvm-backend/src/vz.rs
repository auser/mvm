//! Vz (Apple Virtualization.framework) backend for mvm.
//!
//! Plan 97 / ADR-056. Tier 2 microVM backend for macOS 13+ that runs
//! the workload directly on the host (no nested Firecracker, no
//! libkrun in the path). Lifecycle delegates to a per-VM
//! `mvm-vz-supervisor` Swift subprocess (lives in
//! `crates/mvm-vz-supervisor/`) — same one-process-per-VM contract
//! `LibkrunBackend` uses, swapped underneath.
//!
//! ## Status — Phase B skeleton
//!
//! This is the first VzBackend slice: trait wiring, capabilities,
//! availability probe, security profile, install message. The
//! lifecycle methods (`start`, `stop`, `status`, `list`, `logs`,
//! `stop_all`) return a clear "not yet wired" error so the backend
//! shows up in `auto_select`/`from_hypervisor` and `mvmctl doctor`
//! without pretending to drive workloads. The supervisor-spawn path
//! lands in a follow-up Phase B slice (Plan 97 checklist item
//! "Phase B acceptance").
//!
//! ## Why opt-in only
//!
//! Per Plan 97 §"Phase D" and the user constraint: `auto_select()`
//! stays unchanged on macOS — libkrun remains the macOS default,
//! Firecracker remains the Linux default. Vz is selected only via
//! `MVM_BACKEND=vz` or `--backend vz` (the `from_hypervisor("vz")`
//! path).

use anyhow::{Result, bail};
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, VmBackend,
    VmCapabilities, VmId, VmInfo, VmStartConfig, VmStatus,
};

use mvm_base::ui;

/// Apple Virtualization.framework backend.
///
/// Direct host-level Vz integration; no nested KVM, no libkrun shim.
/// Only available on macOS 13+ (`Platform::has_vz()`). On Linux this
/// type still compiles, but `is_available()` always returns `Ok(false)`
/// and every lifecycle call short-circuits.
pub struct VzBackend;

/// Sentinel error message used by the not-yet-wired lifecycle methods.
/// Pulled into a constant so tests can match on its prefix and so the
/// follow-up "wire `start`" PR sees one place to delete.
const NOT_YET_WIRED: &str = "VzBackend lifecycle methods are wired in a Plan 97 Phase B follow-up — \
     supervisor-spawn path lands separately; this slice ships the trait \
     surface so `auto_select` / `from_hypervisor` / `mvmctl doctor` can \
     reason about Vz availability";

impl VmBackend for VzBackend {
    fn name(&self) -> &str {
        "vz"
    }

    fn capabilities(&self) -> VmCapabilities {
        // Plan 97 §"What Vz can and can't do".
        //
        // - pause_resume: Vz exposes `VZVirtualMachine.pause(completionHandler:)`
        //   / `.resume(completionHandler:)`; both ship in macOS 12+.
        // - snapshots: macOS 14+ via `saveMachineStateTo` /
        //   `restoreMachineStateFrom`. Feature-detect at runtime —
        //   reported as the *backend* capability rather than the live
        //   host's, so callers downgrade gracefully when the host is
        //   below 14.
        // - vsock: always available.
        // - tap_networking: no — Vz uses file-handle attachments via
        //   gvproxy (ADR-055), not OS-level TAP devices.
        // - balloon: `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`
        //   is wired in the Phase A Swift supervisor.
        VmCapabilities {
            // Vz exposes pause/resume since macOS 12 — but the
            // VmBackend contract (vm_backend.rs:599-602) requires
            // capability and behavior to stay in sync, and this slice
            // doesn't yet wire the supervisor's pause/resume verbs.
            // Flip to `true` in the slice that lands the actual calls.
            pause_resume: false,
            snapshots: macos_supports_vz_snapshots(),
            vsock: true,
            tap_networking: false,
            balloon: true,
        }
    }

    fn start(&self, _config: &VmStartConfig) -> Result<VmId> {
        bail!("{NOT_YET_WIRED}");
    }

    fn stop(&self, _id: &VmId) -> Result<()> {
        bail!("{NOT_YET_WIRED}");
    }

    fn pause(&self, _id: &VmId) -> Result<()> {
        bail!("{NOT_YET_WIRED}");
    }

    fn resume(&self, _id: &VmId) -> Result<()> {
        bail!("{NOT_YET_WIRED}");
    }

    fn stop_all(&self) -> Result<()> {
        // Honest answer: no VM ever started under this skeleton, so
        // there is nothing to stop. Return `Ok` rather than `bail` so
        // `mvmctl up` cleanup paths (which call `stop_all` on every
        // backend they touched) don't fail.
        Ok(())
    }

    fn status(&self, _id: &VmId) -> Result<VmStatus> {
        bail!("{NOT_YET_WIRED}");
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        // Mirror `stop_all`: nothing is running under this backend
        // yet, so the honest answer is an empty list. Lets
        // `mvmctl list` succeed when the user has Vz selected.
        Ok(Vec::new())
    }

    fn logs(&self, _id: &VmId, _lines: u32, _hypervisor: bool) -> Result<String> {
        bail!("{NOT_YET_WIRED}");
    }

    fn is_available(&self) -> Result<bool> {
        Ok(mvm_core::platform::current().has_vz())
    }

    fn install(&self) -> Result<()> {
        // Vz the framework is system-provided — there is nothing to
        // install at the framework layer. The supervisor *binary*
        // does need a build + ad-hoc codesign (Plan 97 Phase A,
        // `crates/mvm-vz-supervisor/tools/build.sh`); surface that
        // here so operators don't hit a mid-`mvmctl up` failure.
        ui::info("Apple Virtualization.framework is built into macOS 13+; no host install needed.");
        ui::info(
            "`mvm-vz-supervisor` binary path: ~/.mvm/bin/mvm-vz-supervisor-<mvmctl-version> \
             (or, on source-checkout builds, \
             crates/mvm-vz-supervisor/.build/<arch>-apple-macosx/<config>/mvm-vz-supervisor — \
             produced by `crates/mvm-vz-supervisor/tools/build.sh`).",
        );
        Ok(())
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        // Plan 97 §"Guest communication is still vsock — nothing
        // changes". CID 3 / `GUEST_AGENT_PORT` are hypervisor-agnostic
        // — same answer as libkrun and Apple Container, so callers
        // share their vsock client implementation across backends.
        Ok(GuestChannelInfo::Vsock {
            cid: 3,
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Plan 97 §"Can we still make all nine ADR-002 security
        // claims?". The 7-claim table here covers claims 1–7 (claims
        // 8 and 9 live outside `BackendSecurityProfile`).
        //
        // Claim 3 (verified boot) mirrors `LibkrunBackend`'s
        // `DoesNotHold` status: the W3 dm-verity artifact pipeline
        // currently targets Firecracker; surfacing it as a partial
        // claim until the Vz path is wired keeps `mvmctl doctor`
        // honest about what's in CI today.
        //
        // Claim 5 (vsock + supervisor JSON fuzzed): the guest vsock
        // framing fuzz target (`crates/mvm-guest/fuzz/`) covers Vz
        // identically. The Rust↔Swift `SupervisorConfig` corpus
        // equivalence test is still a Plan 97 Phase A follow-up;
        // surfaced in `notes` rather than dropping the claim.
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds, // 1 — host-fs isolation via Vz; supervisor refuses non-admitted shares
                ClaimStatus::Holds, // 2 — uid-0 protections same as FC (guest-side)
                ClaimStatus::DoesNotHold, // 3 — verified-boot pipeline targets FC today
                ClaimStatus::Holds, // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds, // 5 — vsock framing fuzzed (Swift JSON corpus equivalence still pending)
                ClaimStatus::Holds, // 6 — dev image hash verified
                ClaimStatus::Holds, // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 2",
            notes: &[
                "Hardware isolation via Apple Virtualization.framework on macOS 13+.",
                "Same Hypervisor.framework primitive libkrun uses; Apple-controlled VMM surface.",
                "Claim 3 (verified boot) partial — dm-verity pipeline targets Firecracker today.",
                "Claim 5 (supervisor JSON corpus equivalence) is a Plan 97 Phase A follow-up; vsock framing fuzz already in CI.",
                "Snapshot save/restore feature-detected at runtime (macOS 14+); reported in `VmCapabilities::snapshots`.",
            ],
        }
    }
}

// ─── helpers ───────────────────────────────────────────────────────

/// Whether the running host supports Vz snapshot save/restore. Probes
/// the macOS major version — Plan 97 Phase E gates the snapshot wiring
/// on macOS 14 (`saveMachineStateTo` / `restoreMachineStateFrom`).
///
/// On non-macOS hosts we return `false` unconditionally — the backend
/// is unavailable there anyway (see `Platform::has_vz`), but this
/// keeps capability reporting honest if a non-macOS caller inspects it.
fn macos_supports_vz_snapshots() -> bool {
    if !matches!(
        mvm_core::platform::current(),
        mvm_core::platform::Platform::MacOS
    ) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        macos_major_version() >= 14
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn macos_major_version() -> u32 {
    use std::process::Command;
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|v| v.trim().split('.').next().map(String::from))
        .and_then(|major| major.parse::<u32>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_vz() {
        assert_eq!(VzBackend.name(), "vz");
    }

    #[test]
    fn capabilities_match_plan_97() {
        let caps = VzBackend.capabilities();
        assert!(caps.vsock, "vsock always available");
        assert!(
            caps.balloon,
            "VZVirtioTraditionalMemoryBalloon shipped from day one"
        );
        assert!(
            !caps.tap_networking,
            "Vz uses file-handle attachments via gvproxy"
        );
        // `pause_resume` is `false` until the supervisor wiring lands —
        // see Plan 97 Phase B follow-up. Vz the framework supports it.
        assert!(!caps.pause_resume, "stubbed false until wiring slice");
        // snapshots is a runtime feature-detect; on contributor hosts
        // below macOS 14 it's false. We only assert the property
        // (boolean — never panics) rather than a fixed value that
        // would diverge across the CI matrix.
        let _ = caps.snapshots;
    }

    #[test]
    fn lifecycle_methods_bail_with_known_message() {
        let backend = VzBackend;
        let id = VmId("smoke".into());
        let err = backend.stop(&id).expect_err("stop should bail");
        assert!(
            err.to_string().contains("Plan 97 Phase B"),
            "error mentions the follow-up: {err}"
        );
        // `start` is the most-likely caller path — keep an assertion
        // there even though all stubs share the same constant.
        let cfg = VmStartConfig::default();
        let err = backend.start(&cfg).expect_err("start should bail");
        assert!(err.to_string().contains("Plan 97 Phase B"));
    }

    #[test]
    fn stop_all_and_list_are_ok() {
        // Honest empty answers for the not-yet-implemented backend.
        // `mvmctl up` cleanup + `mvmctl list` rely on these.
        assert!(VzBackend.stop_all().is_ok());
        let list = VzBackend.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn guest_channel_info_is_vsock_at_agent_port() {
        let info = VzBackend.guest_channel_info(&VmId("smoke".into())).unwrap();
        match info {
            GuestChannelInfo::Vsock { cid, port } => {
                assert_eq!(cid, 3);
                assert_eq!(port, mvm_guest::vsock::GUEST_AGENT_PORT);
            }
            other => panic!("expected vsock channel, got {other:?}"),
        }
    }

    #[test]
    fn security_profile_is_tier_2_with_claim_3_partial() {
        let profile = VzBackend.security_profile();
        assert_eq!(profile.tier, "Tier 2");
        assert!(profile.layer_coverage.is_microvm());
        // Claim 3 (verified boot) is the one partial claim — mirrors
        // libkrun's posture today.
        assert_eq!(profile.dropped_claims(), vec![3]);
    }
}
