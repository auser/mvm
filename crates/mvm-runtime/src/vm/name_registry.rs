use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Persistent registry mapping VM names to their runtime directories.
///
/// Stored as `{mvm_share_dir}/vm-names.json`. Enables reliable name-based
/// lookups for `mvmctl logs`, `mvmctl forward`, `mvmctl down`, etc.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VmNameRegistry {
    /// Map from VM name to registration info.
    pub vms: HashMap<String, VmRegistration>,
}

/// Registration info for a running or recently-running VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRegistration {
    /// Absolute path to the VM's runtime directory.
    pub vm_dir: String,
    /// Network name the VM is attached to.
    pub network: String,
    /// Guest IP address.
    pub guest_ip: Option<String>,
    /// VM slot index.
    pub slot_index: u8,
    /// RFC 3339 timestamp of registration.
    pub registered_at: String,
}

impl VmNameRegistry {
    /// Load the registry from disk. Returns an empty registry if the file
    /// doesn't exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read VM name registry: {}", path.display()))?;
        let registry: Self = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse VM name registry: {}", path.display()))?;
        Ok(registry)
    }

    /// Save the registry to disk, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text =
            serde_json::to_string_pretty(self).context("Failed to serialize VM name registry")?;
        mvm_core::atomic_io::atomic_write(path, text.as_bytes())
            .with_context(|| format!("Failed to write VM name registry: {}", path.display()))
    }

    /// Register a VM name. Returns an error if the name is already taken.
    pub fn register(
        &mut self,
        name: &str,
        vm_dir: &str,
        network: &str,
        guest_ip: Option<&str>,
        slot_index: u8,
    ) -> Result<()> {
        if self.vms.contains_key(name) {
            bail!("VM name {:?} is already registered", name);
        }
        let timestamp = mvm_core::time::utc_now();
        self.vms.insert(
            name.to_string(),
            VmRegistration {
                vm_dir: vm_dir.to_string(),
                network: network.to_string(),
                guest_ip: guest_ip.map(str::to_owned),
                slot_index,
                registered_at: timestamp,
            },
        );
        Ok(())
    }

    /// Deregister a VM by name. Returns the registration if it existed.
    pub fn deregister(&mut self, name: &str) -> Option<VmRegistration> {
        self.vms.remove(name)
    }

    /// Look up a VM by name.
    pub fn lookup(&self, name: &str) -> Option<&VmRegistration> {
        self.vms.get(name)
    }

    /// List all registered VM names.
    pub fn names(&self) -> Vec<&str> {
        self.vms.keys().map(String::as_str).collect()
    }

    /// Number of registered VMs.
    pub fn len(&self) -> usize {
        self.vms.len()
    }

    /// Check if registry is empty.
    pub fn is_empty(&self) -> bool {
        self.vms.is_empty()
    }
}

/// Default registry file path.
pub fn registry_path() -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_share_dir()).join("vm-names.json")
}

/// Generate a unique VM name with a random suffix.
pub fn generate_vm_name() -> String {
    let id = mvm_core::naming::generate_instance_id();
    // Use the hex suffix from instance ID generation
    let suffix = &id[2..]; // strip "i-" prefix
    format!("vm-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_registry() {
        let reg = VmNameRegistry::default();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.names().is_empty());
    }

    #[test]
    fn test_register_and_lookup() {
        let mut reg = VmNameRegistry::default();
        reg.register("myvm", "/tmp/vms/myvm", "default", Some("172.16.0.2"), 0)
            .unwrap();

        assert_eq!(reg.len(), 1);
        let info = reg.lookup("myvm").unwrap();
        assert_eq!(info.vm_dir, "/tmp/vms/myvm");
        assert_eq!(info.network, "default");
        assert_eq!(info.guest_ip.as_deref(), Some("172.16.0.2"));
    }

    #[test]
    fn test_register_duplicate_fails() {
        let mut reg = VmNameRegistry::default();
        reg.register("myvm", "/tmp/vms/myvm", "default", None, 0)
            .unwrap();
        assert!(
            reg.register("myvm", "/tmp/vms/myvm2", "default", None, 1)
                .is_err()
        );
    }

    #[test]
    fn test_deregister() {
        let mut reg = VmNameRegistry::default();
        reg.register("myvm", "/tmp/vms/myvm", "default", None, 0)
            .unwrap();
        let removed = reg.deregister("myvm");
        assert!(removed.is_some());
        assert!(reg.is_empty());
        assert!(reg.lookup("myvm").is_none());
    }

    #[test]
    fn test_deregister_nonexistent() {
        let mut reg = VmNameRegistry::default();
        assert!(reg.deregister("nonexistent").is_none());
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut reg = VmNameRegistry::default();
        reg.register("vm1", "/tmp/vms/vm1", "default", Some("172.16.0.2"), 0)
            .unwrap();
        reg.register("vm2", "/tmp/vms/vm2", "isolated", None, 1)
            .unwrap();

        let json = serde_json::to_string(&reg).unwrap();
        let parsed: VmNameRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed.lookup("vm1").is_some());
        assert!(parsed.lookup("vm2").is_some());
    }

    #[test]
    fn test_load_save_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vm-names.json");

        let mut reg = VmNameRegistry::default();
        reg.register("myvm", "/tmp/vms/myvm", "default", Some("172.16.0.2"), 0)
            .unwrap();
        reg.save(&path).unwrap();

        let loaded = VmNameRegistry::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.lookup("myvm").unwrap().guest_ip.as_deref(),
            Some("172.16.0.2")
        );
    }

    #[test]
    fn test_load_nonexistent_returns_empty() {
        let path = PathBuf::from("/nonexistent/vm-names.json");
        let reg = VmNameRegistry::load(&path).unwrap();
        assert!(reg.is_empty());
    }

    #[test]
    fn test_generate_vm_name_format() {
        let name = generate_vm_name();
        assert!(name.starts_with("vm-"));
        assert!(name.len() > 3);
    }

    #[test]
    fn test_generate_vm_name_unique() {
        let name1 = generate_vm_name();
        let name2 = generate_vm_name();
        assert_ne!(name1, name2);
    }

    #[test]
    fn test_names_list() {
        let mut reg = VmNameRegistry::default();
        reg.register("alpha", "/tmp/alpha", "default", None, 0)
            .unwrap();
        reg.register("beta", "/tmp/beta", "default", None, 1)
            .unwrap();
        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }
}
