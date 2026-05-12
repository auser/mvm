//! Test-only in-memory backend.
//!
//! `MockBackend` records `start` / `stop` / `pause` / `resume` calls
//! against a `Mutex<HashMap>` and never touches the host. It exists so
//! the VM-lifecycle CLI verbs (`mvmctl up`, `down`, `pause`, `resume`,
//! `set-ttl`, `fs`, `proc`, `volume mount`, `snapshot`) can be exercised
//! end-to-end in hermetic tests — `cargo test` on a CI runner with no
//! KVM, no Apple Container, no Nix builder VM, no Docker daemon.
//!
//! Selected via `mvmctl up --hypervisor mock` (matches the
//! [`AnyBackend::from_hypervisor`] selector). Production callers don't
//! pick it explicitly — `AnyBackend::auto_select` never falls through
//! to it. Treat as test infrastructure; never trust it for anything
//! state-changing.
//!
//! ## Security profile
//!
//! Tier 3 / claims unknown. The mock satisfies none of ADR-002's
//! seven CI-enforced claims because it doesn't run a guest at all —
//! there's no isolation, no rootfs, no vsock. A loud `--hypervisor
//! mock` banner is expected (the CLI surfaces backend tier on
//! every `up`).

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, StartMode, VmBackend,
    VmCapabilities, VmExitStatus, VmId, VmInfo, VmNetworkInfo, VmStartConfig, VmStatus,
};

/// Per-VM state held by [`MockBackend`].
#[derive(Debug, Clone)]
struct MockVm {
    name: String,
    cpus: u32,
    memory_mib: u32,
    profile: Option<String>,
    flake_ref: Option<String>,
    revision: Option<String>,
    paused: bool,
}

/// In-memory test backend. See module docs.
///
/// The interior `Mutex<HashMap>` is wrapped in an `Arc` so cloning the
/// backend (e.g. across an `AnyBackend::Mock(MockBackend)` enum copy)
/// shares state. The `Default` impl returns an empty registry.
#[derive(Debug, Default, Clone)]
pub struct MockBackend {
    state: std::sync::Arc<Mutex<HashMap<String, MockVm>>>,
}

impl MockBackend {
    /// Construct a fresh empty mock backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: count VMs currently recorded.
    pub fn count(&self) -> usize {
        self.state.lock().map(|s| s.len()).unwrap_or(0)
    }
}

