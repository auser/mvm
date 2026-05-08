//! microsandbox backend for mvm.
//!
//! Plan 60 ([`specs/plans/60-mvm-microsandbox-migration.md`]) and ADR-013
//! ([`specs/adrs/013-microsandbox-libkrun-microvm-nix-pivot.md`]) name
//! microsandbox as the cross-platform builder + macOS/Windows execution
//! backend. It wraps Red Hat's libkrun under a higher-level Rust API,
//! adding sandbox lifecycle, image management, and a working DX out of
//! the box.
//!
//! # Status
//!
//! Phase 1 W1.1 (this file): trait shape only. `start()`/`stop()` and
//! related lifecycle methods return a `not yet wired` error pointing at
//! Phase 1 W2 when the actual `microsandbox = "0.4.5"` dependency lands
//! and `MicroSandbox::builder(...).create().await` drives boot. The
//! capabilities, security profile, and dispatch through
//! [`AnyBackend`](super::backend::AnyBackend) are final from this wave.
//!
//! Why a separate variant from [`LibkrunBackend`](super::libkrun::LibkrunBackend):
//! microsandbox is a higher-level abstraction (sandboxes, images, network
//! policy) backed by libkrun under the hood. The duplication here mirrors
//! the difference between "raw libkrun" (manual VM lifecycle) and
//! "microsandbox" (managed sandbox lifecycle). Both are Tier 2; both are
//! KVM/HVF-backed; the choice between them is API-shape preference.

use anyhow::Result;
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, VmBackend,
    VmCapabilities, VmId, VmInfo, VmStartConfig, VmStatus,
};

/// microsandbox backend (libkrun-backed; cross-platform Linux/macOS/Windows).
///
/// See module docs for status. Tier 2 microVM isolation via KVM (Linux),
/// Hypervisor.framework (macOS), or WHvPlatform (Windows, when supported
/// by the upstream microsandbox crate).
pub struct MicrosandboxBackend;

/// Sentinel error message — keeps the "not yet wired" copy in one place
/// so when Phase 1 W2 lands, deletion is a single `grep`.
const NOT_YET_WIRED: &str = "microsandbox backend is not yet wired \
(Phase 1 W2 of plan 60 — adds `microsandbox = \"0.4.5\"` dep + boot/teardown)";

impl VmBackend for MicrosandboxBackend {
    fn name(&self) -> &str {
        "microsandbox"
    }

    fn capabilities(&self) -> VmCapabilities {
        // microsandbox via libkrun — same capability matrix as the bare
        // LibkrunBackend. vsock works; pause/resume and snapshots are not
        // exposed by the upstream API today (Phase 7a will revisit when
        // the snapshot model lands).
        VmCapabilities {
            pause_resume: false,
            snapshots: false,
            vsock: true,
            tap_networking: false,
        }
    }

    fn start(&self, _config: &VmStartConfig) -> Result<VmId> {
        anyhow::bail!(NOT_YET_WIRED)
    }

    fn stop(&self, _id: &VmId) -> Result<()> {
        anyhow::bail!(NOT_YET_WIRED)
    }

    fn stop_all(&self) -> Result<()> {
        // No running VMs in scaffolding — succeed silently rather than
        // bail, so calls during cleanup don't mask other errors.
        Ok(())
    }

    fn status(&self, _id: &VmId) -> Result<VmStatus> {
        Ok(VmStatus::Stopped)
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        Ok(Vec::new())
    }

    fn logs(&self, _id: &VmId, _lines: u32, _hypervisor: bool) -> Result<String> {
        anyhow::bail!(NOT_YET_WIRED)
    }

    fn is_available(&self) -> Result<bool> {
        // Until W2 wires the `microsandbox` crate, advertise unavailable
        // so `auto_select()` skips this variant on any host.
        Ok(false)
    }

