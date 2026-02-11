use anyhow::{Context, Result};

use crate::infra::shell;

const CGROUP_BASE: &str = "/sys/fs/cgroup/mvm";

/// Create a cgroup v2 hierarchy for a tenant instance.
///
/// Path: /sys/fs/cgroup/mvm/<tenant_id>/<instance_id>/
/// Sets memory.max, cpu.max (bandwidth), and pids.max limits.
pub fn create_instance_cgroup(
    tenant_id: &str,
    instance_id: &str,
    vcpus: u8,
    mem_mib: u32,
) -> Result<()> {
    let cgroup_path = format!("{}/{}/{}", CGROUP_BASE, tenant_id, instance_id);

    // cpu.max format: "quota period" in microseconds
    // e.g., 2 vCPUs = 200000 100000 (200ms every 100ms period)
    let cpu_quota = vcpus as u64 * 100_000;
    let cpu_period = 100_000u64;

    // memory.max in bytes
    let mem_bytes = mem_mib as u64 * 1024 * 1024;

    // pids.max: generous limit based on vCPU count
    let pids_max = vcpus as u32 * 512;

    shell::run_in_vm(&format!(
        r#"
        sudo mkdir -p {path}
        echo "{mem}" | sudo tee {path}/memory.max > /dev/null
        echo "{quota} {period}" | sudo tee {path}/cpu.max > /dev/null
        echo "{pids}" | sudo tee {path}/pids.max > /dev/null
        "#,
        path = cgroup_path,
        mem = mem_bytes,
        quota = cpu_quota,
        period = cpu_period,
        pids = pids_max,
    ))
    .with_context(|| format!("Failed to create cgroup for {}/{}", tenant_id, instance_id))?;

    Ok(())
}

/// Remove the cgroup for a stopped/destroyed instance.
pub fn remove_instance_cgroup(tenant_id: &str, instance_id: &str) -> Result<()> {
    let cgroup_path = format!("{}/{}/{}", CGROUP_BASE, tenant_id, instance_id);

    // Kill any remaining processes in the cgroup before removing
    let parent = format!("{}/{}", CGROUP_BASE, tenant_id);
    shell::run_in_vm(&format!(
        r#"
        if [ -d {path} ]; then
            # Move processes to parent before removal
            cat {path}/cgroup.procs 2>/dev/null | while read pid; do
                echo "$pid" | sudo tee {parent}/cgroup.procs > /dev/null 2>&1 || true
            done
            sudo rmdir {path} 2>/dev/null || true
        fi
        "#,
        path = cgroup_path,
        parent = parent,
    ))?;

    Ok(())
}

/// Compute aggregate cgroup resource usage for a tenant.
///
/// Reads memory.current and cpu.stat from all instance cgroups under the tenant.
/// Returns (total_mem_bytes, total_cpu_usage_usec).
pub fn tenant_cgroup_usage(tenant_id: &str) -> Result<(u64, u64)> {
    let tenant_path = format!("{}/{}", CGROUP_BASE, tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        r#"
        total_mem=0
        total_cpu=0
        if [ -d {path} ]; then
            for inst_dir in {path}/*/; do
                [ -d "$inst_dir" ] || continue
                if [ -f "$inst_dir/memory.current" ]; then
                    mem=$(cat "$inst_dir/memory.current" 2>/dev/null || echo 0)
                    total_mem=$((total_mem + mem))
                fi
                if [ -f "$inst_dir/cpu.stat" ]; then
                    cpu=$(grep '^usage_usec' "$inst_dir/cpu.stat" 2>/dev/null | awk '{{print $2}}')
                    [ -n "$cpu" ] && total_cpu=$((total_cpu + cpu))
                fi
            done
        fi
        echo "$total_mem $total_cpu"
        "#,
        path = tenant_path,
    ))?;

    let parts: Vec<&str> = output.split_whitespace().collect();
    let mem: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let cpu: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

    Ok((mem, cpu))
}

/// Get the cgroup path for an instance (useful for jailer integration).
pub fn instance_cgroup_path(tenant_id: &str, instance_id: &str) -> String {
    format!("{}/{}/{}", CGROUP_BASE, tenant_id, instance_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance_cgroup_path() {
        assert_eq!(
            instance_cgroup_path("acme", "i-abc123"),
            "/sys/fs/cgroup/mvm/acme/i-abc123"
        );
    }

    #[test]
    fn test_cgroup_base_path() {
        assert_eq!(CGROUP_BASE, "/sys/fs/cgroup/mvm");
    }
}
