//! Cloud Hypervisor backend for mvm.
//!
//! Cloud Hypervisor (CH) is a rust-vmm-based Tier 1 microVM monitor —
//! the same family as Firecracker, with a richer device model. mvm
//! offers it as a peer to Firecracker at the same security tier;
//! the choice between them is workload-shape, not security-shape.
//!
//! ## When to pick Cloud Hypervisor over Firecracker
//!
//! - **VFIO passthrough** (PCI device passthrough, including GPU).
//!   Firecracker explicitly excludes this from its device model;
//!   ADR-013 §"GPU and graphics support" names CH as the path for
//!   compute-GPU workloads (CUDA, ROCm, etc.).
//! - **virtio-gpu** for accelerated graphics in-VM (FC doesn't support).
//! - **Larger guests**: CH's memory + device model handles more vCPUs
//!   and devices than FC's intentionally minimal one.
//! - **virtio-fs** for high-throughput shared filesystems (FC's path
//!   is more limited).
//!
//! Firecracker remains the default for typical mvm workloads — its
//! attack surface is smaller, boot is faster, and the security work
//! (jailer, dm-verity, seccomp tier) targets it. CH is for workloads
//! that need what FC deliberately doesn't have.
//!
//! ## Status
//!
//! This file ships the final `VmBackend` shape: [`CloudHypervisorBackend`]
//! declares its capabilities, security profile, and dispatch through
//! `mvm_runtime::vm::backend::AnyBackend`. Lifecycle methods today
//! return a "not yet wired" error pointing at the bring-up wave —
//! same pattern as `LibkrunBackend` until plan 57's libkrun spike
//! landed real lifecycle. CH bring-up is a focused follow-up wave.
//!
//! Once wired, CH will be selectable via:
//!
//!   `mvmctl run --hypervisor cloud-hypervisor`
//!
//! and the `mkGuest { hypervisor = "cloud-hypervisor"; }` argument.

use anyhow::Result;
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, VmBackend,
    VmCapabilities, VmId, VmInfo, VmStartConfig, VmStatus,
};

/// Cloud Hypervisor backend (rust-vmm; Linux/KVM + macOS/HVF).
///
/// Same security tier as Firecracker (Tier 1, rust-vmm-based,
/// passes the plan 53 "fork test"); richer device model. The choice
/// between CH and FC is workload-shape, not security-shape.
pub struct CloudHypervisorBackend;

const NOT_YET_WIRED: &str = "Cloud Hypervisor backend is not yet wired \
(near-term follow-up wave per plan 60 — ships the cloud-hypervisor \
binary integration + the JSON-API client + the artifact dispatch)";

impl VmBackend for CloudHypervisorBackend {
    fn name(&self) -> &str {
        "cloud-hypervisor"
    }

    fn capabilities(&self) -> VmCapabilities {
        // CH supports more than Firecracker: pause/resume, snapshots,
        // VFIO, virtio-gpu, virtio-fs. We surface the subset that's
        // generic at this level; backend-specific extras (GPU,
        // virtio-fs) are addressed via dedicated fields when the
        // VmStartConfig + VmCapabilities shapes grow them.
        VmCapabilities {
            pause_resume: true,
            snapshots: true,
            vsock: true,
            tap_networking: true,
        }
    }

    fn start(&self, _config: &VmStartConfig) -> Result<VmId> {
        anyhow::bail!(NOT_YET_WIRED)
    }

    fn stop(&self, _id: &VmId) -> Result<()> {
        anyhow::bail!(NOT_YET_WIRED)
    }

    fn stop_all(&self) -> Result<()> {
        // No running VMs in scaffolding — succeed silently so cleanup
        // paths don't mask other errors.
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
        Ok(mvm_core::platform::current().has_cloud_hypervisor())
    }

