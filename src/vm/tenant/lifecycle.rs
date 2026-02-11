use anyhow::{Context, Result};
use tracing::instrument;

use super::config::{TenantConfig, TenantNet, TenantQuota, tenant_config_path, tenant_dir};
use crate::infra::http;
use crate::infra::shell;

/// Create a new tenant: directories, config, SSH keypair.
#[instrument(skip_all, fields(tenant_id))]
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

/// Load tenant config from disk, with validation.
pub fn tenant_load(tenant_id: &str) -> Result<TenantConfig> {
    let path = tenant_config_path(tenant_id);
    let json = shell::run_in_vm_stdout(&format!("cat {}", path))
        .with_context(|| format!("Failed to load tenant config: {}", tenant_id))?;
    let config: TenantConfig = serde_json::from_str(&json)
        .with_context(|| format!("Corrupt tenant config at {}", path))?;
    if config.tenant_id.is_empty() {
        anyhow::bail!("Tenant config has empty tenant_id at {}", path);
    }
    if config.net.ipv4_subnet.is_empty() {
        anyhow::bail!("Tenant {} has empty subnet", config.tenant_id);
    }
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
#[instrument(skip_all, fields(tenant_id))]
pub fn tenant_destroy(tenant_id: &str, _wipe_volumes: bool) -> Result<()> {
    let dir = tenant_dir(tenant_id);
    shell::run_in_vm(&format!("rm -rf {}", dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::shell_mock;

    #[test]
    fn test_tenant_create_and_load() {
        let (_guard, _fs) = shell_mock::mock_fs().install();

        let net = TenantNet::new(3, "10.240.3.0/24", "10.240.3.1");
        let config = tenant_create("acme", net, TenantQuota::default()).unwrap();
        assert_eq!(config.tenant_id, "acme");
        assert_eq!(config.net.tenant_net_id, 3);

        let loaded = tenant_load("acme").unwrap();
        assert_eq!(loaded.tenant_id, "acme");
        assert_eq!(loaded.net.gateway_ip, "10.240.3.1");
        assert_eq!(loaded.net.ipv4_subnet, "10.240.3.0/24");
    }

    #[test]
    fn test_tenant_list_empty() {
        let (_guard, _fs) = shell_mock::mock_fs().install();
        let tenants = tenant_list().unwrap();
        assert!(tenants.is_empty());
    }

    #[test]
    fn test_tenant_create_then_list() {
        let (_guard, _fs) = shell_mock::mock_fs().install();

        tenant_create(
            "acme",
            TenantNet::new(3, "10.240.3.0/24", "10.240.3.1"),
            TenantQuota::default(),
        )
        .unwrap();
        tenant_create(
            "beta",
            TenantNet::new(4, "10.240.4.0/24", "10.240.4.1"),
            TenantQuota::default(),
        )
        .unwrap();

        let mut tenants = tenant_list().unwrap();
        tenants.sort();
        assert_eq!(tenants, vec!["acme", "beta"]);
    }

    #[test]
    fn test_tenant_exists() {
        let (_guard, _fs) = shell_mock::mock_fs().install();
        assert!(!tenant_exists("acme").unwrap());

        tenant_create(
            "acme",
            TenantNet::new(3, "10.240.3.0/24", "10.240.3.1"),
            TenantQuota::default(),
        )
        .unwrap();
        assert!(tenant_exists("acme").unwrap());
    }

    #[test]
    fn test_tenant_create_then_destroy() {
        let (_guard, _fs) = shell_mock::mock_fs().install();

        tenant_create(
            "acme",
            TenantNet::new(3, "10.240.3.0/24", "10.240.3.1"),
            TenantQuota::default(),
        )
        .unwrap();
        assert!(tenant_exists("acme").unwrap());

        tenant_destroy("acme", true).unwrap();
        assert!(!tenant_exists("acme").unwrap());
    }

    #[test]
    fn test_tenant_load_nonexistent_fails() {
        let (_guard, _fs) = shell_mock::mock_fs().install();
        assert!(tenant_load("nonexistent").is_err());
    }

    #[test]
    fn test_tenant_config_preserves_quotas() {
        let (_guard, _fs) = shell_mock::mock_fs().install();

        let quotas = TenantQuota {
            max_vcpus: 32,
            max_mem_mib: 65536,
            max_running: 16,
            max_warm: 8,
            max_pools: 10,
            max_instances_per_pool: 32,
            max_disk_gib: 500,
        };
        tenant_create(
            "acme",
            TenantNet::new(3, "10.240.3.0/24", "10.240.3.1"),
            quotas,
        )
        .unwrap();

        let loaded = tenant_load("acme").unwrap();
        assert_eq!(loaded.quotas.max_vcpus, 32);
        assert_eq!(loaded.quotas.max_mem_mib, 65536);
        assert_eq!(loaded.quotas.max_running, 16);
    }
}
