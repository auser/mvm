use serde::{Deserialize, Serialize};

use crate::vm::tenant::config::tenant_pools_dir;

/// A WorkerPool defines a homogeneous group of instances within a tenant.
/// Has desired counts but NO runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolSpec {
    pub pool_id: String,
    pub tenant_id: String,
    pub flake_ref: String,
    /// Guest profile name. Built-in: "minimal", "python".
    /// Users can define custom profiles in their own flake.
    /// The build system evaluates: `nix build <flake_ref>#tenant-<profile>`
    pub profile: String,
    pub instance_resources: InstanceResources,
    pub desired_counts: DesiredCounts,
    /// "baseline" | "strict"
    #[serde(default = "default_seccomp")]
    pub seccomp_policy: String,
    /// "none" | "lz4" | "zstd"
    #[serde(default = "default_compression")]
    pub snapshot_compression: String,
    #[serde(default)]
    pub metadata_enabled: bool,
    /// If true, reconcile won't auto-sleep this pool's instances.
    #[serde(default)]
    pub pinned: bool,
    /// If true, reconcile won't touch this pool at all.
    #[serde(default)]
    pub critical: bool,
}

fn default_seccomp() -> String {
    "baseline".to_string()
}

fn default_compression() -> String {
    "none".to_string()
}

/// Resource allocation for each instance in the pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceResources {
    pub vcpus: u8,
    pub mem_mib: u32,
    #[serde(default)]
    pub data_disk_mib: u32,
}

/// Desired instance counts by status, evaluated by the reconcile loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredCounts {
    pub running: u32,
    pub warm: u32,
    pub sleeping: u32,
}

/// A completed build revision with artifact locations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRevision {
    pub revision_hash: String,
    pub flake_ref: String,
    pub flake_lock_hash: String,
    pub artifact_paths: ArtifactPaths,
    pub built_at: String,
}

/// Paths to build artifacts within the pool's artifact directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPaths {
    pub vmlinux: String,
    pub rootfs: String,
    pub fc_base_config: String,
}

// --- Filesystem paths ---

pub fn pool_dir(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/{}", tenant_pools_dir(tenant_id), pool_id)
}

pub fn pool_config_path(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/pool.json", pool_dir(tenant_id, pool_id))
}

pub fn pool_artifacts_dir(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/artifacts", pool_dir(tenant_id, pool_id))
}

pub fn pool_instances_dir(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/instances", pool_dir(tenant_id, pool_id))
}

pub fn pool_snapshots_dir(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/snapshots", pool_dir(tenant_id, pool_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_dir_path() {
        assert_eq!(
            pool_dir("acme", "workers"),
            "/var/lib/mvm/tenants/acme/pools/workers"
        );
    }

    #[test]
    fn test_pool_config_roundtrip() {
        let spec = PoolSpec {
            pool_id: "workers".to_string(),
            tenant_id: "acme".to_string(),
            flake_ref: "github:org/repo".to_string(),
            profile: "minimal".to_string(),
            instance_resources: InstanceResources {
                vcpus: 2,
                mem_mib: 1024,
                data_disk_mib: 2048,
            },
            desired_counts: DesiredCounts {
                running: 3,
                warm: 1,
                sleeping: 2,
            },
            seccomp_policy: "baseline".to_string(),
            snapshot_compression: "zstd".to_string(),
            metadata_enabled: false,
            pinned: false,
            critical: false,
        };

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: PoolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pool_id, "workers");
        assert_eq!(parsed.instance_resources.vcpus, 2);
        assert_eq!(parsed.desired_counts.running, 3);
    }
}