    fn install(&self) -> Result<()> {
        anyhow::bail!(
            "Cloud Hypervisor must be installed via the host's package \
             manager (apt: cloud-hypervisor; nixpkgs: cloud-hypervisor; \
             cargo: cloud-hypervisor) or downloaded from \
             https://github.com/cloud-hypervisor/cloud-hypervisor/releases. \
             Once installed, mvm detects it on PATH automatically."
        )
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        // CH exposes vsock natively; the guest agent listens on the
        // shared GUEST_AGENT_PORT (same contract as Firecracker /
        // microsandbox / libkrun).
        Ok(GuestChannelInfo::Vsock {
            cid: 3,
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Tier 1 — rust-vmm-based, comparable VMM TCB to Firecracker.
        // Passes the plan 53 §"fork test" (rust-vmm origin, no
        // Firecracker-excluded features in the boot path; the richer
        // device set is opt-in per VM, not always-on).
        //
        // Claim 3 (verified boot) is partial because the W3 dm-verity
        // pipeline currently targets Firecracker; CH support is a
        // follow-up that lands alongside the bring-up wave.
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds,       // 1 — host-fs isolation via KVM/HVF
                ClaimStatus::Holds,       // 2 — uid-0 protections same as FC
                ClaimStatus::DoesNotHold, // 3 — verified boot pending
                ClaimStatus::Holds,       // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds,       // 5 — vsock framing is fuzzed
                ClaimStatus::Holds,       // 6 — image hash verification
                ClaimStatus::Holds,       // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 1",
            notes: &[
                "rust-vmm-based; comparable VMM TCB to Firecracker.",
                "Picks up where Firecracker stops: VFIO, virtio-gpu, \
                 virtio-fs, larger guests.",
                "Claim 3 (verified boot) pending — dm-verity pipeline \
                 targets Firecracker today; CH parity is a follow-up.",
                "Default for workloads that need CH-specific devices; \
                 Firecracker remains the default for typical mvm work.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_hypervisor_backend_name() {
        assert_eq!(CloudHypervisorBackend.name(), "cloud-hypervisor");
    }

    #[test]
    fn cloud_hypervisor_capabilities_richer_than_firecracker() {
        let caps = CloudHypervisorBackend.capabilities();
        assert!(caps.pause_resume, "CH supports pause/resume");
        assert!(caps.snapshots, "CH supports snapshots");
        assert!(caps.vsock, "CH supports vsock");
        assert!(caps.tap_networking, "CH supports TAP networking");
    }

    #[test]
    fn cloud_hypervisor_security_profile_is_tier_1_with_partial_claim_3() {
        let p = CloudHypervisorBackend.security_profile();
        assert_eq!(p.tier, "Tier 1");
        assert!(p.layer_coverage.is_microvm());
        assert_eq!(
            p.dropped_claims(),
            vec![3],
            "claim 3 (verified boot) is the only outstanding gap"
        );
    }

    #[test]
    fn cloud_hypervisor_start_is_not_yet_wired() {
        let config = VmStartConfig {
            name: "ch-test".to_string(),
            rootfs_path: "/tmp/rootfs.ext4".to_string(),
            ..Default::default()
        };
        let err = CloudHypervisorBackend
            .start(&config)
            .expect_err("scaffolding start must error");
        assert!(
            err.to_string().contains("not yet wired"),
            "error must reference the bring-up followup, got: {err}"
        );
    }

    #[test]
    fn cloud_hypervisor_stop_all_is_idempotent() {
        CloudHypervisorBackend
            .stop_all()
            .expect("stop_all is a no-op in scaffolding");
    }

    #[test]
    fn cloud_hypervisor_status_returns_stopped_in_scaffolding() {
        let s = CloudHypervisorBackend
            .status(&VmId("any".to_string()))
            .expect("status is conservative in scaffolding");
        assert_eq!(s, VmStatus::Stopped);
    }

    #[test]
    fn cloud_hypervisor_guest_channel_uses_shared_vsock_port() {
        let info = CloudHypervisorBackend
            .guest_channel_info(&VmId("any".to_string()))
            .expect("guest_channel_info is static in scaffolding");
        let GuestChannelInfo::Vsock { cid, port } = info else {
            panic!("CH must use vsock, not UnixSocket");
        };
        assert_eq!(cid, 3);
        assert_eq!(port, mvm_guest::vsock::GUEST_AGENT_PORT);
    }
}
