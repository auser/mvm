//! libkrun backend for mvm.
//!
//! Plan 53 §"Plan E" / Sprint 48: scaffolding for a Tier 2 microVM
//! backend that runs on Linux KVM, macOS Apple Silicon, and macOS Intel
//! (the only VMM in mvm's tree that covers all three). The lifecycle
//! delegates to [`mvm_libkrun`], which in turn wraps the Red Hat libkrun
//! C library.
//!
//! # Status
//!
//! This file provides the final `VmBackend` shape: [`LibkrunBackend`]
//! declares its capabilities, security profile, and dispatch through
//! `mvm::vm::backend::AnyBackend`. `start()` and `stop()`
//! delegate to `mvm_libkrun`, which today returns a "not yet wired"
//! error pointing at the Plan E spike phase. Once the spike confirms
//! kernel + vsock + entitlement compatibility, the lifecycle will be
//! real with no caller-side changes.

use anyhow::Result;
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, StartMode, VmBackend,
    VmCapabilities, VmId, VmInfo, VmStartConfig, VmStatus,
};

use mvm_base::ui;

/// libkrun backend (Linux KVM / macOS Hypervisor.framework).
pub struct LibkrunBackend;

impl VmBackend for LibkrunBackend {
    fn name(&self) -> &str {
        "libkrun"
    }

    fn capabilities(&self) -> VmCapabilities {
        // libkrun does not support memory snapshots (same trade as
        // Apple Container) — vsock and TAP are available; pause/resume
        // is theoretically possible but not exposed by libkrun's public
        // C API today.
        VmCapabilities {
            pause_resume: false,
            snapshots: false,
            vsock: true,
            tap_networking: false,
        }
    }

    fn start(&self, config: &VmStartConfig) -> Result<VmId> {
        if !mvm_providers::libkrun::is_available() {
            anyhow::bail!(
                "libkrun is not installed on this host.\n  {}",
                mvm_providers::libkrun::install_hint()
            );
        }

        let kernel = config
            .kernel_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("libkrun backend requires a kernel path"))?;

        let ctx =
            mvm_providers::libkrun::KrunContext::new(&config.name, kernel, &config.rootfs_path)
                .with_resources(
                    u8::try_from(config.cpus.clamp(1, u32::from(u8::MAX))).unwrap_or(u8::MAX),
                    config.memory_mib,
                )
                .add_vsock_port(mvm_guest::vsock::GUEST_AGENT_PORT);

        // W6.2.1: thread the build-time sidecar into per-VM runtime
        // metadata so `mvmctl console` enforces the accessible/sealed
        // gate on libkrun-launched VMs the same way as on the
        // microsandbox/Firecracker paths.
        let rootfs = std::path::Path::new(&config.rootfs_path);
        mvm_base::runtime_meta::record_from_rootfs(&config.name, StartMode::Detached, rootfs)?;

        ui::info(&format!(
            "Starting libkrun VM '{}' (cpus={}, mem={}MiB)...",
            config.name, ctx.vcpus, ctx.ram_mib
        ));

