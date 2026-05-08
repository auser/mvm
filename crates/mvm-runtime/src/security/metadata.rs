use anyhow::{Context, Result};

use crate::shell;

/// Set up a per-tenant metadata endpoint on the bridge gateway.
///
/// Creates nftables rules that:
/// 1. Allow instances on the tenant's bridge to reach the metadata service
///    at the gateway IP on port 8169 (inspired by cloud metadata at 169.254.169.254)
/// 2. Restrict access so only traffic from the tenant's bridge can reach it
/// 3. DNAT metadata requests to the local metadata handler
pub fn setup_metadata_endpoint(tenant_id: &str, bridge_name: &str, gateway_ip: &str) -> Result<()> {
    let table_name = format!("mvm-meta-{}", tenant_id);

    shell::run_in_vm(&format!(
        r#"
        # Create nftables table for this tenant's metadata
        sudo nft add table ip {table}

        # Input chain: allow metadata requests from the tenant bridge
        sudo nft add chain ip {table} input {{ type filter hook input priority 0 \; }}
        sudo nft add rule ip {table} input iifname "{bridge}" tcp dport 8169 accept
        sudo nft add rule ip {table} input tcp dport 8169 drop

        # DNAT chain: redirect metadata requests to local handler
        sudo nft add chain ip {table} prerouting {{ type nat hook prerouting priority -100 \; }}
        sudo nft add rule ip {table} prerouting iifname "{bridge}" ip daddr {gw} tcp dport 8169 dnat to 127.0.0.1:8169
        "#,
        table = table_name,
        bridge = bridge_name,
        gw = gateway_ip,
    ))
    .with_context(|| format!("Failed to set up metadata endpoint for tenant {}", tenant_id))?;

    Ok(())
}

/// Tear down metadata endpoint for a tenant.
///
/// Removes the tenant-specific nftables table and all its rules.
pub fn teardown_metadata_endpoint(tenant_id: &str) -> Result<()> {
    let table_name = format!("mvm-meta-{}", tenant_id);

    shell::run_in_vm(&format!(
        "sudo nft delete table ip {} 2>/dev/null || true",
        table_name,
    ))?;

    Ok(())
}

/// Check if the metadata endpoint is configured for a tenant.
pub fn metadata_endpoint_active(tenant_id: &str) -> Result<bool> {
    let table_name = format!("mvm-meta-{}", tenant_id);

    let out = shell::run_in_vm_stdout(&format!(
        "sudo nft list table ip {} >/dev/null 2>&1 && echo yes || echo no",
        table_name,
    ))?;

    Ok(out.trim() == "yes")
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_metadata_table_naming() {
        let tenant_id = "acme";
        let table_name = format!("mvm-meta-{}", tenant_id);
        assert_eq!(table_name, "mvm-meta-acme");
    }
}
