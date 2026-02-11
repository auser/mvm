use anyhow::Result;
use serde::Serialize;
use tracing::{info, warn};

use super::lifecycle::{instance_list, is_pid_alive};
use super::state::InstanceStatus;
use crate::infra::shell;
use crate::vm::pool::lifecycle::pool_list;
use crate::vm::tenant::lifecycle::tenant_list;

/// Result of a stale PID scan for a single instance.
#[derive(Debug, Clone, Serialize)]
pub struct StalePidResult {
    pub tenant_id: String,
    pub pool_id: String,
    pub instance_id: String,
    pub recorded_pid: u32,
    pub actually_alive: bool,
}

/// Scan all instances across all tenants for stale PIDs.
///
/// An instance is stale if it claims to be Running/Warm but its
/// Firecracker PID is no longer alive.
pub fn detect_stale_pids() -> Result<Vec<StalePidResult>> {
    let mut results = Vec::new();

    for tid in tenant_list()? {
        for pid in pool_list(&tid)? {
            if let Ok(instances) = instance_list(&tid, &pid) {
                for inst in &instances {
                    if matches!(inst.status, InstanceStatus::Running | InstanceStatus::Warm)
                        && let Some(fc_pid) = inst.firecracker_pid
                    {
                        let alive = is_pid_alive(fc_pid).unwrap_or(false);
                        if !alive {
                            warn!(
                                tenant_id = %tid,
                                pool_id = %pid,
                                instance_id = %inst.instance_id,
                                pid = fc_pid,
                                "Stale PID detected — process no longer alive"
                            );
                            results.push(StalePidResult {
                                tenant_id: tid.clone(),
                                pool_id: pid.clone(),
                                instance_id: inst.instance_id.clone(),
                                recorded_pid: fc_pid,
                                actually_alive: false,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(results)
}

/// Result of an orphan scan.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanResult {
    pub path: String,
    pub reason: String,
}

/// Scan for orphaned instance directories whose parent pool or tenant no longer exists.
///
/// Does NOT auto-delete — callers should review and decide.
pub fn detect_orphans() -> Result<Vec<OrphanResult>> {
    let mut orphans = Vec::new();
    let existing_tenants = tenant_list()?;

    // Scan the filesystem for tenant directories
    let output = shell::run_in_vm_stdout("ls -1 /var/lib/mvm/tenants/ 2>/dev/null || true")?;

    for tenant_dir in output.lines().filter(|l| !l.is_empty()) {
        if !existing_tenants.contains(&tenant_dir.to_string()) {
            // Check if this directory has a tenant.json — if not, it's orphaned
            let has_config = shell::run_in_vm_stdout(&format!(
                "test -f /var/lib/mvm/tenants/{}/tenant.json && echo yes || echo no",
                tenant_dir
            ))?;
            if has_config.trim() != "yes" {
                orphans.push(OrphanResult {
                    path: format!("/var/lib/mvm/tenants/{}", tenant_dir),
                    reason: "Tenant directory without config".to_string(),
                });
                continue;
            }
        }

        // Check pools within this tenant
        let pools_output = shell::run_in_vm_stdout(&format!(
            "ls -1 /var/lib/mvm/tenants/{}/pools/ 2>/dev/null || true",
            tenant_dir
        ))?;

        for pool_dir in pools_output.lines().filter(|l| !l.is_empty()) {
            let has_pool_config = shell::run_in_vm_stdout(&format!(
                "test -f /var/lib/mvm/tenants/{}/pools/{}/pool.json && echo yes || echo no",
                tenant_dir, pool_dir
            ))?;

            if has_pool_config.trim() != "yes" {
                orphans.push(OrphanResult {
                    path: format!("/var/lib/mvm/tenants/{}/pools/{}", tenant_dir, pool_dir),
                    reason: "Pool directory without config".to_string(),
                });
            }
        }
    }

    if !orphans.is_empty() {
        info!(count = orphans.len(), "Orphaned directories detected");
    }

    Ok(orphans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::shell_mock;

    #[test]
    fn test_detect_stale_pids_empty() {
        let (_guard, _fs) = shell_mock::mock_fs().install();
        let results = detect_stale_pids().unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_detect_orphans_clean() {
        let tenant_json = shell_mock::tenant_fixture("acme", 3, "10.240.3.0/24", "10.240.3.1");
        let (_guard, _fs) = shell_mock::mock_fs()
            .with_file("/var/lib/mvm/tenants/acme/tenant.json", &tenant_json)
            .install();

        let orphans = detect_orphans().unwrap();
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_detect_orphans_finds_missing_pool_config() {
        let tenant_json = shell_mock::tenant_fixture("acme", 3, "10.240.3.0/24", "10.240.3.1");
        // Pool directory exists but no pool.json
        let (_guard, _fs) = shell_mock::mock_fs()
            .with_file("/var/lib/mvm/tenants/acme/tenant.json", &tenant_json)
            .with_file(
                "/var/lib/mvm/tenants/acme/pools/orphaned/instances/i-123/instance.json",
                "{}",
            )
            .install();

        let orphans = detect_orphans().unwrap();
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].path.contains("orphaned"));
        assert!(orphans[0].reason.contains("Pool directory without config"));
    }
}
