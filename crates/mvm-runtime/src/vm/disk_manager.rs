use anyhow::Result;
use serde::Serialize;

use crate::shell;
use crate::vm::{pool, tenant};

/// Disk usage summary for a tenant.
#[derive(Debug, Serialize)]
pub struct TenantDiskUsage {
    pub tenant_id: String,
    pub total_bytes: u64,
    pub pools: Vec<PoolDiskUsage>,
}

/// Disk usage summary for a pool.
#[derive(Debug, Serialize)]
pub struct PoolDiskUsage {
    pub pool_id: String,
    pub artifacts_bytes: u64,
    pub instances_bytes: u64,
    pub total_bytes: u64,
}

/// Aggregate disk usage for the entire mvm data directory.
#[derive(Debug, Serialize)]
pub struct DiskReport {
    pub total_bytes: u64,
    pub tenants: Vec<TenantDiskUsage>,
}

/// Scan and report disk usage across all tenants and pools.
pub fn disk_usage_report() -> Result<DiskReport> {
    let tenant_ids = tenant::lifecycle::tenant_list()?;
    let mut tenants = Vec::new();
    let mut total = 0u64;

    for tid in &tenant_ids {
        let mut tenant_usage = TenantDiskUsage {
            tenant_id: tid.clone(),
            total_bytes: 0,
            pools: Vec::new(),
        };

        if let Ok(pool_ids) = pool::lifecycle::pool_list(tid) {
            for pid in &pool_ids {
                let pool_usage = measure_pool_disk(tid, pid);
                tenant_usage.total_bytes += pool_usage.total_bytes;
                tenant_usage.pools.push(pool_usage);
            }
        }

        total += tenant_usage.total_bytes;
        tenants.push(tenant_usage);
    }

    Ok(DiskReport {
        total_bytes: total,
        tenants,
    })
}

fn measure_pool_disk(tenant_id: &str, pool_id: &str) -> PoolDiskUsage {
    let artifacts_dir = format!(
        "/var/lib/mvm/tenants/{}/pools/{}/artifacts",
        tenant_id, pool_id
    );
    let instances_dir = format!(
        "/var/lib/mvm/tenants/{}/pools/{}/instances",
        tenant_id, pool_id
    );

    let artifacts_bytes = dir_size(&artifacts_dir).unwrap_or(0);
    let instances_bytes = dir_size(&instances_dir).unwrap_or(0);

    PoolDiskUsage {
        pool_id: pool_id.to_string(),
        artifacts_bytes,
        instances_bytes,
        total_bytes: artifacts_bytes + instances_bytes,
    }
}

fn dir_size(path: &str) -> Result<u64> {
    let output =
        shell::run_in_vm_stdout(&format!("du -sb {} 2>/dev/null | awk '{{print $1}}'", path))?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    Ok(trimmed.parse().unwrap_or(0))
}

/// Clean up old build revisions for a pool, keeping the N most recent.
pub fn cleanup_old_revisions(tenant_id: &str, pool_id: &str, keep_n: usize) -> Result<u32> {
    let artifacts_dir = format!(
        "/var/lib/mvm/tenants/{}/pools/{}/artifacts/revisions",
        tenant_id, pool_id
    );

    // List revisions sorted by modification time (oldest first)
    let output = shell::run_in_vm_stdout(&format!("ls -1t {} 2>/dev/null || true", artifacts_dir))?;

    let revisions: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

    if revisions.len() <= keep_n {
        return Ok(0);
    }

    let to_remove = &revisions[keep_n..];
    let mut removed = 0u32;

    for rev in to_remove {
        let rev_path = format!("{}/{}", artifacts_dir, rev);
        // Zero-fill files before unlinking for secure deletion
        let _ = secure_wipe_dir(&rev_path);
        if shell::run_in_vm(&format!("rm -rf {}", rev_path)).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

/// Zero-fill all regular files in a directory before deletion.
/// Prevents data recovery from freed disk blocks.
pub fn secure_wipe_dir(path: &str) -> Result<()> {
    shell::run_in_vm(&format!(
        r#"
        find {path} -type f 2>/dev/null | while read f; do
            SIZE=$(stat -c%s "$f" 2>/dev/null || echo 0)
            if [ "$SIZE" -gt 0 ]; then
                dd if=/dev/zero of="$f" bs=4096 count=$((SIZE / 4096 + 1)) conv=notrunc 2>/dev/null || true
            fi
        done
        "#,
        path = path,
    ))?;
    Ok(())
}

/// Securely wipe a single file (zero-fill then unlink).
pub fn secure_wipe_file(path: &str) -> Result<()> {
    shell::run_in_vm(&format!(
        r#"
        if [ -f {path} ]; then
            SIZE=$(stat -c%s {path} 2>/dev/null || echo 0)
            if [ "$SIZE" -gt 0 ]; then
                dd if=/dev/zero of={path} bs=4096 count=$((SIZE / 4096 + 1)) conv=notrunc 2>/dev/null || true
            fi
            rm -f {path}
        fi
        "#,
        path = path,
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_disk_usage_struct() {
        let usage = PoolDiskUsage {
            pool_id: "workers".to_string(),
            artifacts_bytes: 1024,
            instances_bytes: 2048,
            total_bytes: 3072,
        };
        assert_eq!(
            usage.total_bytes,
            usage.artifacts_bytes + usage.instances_bytes
        );
    }

    #[test]
    fn test_disk_report_struct() {
        let report = DiskReport {
            total_bytes: 5000,
            tenants: vec![TenantDiskUsage {
                tenant_id: "acme".to_string(),
                total_bytes: 5000,
                pools: vec![],
            }],
        };
        assert_eq!(report.tenants.len(), 1);
        assert_eq!(report.total_bytes, 5000);
    }

    #[test]
    fn test_disk_report_serializes() {
        let report = DiskReport {
            total_bytes: 0,
            tenants: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("total_bytes"));
    }

    #[test]
    fn test_disk_report_with_mock() {
        use crate::shell_mock;
        let (_guard, _fs) = shell_mock::mock_fs().install();

        let report = disk_usage_report().unwrap();
        assert_eq!(report.total_bytes, 0);
        assert!(report.tenants.is_empty());
    }

    #[test]
    fn test_cleanup_old_revisions_empty() {
        use crate::shell_mock;
        let (_guard, _fs) = shell_mock::mock_fs().install();

        let removed = cleanup_old_revisions("acme", "workers", 2).unwrap();
        assert_eq!(removed, 0);
    }
}
