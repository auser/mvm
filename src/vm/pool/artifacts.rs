use anyhow::Result;

use super::config::{BuildRevision, pool_artifacts_dir};
use crate::infra::shell;

/// Get the current active revision hash for a pool.
pub fn current_revision(tenant_id: &str, pool_id: &str) -> Result<Option<String>> {
    let link = format!("{}/current", pool_artifacts_dir(tenant_id, pool_id));
    let output = shell::run_in_vm_stdout(&format!("readlink {} 2>/dev/null || echo ''", link))?;
    let target = output.trim();
    if target.is_empty() {
        return Ok(None);
    }
    // Extract hash from "revisions/<hash>"
    Ok(target.strip_prefix("revisions/").map(|s| s.to_string()))
}

/// Record a new build revision and update the current symlink.
pub fn record_revision(tenant_id: &str, pool_id: &str, revision: &BuildRevision) -> Result<()> {
    let artifacts_dir = pool_artifacts_dir(tenant_id, pool_id);
    let rev_dir = format!("{}/revisions/{}", artifacts_dir, revision.revision_hash);

    shell::run_in_vm(&format!("mkdir -p {}", rev_dir))?;

    // Update current symlink atomically
    shell::run_in_vm(&format!(
        "ln -snf revisions/{} {}/current",
        revision.revision_hash, artifacts_dir
    ))?;

    Ok(())
}

/// Rollback to a previous revision.
pub fn rollback(tenant_id: &str, pool_id: &str, revision_hash: &str) -> Result<()> {
    let artifacts_dir = pool_artifacts_dir(tenant_id, pool_id);
    let rev_dir = format!("{}/revisions/{}", artifacts_dir, revision_hash);

    // Verify revision exists
    let exists = shell::run_in_vm_stdout(&format!("test -d {} && echo yes || echo no", rev_dir))?;

    if exists.trim() != "yes" {
        anyhow::bail!("Revision {} not found", revision_hash);
    }

    shell::run_in_vm(&format!(
        "ln -snf revisions/{} {}/current",
        revision_hash, artifacts_dir
    ))?;

    Ok(())
}