    fn install(&self) -> Result<()> {
        anyhow::bail!(
            "microsandbox is installed via `cargo install microsandbox-cli` \
             or via the upstream installer; see plan 60 §\"microvm.nix integration\"."
        )
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        // microsandbox exposes vsock the same way bare libkrun does — the
        // guest agent listens on the shared `GUEST_AGENT_PORT`, so all
        // backends share one client implementation.
        Ok(GuestChannelInfo::Vsock {
            cid: 3,
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Tier 2: hardware isolation via KVM/HVF. Same posture as
        // LibkrunBackend (microsandbox sits on top of libkrun) — claim
        // 3 (verified boot via dm-verity) is the gap because the W3
        // pipeline targets Firecracker today; Phase 6 of plan 60 lands
        // a microsandbox-flavored integrity check (image-hash + HMAC).
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds,       // 1 — host-fs isolation via KVM/HVF
                ClaimStatus::Holds,       // 2 — uid-0 protections same as FC
                ClaimStatus::DoesNotHold, // 3 — verified boot not yet wired
                ClaimStatus::Holds,       // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds,       // 5 — vsock framing is fuzzed
                ClaimStatus::Holds,       // 6 — image hash verification
                ClaimStatus::Holds,       // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 2",
            notes: &[
                "Hardware isolation via KVM (Linux) or Hypervisor.framework (macOS).",
                "Cross-platform: Linux + macOS arm64/x86_64. Windows pending upstream.",
                "Higher-level API than bare libkrun — managed sandbox lifecycle.",
                "Claim 3 (verified boot) is partial — Phase 6 closes via image-hash + HMAC.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microsandbox_backend_name() {
        assert_eq!(MicrosandboxBackend.name(), "microsandbox");
    }

    #[test]
    fn microsandbox_capabilities_match_libkrun_substrate() {
        let caps = MicrosandboxBackend.capabilities();
        assert!(caps.vsock, "vsock must always be available — guest agent contract");
        assert!(!caps.snapshots, "Phase 7a wires snapshots; W1.1 advertises none");
        assert!(!caps.pause_resume, "upstream microsandbox doesn't expose pause/resume");
        assert!(!caps.tap_networking, "L4/L7 proxy in Phase 3 is the network path");
    }

    #[test]
    fn microsandbox_security_profile_is_tier_2_with_partial_claim_3() {
        let profile = MicrosandboxBackend.security_profile();
        assert_eq!(profile.tier, "Tier 2");
        assert!(profile.layer_coverage.is_microvm());
        assert_eq!(
            profile.dropped_claims(),
            vec![3],
            "claim 3 (verified boot) is the only outstanding gap"
        );
        assert!(profile.na_claims().is_empty());
    }

    #[test]
    fn microsandbox_unavailable_in_scaffolding() {
        // Phase 1 W1.1: backend always reports unavailable so
        // auto_select() doesn't pick it. W2 flips this when the
        // upstream crate lands.
        let available = MicrosandboxBackend
            .is_available()
            .expect("is_available never errors in scaffolding");
        assert!(!available, "scaffolding must advertise unavailable");
    }

    #[test]
    fn microsandbox_start_is_not_yet_wired() {
        let config = VmStartConfig {
            name: "ms-test".to_string(),
            rootfs_path: "/tmp/rootfs.ext4".to_string(),
            ..Default::default()
        };
        let err = MicrosandboxBackend
            .start(&config)
            .expect_err("scaffolding start must error");
        assert!(
            err.to_string().contains("not yet wired"),
            "error must reference the W2 followup, got: {err}"
        );
    }

    #[test]
    fn microsandbox_stop_all_is_idempotent_no_op() {
        // stop_all is the one method that succeeds in scaffolding —
        // cleanup paths shouldn't propagate failures from a backend that
        // has nothing to clean up.
        MicrosandboxBackend
            .stop_all()
            .expect("stop_all is a no-op in scaffolding");
    }

    #[test]
    fn microsandbox_status_returns_stopped() {
        let status = MicrosandboxBackend
            .status(&VmId("any-vm".to_string()))
            .expect("status is conservative in scaffolding");
        assert_eq!(status, VmStatus::Stopped);
    }

    #[test]
    fn microsandbox_list_returns_empty() {
        let vms = MicrosandboxBackend
            .list()
            .expect("list never errors in scaffolding");
        assert!(vms.is_empty());
    }

    #[test]
    fn microsandbox_guest_channel_uses_shared_vsock_port() {
        // All backends share the same guest-agent vsock port — that's
        // the contract that lets a single vsock client speak to any
        // backend. Asserting it explicitly here so a future regression
        // (e.g., someone hardcodes a different port or switches to
        // UnixSocket) trips this test.
        let info = MicrosandboxBackend
            .guest_channel_info(&VmId("any-vm".to_string()))
            .expect("guest_channel_info is static in scaffolding");
        let GuestChannelInfo::Vsock { cid, port } = info else {
            panic!("microsandbox must use vsock, not UnixSocket");
        };
        assert_eq!(cid, 3, "guest CID is 3 for all backends");
        assert_eq!(
            port,
            mvm_guest::vsock::GUEST_AGENT_PORT,
            "all backends use the shared guest-agent port"
        );
    }
}
