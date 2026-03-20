//! Apple Container backend for macOS 26+.
//!
//! Uses Apple's Containerization framework to run Linux containers in
//! lightweight VMs with sub-second startup. Each container gets its own
//! VM with dedicated networking (vmnet) and vsock for guest communication.
//!
//! The actual Containerization framework calls are behind a Swift FFI bridge
//! (`mvm-apple-container` crate). This module provides the `VmBackend`
//! implementation that translates `VmStartConfig` into container operations.
//!
//! # Platform Requirements
//!
//! - macOS 26+ on Apple Silicon
//! - Containerization framework available via Xcode 26+
//!
//! # Architecture
//!
//! ```text
//! AppleContainerBackend (this module)
//!   └── swift-bridge FFI (future: mvm-apple-container crate)
//!         └── Containerization.framework
//!               ├── ContainerManager (lifecycle)
//!               ├── LinuxContainer (per-VM)
//!               └── vminitd (PID 1, gRPC over vsock:1024)
//! ```

use anyhow::Result;
use mvm_core::vm_backend::{
    GuestChannelInfo, VmBackend, VmCapabilities, VmId, VmInfo, VmNetworkInfo, VmStartConfig,
    VmStatus,
};

use crate::ui;

/// Apple Container backend using macOS Containerization framework.
///
/// Currently a stub implementation — the Swift FFI bridge will be
/// connected when macOS 26 is available. All lifecycle methods return
/// appropriate errors until the bridge is wired up.
pub struct AppleContainerBackend;

impl AppleContainerBackend {
    /// Check whether the Apple Containerization framework is available
    /// at runtime (macOS 26+ on Apple Silicon).
    pub fn is_platform_available() -> bool {
        mvm_core::platform::current().has_apple_containers()
    }
}

impl VmBackend for AppleContainerBackend {
    fn name(&self) -> &str {
        "apple-container"
    }

    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            pause_resume: false,
            snapshots: false,
            vsock: true,
            tap_networking: false,
        }
    }

    fn start(&self, config: &VmStartConfig) -> Result<VmId> {
        if !Self::is_platform_available() {
            anyhow::bail!(
                "Apple Containers require macOS 26+ on Apple Silicon.\n\
                 Use '--hypervisor firecracker' or run on a supported platform."
            );
        }

        // TODO: Wire to swift-bridge FFI when mvm-apple-container crate is ready.
        // The flow will be:
        //   1. ContainerManager::new(kernel, network)
        //   2. manager.create(id, rootfs, cpus, memory)
        //   3. container.create()
        //   4. container.start()
        ui::info(&format!(
            "Starting Apple Container '{}' (cpus={}, mem={}MiB)...",
            config.name, config.cpus, config.memory_mib
        ));

        anyhow::bail!(
            "Apple Container backend is not yet connected to the Swift FFI bridge.\n\
             The Rust-side architecture is complete — the swift-bridge integration\n\
             will be added when macOS 26 and Xcode 26 are available.\n\
             Use '--hypervisor firecracker' for now."
        )
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        anyhow::bail!("Apple Container stop not yet implemented for VM '{}'", id.0)
    }

    fn stop_all(&self) -> Result<()> {
        // No containers to stop if we can't start any yet
        Ok(())
    }

    fn status(&self, id: &VmId) -> Result<VmStatus> {
        // No running containers in stub mode
        let _ = id;
        Ok(VmStatus::Stopped)
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        // No running containers in stub mode
        Ok(vec![])
    }

    fn logs(&self, id: &VmId, _lines: u32, _hypervisor: bool) -> Result<String> {
        anyhow::bail!("Apple Container logs not yet implemented for VM '{}'", id.0)
    }

    fn is_available(&self) -> Result<bool> {
        Ok(Self::is_platform_available())
    }

    fn install(&self) -> Result<()> {
        ui::info(
            "Apple Containers are built into macOS 26+. No separate installation needed.\n\
             Ensure you are running macOS 26 or later on Apple Silicon.",
        );
        Ok(())
    }

    fn network_info(&self, id: &VmId) -> Result<VmNetworkInfo> {
        // Apple Containers use vmnet with 192.168.64.0/24 subnet.
        // The actual IP is assigned dynamically by vmnet.
        anyhow::bail!(
            "Apple Container network info not yet available for VM '{}'",
            id.0
        )
    }

    fn guest_channel_info(&self, id: &VmId) -> Result<GuestChannelInfo> {
        // Apple Containers use vsock — vminitd on port 1024,
        // mvm guest agent on port 52.
        anyhow::bail!(
            "Apple Container guest channel not yet available for VM '{}'",
            id.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apple_container_backend_name() {
        let backend = AppleContainerBackend;
        assert_eq!(backend.name(), "apple-container");
    }

    #[test]
    fn test_apple_container_capabilities() {
        let backend = AppleContainerBackend;
        let caps = backend.capabilities();
        assert!(!caps.pause_resume);
        assert!(!caps.snapshots);
        assert!(caps.vsock);
        assert!(!caps.tap_networking);
    }

    #[test]
    fn test_apple_container_list_returns_empty() {
        let backend = AppleContainerBackend;
        let vms = backend.list().unwrap();
        assert!(vms.is_empty());
    }

    #[test]
    fn test_apple_container_stop_all_succeeds() {
        let backend = AppleContainerBackend;
        assert!(backend.stop_all().is_ok());
    }

    #[test]
    fn test_apple_container_status_returns_stopped() {
        let backend = AppleContainerBackend;
        let status = backend.status(&VmId("test".into())).unwrap();
        assert_eq!(status, VmStatus::Stopped);
    }
}
