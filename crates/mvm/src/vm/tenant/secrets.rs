use anyhow::Result;

use crate::shell;
use mvm_core::tenant::tenant_secrets_path;

/// Set tenant secrets from a JSON file.
pub fn secrets_set(tenant_id: &str, from_file: &str) -> Result<()> {
    let dest = tenant_secrets_path(tenant_id);
    shell::run_in_vm(&format!("cp {} {}", from_file, dest))?;
    Ok(())
}

/// Rotate tenant secrets (bumps epoch, running instances get new secrets on restart).
pub fn secrets_rotate(tenant_id: &str) -> Result<()> {
    let _ = tenant_id;
    // Rotation updates secrets_epoch in TenantConfig and regenerates secrets disk on next instance start.
    // Full implementation in Phase 5 with disk.rs.
    Ok(())
}
