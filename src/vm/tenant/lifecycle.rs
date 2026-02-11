use anyhow::{Context, Result};

use super::config::{TenantConfig, TenantNet, TenantQuota, tenant_config_path, tenant_dir};
use crate::infra::http;
use crate::infra::shell;

/// Create a new tenant: directories, config, SSH keypair.
pub fn tenant_create(tenant_id: &str, net: TenantNet, quotas: TenantQuota) -> Result<TenantConfig> {
    let dir = tenant_dir(tenant_id);
    shell::run_in_vm(&format!("mkdir -p {dir}/pools"))?;

    let config = TenantConfig {
        tenant_id: tenant_id.to_string(),
        quotas,
        net,
        secrets_epoch: 0,
        config_version: 1,
        pinned: false,
        audit_retention_days: 0,
        created_at: http::utc_now(),
    };

    let json = serde_json::to_string_pretty(&config)?;
    let path = tenant_config_path(tenant_id);
    shell::run_in_vm(&format!("cat > {} << 'MVMEOF'\n{}\nMVMEOF", path, json))?;

    Ok(config)
}

/// Load tenant config from disk.
pub fn tenant_load(tenant_id: &str) -> Result<TenantConfig> {
    let path = tenant_config_path(tenant_id);
    let json = shell::run_in_vm_stdout(&format!("cat {}", path))
        .with_context(|| format!("Failed to load tenant config: {}", tenant_id))?;
    let config: TenantConfig = serde_json::from_str(&json)?;
    Ok(config)
}

/// List all tenant IDs on this node.
pub fn tenant_list() -> Result<Vec<String>> {
    let output = shell::run_in_vm_stdout("ls -1 /var/lib/mvm/tenants/ 2>/dev/null || true")?;
    Ok(output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Check if a tenant exists.
pub fn tenant_exists(tenant_id: &str) -> Result<bool> {
    let path = tenant_config_path(tenant_id);
    let output = shell::run_in_vm_stdout(&format!("test -f {} && echo yes || echo no", path))?;
    Ok(output.trim() == "yes")
}

/// Destroy a tenant and all its resources.
pub fn tenant_destroy(tenant_id: &str, _wipe_volumes: bool) -> Result<()> {
    let dir = tenant_dir(tenant_id);
    shell::run_in_vm(&format!("rm -rf {}", dir))?;
    Ok(())
}