        mvm_providers::libkrun::start(&ctx).map_err(|e| anyhow::anyhow!("libkrun start: {e}"))?;
        ui::success(&format!("libkrun VM '{}' started.", config.name));
        Ok(VmId(config.name.clone()))
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        mvm_providers::libkrun::stop(&id.0).map_err(|e| anyhow::anyhow!("libkrun stop: {e}"))
    }

    fn stop_all(&self) -> Result<()> {
        // Until the Plan E spike lands a real registry of running
        // libkrun VMs, stop_all is a no-op. Real implementation tracks
        // VMs in `~/.mvm/vms/<name>/libkrun.pid` (parallel to
        // Firecracker's pidfile convention).
        Ok(())
    }

    fn pause(&self, _id: &VmId) -> Result<()> {
        // libkrun's C API does not expose vCPU pause/resume today;
        // matches `capabilities().pause_resume == false`. When upstream
        // surfaces it (or we wire HVF/KVM ioctl access ourselves), flip
        // the capability flag and replace this bail with a real impl.
        anyhow::bail!(
            "pause is not supported by the libkrun backend (upstream C API does not expose vCPU pause)"
        )
    }

    fn resume(&self, _id: &VmId) -> Result<()> {
        anyhow::bail!(
            "resume is not supported by the libkrun backend (upstream C API does not expose vCPU pause)"
        )
    }

    fn status(&self, _id: &VmId) -> Result<VmStatus> {
        // No real lifecycle yet — assume stopped.
        Ok(VmStatus::Stopped)
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        Ok(Vec::new())
    }

    fn logs(&self, id: &VmId, _lines: u32, _hypervisor: bool) -> Result<String> {
        anyhow::bail!("libkrun logs not yet implemented for VM '{}'", id.0)
    }

    fn is_available(&self) -> Result<bool> {
        Ok(mvm_providers::libkrun::is_available())
    }

    fn install(&self) -> Result<()> {
        ui::info(&format!(
            "libkrun must be installed via the host's package manager.\n  {}",
            mvm_providers::libkrun::install_hint()
        ));
        Ok(())
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        // libkrun exposes vsock as a host-side abstract socket; the
        // guest agent listens on the shared `GUEST_AGENT_PORT` port,
        // identical to Firecracker and Apple Container, so callers can
        // share the same vsock client implementation across backends.
        Ok(GuestChannelInfo::Vsock {
            cid: 3, // standard guest CID
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Tier 2: hardware isolation via KVM (Linux) or Hypervisor.framework
        // (macOS). Comparable VMM TCB to Firecracker — libkrun is rust-vmm
        // based, ~80K LOC, no Firecracker-excluded features (so it passes
        // the plan 53 §"fork test"). Claim 3 (verified boot) is partial
        // because the W3 dm-verity pipeline currently targets Firecracker;
        // libkrun support is a follow-up.
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds,       // 1 — host-fs isolation via KVM/HVF
                ClaimStatus::Holds,       // 2 — uid-0 protections same as FC
                ClaimStatus::DoesNotHold, // 3 — verified boot for libkrun rootfs not yet wired
                ClaimStatus::Holds,       // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds,       // 5 — vsock framing is fuzzed
                ClaimStatus::Holds,       // 6 — image hash verification
                ClaimStatus::Holds,       // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 2",
            notes: &[
                "Hardware isolation via KVM (Linux) or Hypervisor.framework (macOS).",
                "Comparable VMM TCB to Firecracker; passes plan 53 §\"fork test\".",
                "Claim 3 (verified boot) is partial — dm-verity pipeline targets Firecracker today.",
                "Runs on macOS Intel where Apple Container is unavailable.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libkrun_backend_name() {
        assert_eq!(LibkrunBackend.name(), "libkrun");
    }

    #[test]
    fn libkrun_capabilities() {
        let caps = LibkrunBackend.capabilities();
        assert!(caps.vsock);
        assert!(!caps.snapshots);
        assert!(!caps.pause_resume);
        assert!(!caps.tap_networking);
    }

    #[test]
    fn libkrun_security_profile_is_tier_2_with_partial_claim_3() {
        let profile = LibkrunBackend.security_profile();
        assert_eq!(profile.tier, "Tier 2");
        assert!(profile.layer_coverage.is_microvm());
        // Claim 3 (verified boot) is the only partial claim; everything
        // else holds because libkrun matches Firecracker's L2 properties.
        assert_eq!(profile.dropped_claims(), vec![3]);
        assert!(profile.na_claims().is_empty());
    }

    #[test]
    fn libkrun_install_message_is_actionable() {
        // The install() method is informational on every host — it shells
        // out to ui::info instead of attempting to install. The check is
        // just that we can call it without panicking; the actual hint
        // copy lives in mvm_providers::libkrun::install_hint().
        LibkrunBackend.install().expect("install hint never errors");
        let hint = mvm_providers::libkrun::install_hint();
        assert!(!hint.is_empty());
    }

    #[test]
    fn libkrun_start_errors_when_kernel_path_missing() {
        let config = VmStartConfig {
            name: "libkrun-test".to_string(),
            rootfs_path: "/tmp/rootfs.ext4".to_string(),
            ..Default::default()
        };
        // No kernel_path set → start should fail with a precise message
        // before attempting to call into mvm_libkrun.
        let err = LibkrunBackend
            .start(&config)
            .expect_err("expected failure without kernel_path");
        let msg = err.to_string();
        assert!(
            msg.contains("kernel path") || msg.contains("not installed"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn libkrun_status_returns_stopped_in_scaffolding() {
        let status = LibkrunBackend
            .status(&VmId("any-vm".to_string()))
            .expect("status should not error");
        assert_eq!(status, VmStatus::Stopped);
    }

    #[test]
    fn libkrun_list_returns_empty_in_scaffolding() {
        let vms = LibkrunBackend.list().expect("list should not error");
        assert!(vms.is_empty());
    }
}