impl VmBackend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            pause_resume: true,
            snapshots: true,
            vsock: false,
            tap_networking: false,
        }
    }

    fn start_with_mode(&self, config: &VmStartConfig, _mode: StartMode) -> Result<VmId> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        if state.contains_key(&config.name) {
            bail!("mock: VM '{}' already running", config.name);
        }
        state.insert(
            config.name.clone(),
            MockVm {
                name: config.name.clone(),
                cpus: config.cpus,
                memory_mib: config.memory_mib,
                profile: config.profile.clone(),
                flake_ref: Some(config.flake_ref.clone()),
                revision: Some(config.revision_hash.clone()),
                paused: false,
            },
        );
        Ok(VmId(config.name.clone()))
    }

    fn wait(&self, _id: &VmId) -> Result<VmExitStatus> {
        // Mock VMs run forever and never exit. Wait would block
        // indefinitely; bailing matches the behavior of other
        // backends that don't support wait.
        bail!("mock: wait is not supported (mock VMs do not exit)")
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        state.remove(&id.0);
        Ok(())
    }

    fn stop_all(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        state.clear();
        Ok(())
    }

    fn pause(&self, id: &VmId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        match state.get_mut(&id.0) {
            Some(vm) => {
                vm.paused = true;
                Ok(())
            }
            None => bail!("mock: VM '{}' is not running", id.0),
        }
    }

    fn resume(&self, id: &VmId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        match state.get_mut(&id.0) {
            Some(vm) => {
                vm.paused = false;
                Ok(())
            }
            None => bail!("mock: VM '{}' is not running", id.0),
        }
    }

    fn status(&self, id: &VmId) -> Result<VmStatus> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        match state.get(&id.0) {
            Some(vm) if vm.paused => Ok(VmStatus::Paused),
            Some(_) => Ok(VmStatus::Running),
            None => Ok(VmStatus::Stopped),
        }
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        Ok(state
            .values()
            .map(|vm| VmInfo {
                id: VmId(vm.name.clone()),
                name: vm.name.clone(),
                status: if vm.paused {
                    VmStatus::Paused
                } else {
                    VmStatus::Running
                },
                guest_ip: None,
                cpus: vm.cpus,
                memory_mib: vm.memory_mib,
                profile: vm.profile.clone(),
                revision: vm.revision.clone(),
                flake_ref: vm.flake_ref.clone(),
                ports: Vec::new(),
            })
            .collect())
    }

    fn logs(&self, id: &VmId, _lines: u32, _hypervisor: bool) -> Result<String> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("mock backend state mutex poisoned"))?;
        if state.contains_key(&id.0) {
            Ok(format!("[mock] no logs for '{}' (mock backend)", id.0))
        } else {
            bail!("mock: VM '{}' is not running", id.0)
        }
    }

    fn is_available(&self) -> Result<bool> {
        Ok(true)
    }

    fn install(&self) -> Result<()> {
        Ok(())
    }

    fn network_info(&self, _id: &VmId) -> Result<VmNetworkInfo> {
        bail!("mock backend does not provide network info")
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        bail!("mock backend does not provide guest channel info")
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // The mock satisfies none of ADR-002's seven claims because
        // it doesn't run a guest at all. Operators selecting it via
        // `--hypervisor mock` get a loud Tier-3 banner so they
        // can't accidentally land production traffic on it.
        BackendSecurityProfile {
            claims: [ClaimStatus::DoesNotHold; 7],
            layer_coverage: LayerCoverage::default(),
            tier: "Tier 3 (test-only)",
            notes: &[
                "MockBackend is in-process test infrastructure.",
                "No guest, no rootfs, no isolation; never use in production.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mvm_core::vm_backend::VmStartConfig;

    fn cfg(name: &str) -> VmStartConfig {
        VmStartConfig {
            name: name.to_string(),
            kernel_path: None,
            initrd_path: None,
            rootfs_path: "/tmp/stub.ext4".to_string(),
            verity_path: None,
            roothash: None,
            revision_hash: "abc".to_string(),
            flake_ref: ".".to_string(),
            profile: Some("default".to_string()),
            cpus: 2,
            memory_mib: 512,
            volumes: Vec::new(),
            config_files: Vec::new(),
            secret_files: Vec::new(),
            ports: Vec::new(),
            runner_dir: None,
        }
    }

    #[test]
    fn start_records_vm_and_returns_id() {
        let b = MockBackend::new();
        let id = b.start(&cfg("vm-a")).unwrap();
        assert_eq!(id.0, "vm-a");
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn double_start_fails() {
        let b = MockBackend::new();
        b.start(&cfg("vm-a")).unwrap();
        let err = b.start(&cfg("vm-a")).unwrap_err();
        assert!(err.to_string().contains("already running"));
    }

    #[test]
    fn stop_removes_from_registry() {
        let b = MockBackend::new();
        b.start(&cfg("vm-a")).unwrap();
        b.stop(&VmId("vm-a".to_string())).unwrap();
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn pause_resume_round_trip() {
        let b = MockBackend::new();
        b.start(&cfg("vm-a")).unwrap();
        let id = VmId("vm-a".to_string());
        assert_eq!(b.status(&id).unwrap(), VmStatus::Running);
        b.pause(&id).unwrap();
        assert_eq!(b.status(&id).unwrap(), VmStatus::Paused);
        b.resume(&id).unwrap();
        assert_eq!(b.status(&id).unwrap(), VmStatus::Running);
    }

    #[test]
    fn list_returns_recorded_vms() {
        let b = MockBackend::new();
        b.start(&cfg("vm-a")).unwrap();
        b.start(&cfg("vm-b")).unwrap();
        let listed = b.list().unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn status_of_unknown_vm_is_stopped() {
        let b = MockBackend::new();
        let status = b.status(&VmId("nonexistent".to_string())).unwrap();
        assert_eq!(status, VmStatus::Stopped);
    }

    #[test]
    fn capabilities_advertises_pause_resume_and_snapshots() {
        let b = MockBackend::new();
        let caps = b.capabilities();
        assert!(caps.pause_resume);
        assert!(caps.snapshots);
        // No vsock / tap-networking — mock has no guest channel.
        assert!(!caps.vsock);
        assert!(!caps.tap_networking);
    }

    #[test]
    fn security_profile_is_tier_3_test_only() {
        let b = MockBackend::new();
        let profile = b.security_profile();
        assert_eq!(profile.tier, "Tier 3 (test-only)");
        assert!(
            profile
                .claims
                .iter()
                .all(|c| matches!(c, ClaimStatus::DoesNotHold)),
            "mock backend must not claim any ADR-002 guarantees"
        );
    }

    #[test]
    fn stop_all_clears_the_registry() {
        let b = MockBackend::new();
        b.start(&cfg("vm-a")).unwrap();
        b.start(&cfg("vm-b")).unwrap();
        b.start(&cfg("vm-c")).unwrap();
        b.stop_all().unwrap();
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn shared_state_across_clones() {
        // The `Arc` lets `AnyBackend::Mock(MockBackend)` clone the
        // outer wrapper without losing track of in-flight VMs.
        let b1 = MockBackend::new();
        let b2 = b1.clone();
        b1.start(&cfg("vm-a")).unwrap();
        assert_eq!(b2.count(), 1, "cloned mock must see vm-a started via b1");
    }
}
