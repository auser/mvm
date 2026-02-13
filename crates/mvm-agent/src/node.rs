use anyhow::Result;

use mvm_core::config;
use mvm_core::instance::InstanceStatus;
use mvm_core::node::{NodeInfo, NodeStats};
use mvm_runtime::security::{attestation, jailer};
use mvm_runtime::shell;
use mvm_runtime::vm::instance::lifecycle::instance_list;
use mvm_runtime::vm::pool::lifecycle::pool_list;
use mvm_runtime::vm::tenant::lifecycle::tenant_list;

/// Collect node information: Lima status, FC version, capabilities.
pub fn collect_info() -> Result<NodeInfo> {
    let hostname = shell::run_in_vm_stdout("hostname 2>/dev/null || echo unknown")
        .unwrap_or_else(|_| "unknown".to_string());

    let arch = config::ARCH.to_string();

    // Read node_id from persistent file, or generate one
    let node_id = shell::run_in_vm_stdout(
        "cat /var/lib/mvm/node_id 2>/dev/null || (uuidgen | tee /var/lib/mvm/node_id)",
    )
    .unwrap_or_else(|_| "unknown".to_string());

    // Lima VM status
    let lima_status = shell::run_in_vm_stdout("echo running").ok();

    // Firecracker version
    let fc_version =
        shell::run_in_vm_stdout("firecracker --version 2>/dev/null | head -1 || echo unknown").ok();

    // System resources
    let vcpus: u32 = shell::run_in_vm_stdout("nproc 2>/dev/null || echo 0")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let mem_mib: u64 = shell::run_in_vm_stdout(
        "awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0",
    )
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(0);

    let has_jailer = jailer::jailer_available().unwrap_or(false);

    let cgroup_v2 =
        shell::run_in_vm_stdout("test -f /sys/fs/cgroup/cgroup.controllers && echo yes || echo no")
            .map(|s| s.trim() == "yes")
            .unwrap_or(false);

    let attest_provider = attestation::default_provider();

    Ok(NodeInfo {
        node_id: node_id.trim().to_string(),
        hostname: hostname.trim().to_string(),
        arch,
        total_vcpus: vcpus,
        total_mem_mib: mem_mib,
        lima_status,
        firecracker_version: fc_version,
        jailer_available: has_jailer,
        cgroup_v2,
        attestation_provider: attest_provider.provider_name().to_string(),
    })
}

/// Collect aggregate node statistics across all tenants and pools.
pub fn collect_stats() -> Result<NodeStats> {
    let tenants = tenant_list()?;
    let mut stats = NodeStats {
        tenant_count: tenants.len() as u32,
        ..Default::default()
    };

    for tenant_id in &tenants {
        let pools = pool_list(tenant_id)?;
        stats.pool_count += pools.len() as u32;

        for pool_id in &pools {
            if let Ok(instances) = instance_list(tenant_id, pool_id) {
                for inst in &instances {
                    match inst.status {
                        InstanceStatus::Running => stats.running_instances += 1,
                        InstanceStatus::Warm => stats.warm_instances += 1,
                        InstanceStatus::Sleeping => stats.sleeping_instances += 1,
                        InstanceStatus::Stopped => stats.stopped_instances += 1,
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_info_roundtrip() {
        let info = NodeInfo {
            node_id: "abc-123".to_string(),
            hostname: "worker-1".to_string(),
            arch: "aarch64".to_string(),
            total_vcpus: 8,
            total_mem_mib: 16384,
            lima_status: Some("running".to_string()),
            firecracker_version: Some("v1.6.0".to_string()),
            jailer_available: true,
            cgroup_v2: true,
            attestation_provider: "none".to_string(),
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: NodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.node_id, "abc-123");
        assert_eq!(parsed.total_vcpus, 8);
        assert!(parsed.jailer_available);
    }

    #[test]
    fn test_node_stats_default() {
        let stats = NodeStats::default();
        assert_eq!(stats.running_instances, 0);
        assert_eq!(stats.tenant_count, 0);
    }

    #[test]
    fn test_node_stats_roundtrip() {
        let stats = NodeStats {
            running_instances: 5,
            warm_instances: 2,
            sleeping_instances: 10,
            stopped_instances: 1,
            total_vcpus_used: 20,
            total_mem_used_mib: 8192,
            tenant_count: 3,
            pool_count: 7,
        };

        let json = serde_json::to_string(&stats).unwrap();
        let parsed: NodeStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.running_instances, 5);
        assert_eq!(parsed.pool_count, 7);
    }
}
