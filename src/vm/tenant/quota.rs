use anyhow::Result;
use serde::Serialize;

use super::config::{TenantQuota, tenant_pools_dir};
use crate::infra::shell;
use crate::vm::instance::state::{InstanceState, InstanceStatus};
use crate::vm::pool::lifecycle::pool_load;

/// Current resource usage for a tenant across all its instances.
#[derive(Debug, Default, Serialize)]
pub struct TenantUsage {
    pub total_vcpus: u32,
    pub total_mem_mib: u64,
    pub running_count: u32,
    pub warm_count: u32,
    pub sleeping_count: u32,
    pub total_instances: u32,
}

/// Compute current resource usage for a tenant by scanning all instance.json files.
pub fn compute_tenant_usage(tenant_id: &str) -> Result<TenantUsage> {
    let mut usage = TenantUsage::default();

    let pools_dir = tenant_pools_dir(tenant_id);
    let output = shell::run_in_vm_stdout(&format!(
        "find {}/*/instances/*/instance.json -type f 2>/dev/null || true",
        pools_dir
    ))?;

    for path in output.lines().filter(|l| !l.is_empty()) {
        let json = match shell::run_in_vm_stdout(&format!("cat {}", path)) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let state: InstanceState = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(_) => continue,
        };

        usage.total_instances += 1;

        // Only count resource usage for active instances
        match state.status {
            InstanceStatus::Running => {
                usage.running_count += 1;
                if let Ok(spec) = pool_load(tenant_id, &state.pool_id) {
                    usage.total_vcpus += spec.instance_resources.vcpus as u32;
                    usage.total_mem_mib += spec.instance_resources.mem_mib as u64;
                }
            }
            InstanceStatus::Warm => {
                usage.warm_count += 1;
                if let Ok(spec) = pool_load(tenant_id, &state.pool_id) {
                    usage.total_vcpus += spec.instance_resources.vcpus as u32;
                    usage.total_mem_mib += spec.instance_resources.mem_mib as u64;
                }
            }
            InstanceStatus::Sleeping => {
                usage.sleeping_count += 1;
            }
            _ => {}
        }
    }

    Ok(usage)
}

/// Check whether starting/waking one more instance would exceed tenant quotas.
pub fn check_quota(
    quota: &TenantQuota,
    usage: &TenantUsage,
    additional_vcpus: u32,
    additional_mem_mib: u64,
) -> Result<()> {
    if usage.total_vcpus + additional_vcpus > quota.max_vcpus {
        anyhow::bail!(
            "Tenant quota exceeded: vCPUs ({} + {} > {})",
            usage.total_vcpus,
            additional_vcpus,
            quota.max_vcpus
        );
    }
    if usage.total_mem_mib + additional_mem_mib > quota.max_mem_mib {
        anyhow::bail!(
            "Tenant quota exceeded: memory ({} + {} > {} MiB)",
            usage.total_mem_mib,
            additional_mem_mib,
            quota.max_mem_mib
        );
    }
    if usage.running_count + 1 > quota.max_running {
        anyhow::bail!(
            "Tenant quota exceeded: running instances ({} >= {})",
            usage.running_count,
            quota.max_running
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_quota_within_limits() {
        let quota = TenantQuota::default(); // 16 vCPUs, 32768 MiB, 8 running
        let usage = TenantUsage {
            total_vcpus: 4,
            total_mem_mib: 4096,
            running_count: 2,
            ..Default::default()
        };
        assert!(check_quota(&quota, &usage, 2, 1024).is_ok());
    }

    #[test]
    fn test_check_quota_vcpu_exceeded() {
        let quota = TenantQuota::default();
        let usage = TenantUsage {
            total_vcpus: 15,
            total_mem_mib: 1024,
            running_count: 1,
            ..Default::default()
        };
        assert!(check_quota(&quota, &usage, 2, 1024).is_err());
    }

    #[test]
    fn test_check_quota_memory_exceeded() {
        let quota = TenantQuota::default();
        let usage = TenantUsage {
            total_vcpus: 2,
            total_mem_mib: 32000,
            running_count: 1,
            ..Default::default()
        };
        assert!(check_quota(&quota, &usage, 2, 1024).is_err());
    }

    #[test]
    fn test_check_quota_running_exceeded() {
        let quota = TenantQuota::default();
        let usage = TenantUsage {
            total_vcpus: 2,
            total_mem_mib: 1024,
            running_count: 8,
            ..Default::default()
        };
        assert!(check_quota(&quota, &usage, 2, 1024).is_err());
    }
}
