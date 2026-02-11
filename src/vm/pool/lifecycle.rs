use anyhow::{Context, Result};

use super::config::{DesiredCounts, InstanceResources, PoolSpec, pool_config_path, pool_dir};
use crate::infra::shell;
use crate::vm::naming;
use crate::vm::tenant::lifecycle::tenant_exists;

/// Create a new pool under a tenant.
pub fn pool_create(
    tenant_id: &str,
    pool_id: &str,
    flake_ref: &str,
    profile: &str,
    resources: InstanceResources,
) -> Result<PoolSpec> {
    naming::validate_id(tenant_id, "Tenant")?;
    naming::validate_id(pool_id, "Pool")?;

    if !tenant_exists(tenant_id)? {
        anyhow::bail!("Tenant '{}' does not exist", tenant_id);
    }

    let dir = pool_dir(tenant_id, pool_id);
    shell::run_in_vm(&format!(
        "mkdir -p {dir}/artifacts/revisions {dir}/instances {dir}/snapshots/base"
    ))?;

    let spec = PoolSpec {
        pool_id: pool_id.to_string(),
        tenant_id: tenant_id.to_string(),
        flake_ref: flake_ref.to_string(),
        profile: profile.to_string(),
        instance_resources: resources,
        desired_counts: DesiredCounts::default(),
        seccomp_policy: "baseline".to_string(),
        snapshot_compression: "none".to_string(),
        metadata_enabled: false,
        pinned: false,
        critical: false,
    };

    let json = serde_json::to_string_pretty(&spec)?;
    let path = pool_config_path(tenant_id, pool_id);
    shell::run_in_vm(&format!("cat > {} << 'MVMEOF'\n{}\nMVMEOF", path, json))?;

    Ok(spec)
}

/// Load a pool spec from disk.
pub fn pool_load(tenant_id: &str, pool_id: &str) -> Result<PoolSpec> {
    let path = pool_config_path(tenant_id, pool_id);
    let json = shell::run_in_vm_stdout(&format!("cat {}", path))
        .with_context(|| format!("Failed to load pool: {}/{}", tenant_id, pool_id))?;
    let spec: PoolSpec = serde_json::from_str(&json)?;
    Ok(spec)
}

/// List all pool IDs for a tenant.
pub fn pool_list(tenant_id: &str) -> Result<Vec<String>> {
    let output = shell::run_in_vm_stdout(&format!(
        "ls -1 /var/lib/mvm/tenants/{}/pools/ 2>/dev/null || true",
        tenant_id
    ))?;
    Ok(output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Update desired counts for a pool.
pub fn pool_scale(
    tenant_id: &str,
    pool_id: &str,
    running: Option<u32>,
    warm: Option<u32>,
    sleeping: Option<u32>,
) -> Result<()> {
    let mut spec = pool_load(tenant_id, pool_id)?;

    if let Some(r) = running {
        spec.desired_counts.running = r;
    }
    if let Some(w) = warm {
        spec.desired_counts.warm = w;
    }
    if let Some(s) = sleeping {
        spec.desired_counts.sleeping = s;
    }

    let json = serde_json::to_string_pretty(&spec)?;
    let path = pool_config_path(tenant_id, pool_id);
    shell::run_in_vm(&format!("cat > {} << 'MVMEOF'\n{}\nMVMEOF", path, json))?;

    Ok(())
}

/// Destroy a pool and all its instances.
pub fn pool_destroy(tenant_id: &str, pool_id: &str, force: bool) -> Result<()> {
    let _ = force; // TODO: check for running instances unless force
    let dir = pool_dir(tenant_id, pool_id);
    shell::run_in_vm(&format!("rm -rf {}", dir))?;
    Ok(())
}
