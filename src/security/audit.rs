use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::infra::http;
use crate::infra::shell;
use crate::vm::tenant::config::tenant_audit_log_path;

/// Audit event types for per-tenant audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditAction {
    InstanceCreated,
    InstanceStarted,
    InstanceStopped,
    InstanceWarmed,
    InstanceSlept,
    InstanceWoken,
    InstanceDestroyed,
    PoolCreated,
    PoolBuilt,
    PoolDestroyed,
    TenantCreated,
    TenantDestroyed,
    QuotaExceeded,
    SecretsRotated,
    SnapshotCreated,
    SnapshotRestored,
}

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub tenant_id: String,
    pub pool_id: Option<String>,
    pub instance_id: Option<String>,
    pub action: AuditAction,
    pub detail: Option<String>,
}

/// Append an audit event to the tenant's audit log.
///
/// Each event is a single JSON line appended to `tenants/<tenant>/audit.log`.
/// The log is append-only — entries are never modified or deleted.
pub fn log_event(
    tenant_id: &str,
    pool_id: Option<&str>,
    instance_id: Option<&str>,
    action: AuditAction,
    detail: Option<&str>,
) -> Result<()> {
    let entry = AuditEntry {
        timestamp: http::utc_now(),
        tenant_id: tenant_id.to_string(),
        pool_id: pool_id.map(|s| s.to_string()),
        instance_id: instance_id.map(|s| s.to_string()),
        action,
        detail: detail.map(|s| s.to_string()),
    };

    let json_line =
        serde_json::to_string(&entry).with_context(|| "Failed to serialize audit entry")?;

    let log_path = tenant_audit_log_path(tenant_id);

    // Append JSON line atomically (>> is append, single write)
    shell::run_in_vm(&format!(
        "echo '{}' >> {}",
        json_line.replace('\'', "'\\''"),
        log_path,
    ))
    .with_context(|| format!("Failed to write audit log for tenant {}", tenant_id))?;

    Ok(())
}

/// Read the last N audit log entries for a tenant.
pub fn read_audit_log(tenant_id: &str, last_n: usize) -> Result<Vec<AuditEntry>> {
    let log_path = tenant_audit_log_path(tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        "tail -n {} {} 2>/dev/null || true",
        last_n, log_path
    ))?;

    let mut entries = Vec::new();
    for line in output.lines().filter(|l| !l.is_empty()) {
        if let Ok(entry) = serde_json::from_str::<AuditEntry>(line) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_entry_serialization() {
        let entry = AuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant_id: "acme".to_string(),
            pool_id: Some("workers".to_string()),
            instance_id: Some("i-abc123".to_string()),
            action: AuditAction::InstanceStarted,
            detail: Some("pid=12345".to_string()),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"tenant_id\":\"acme\""));
        assert!(json.contains("\"InstanceStarted\""));
        assert!(json.contains("pid=12345"));
    }

    #[test]
    fn test_audit_entry_no_optionals() {
        let entry = AuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant_id: "acme".to_string(),
            pool_id: None,
            instance_id: None,
            action: AuditAction::TenantCreated,
            detail: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"pool_id\":null"));
        assert!(json.contains("\"TenantCreated\""));
    }

    #[test]
    fn test_all_audit_actions_serialize() {
        let actions = vec![
            AuditAction::InstanceCreated,
            AuditAction::InstanceStarted,
            AuditAction::InstanceStopped,
            AuditAction::InstanceWarmed,
            AuditAction::InstanceSlept,
            AuditAction::InstanceWoken,
            AuditAction::InstanceDestroyed,
            AuditAction::PoolCreated,
            AuditAction::PoolBuilt,
            AuditAction::PoolDestroyed,
            AuditAction::TenantCreated,
            AuditAction::TenantDestroyed,
            AuditAction::QuotaExceeded,
            AuditAction::SecretsRotated,
            AuditAction::SnapshotCreated,
            AuditAction::SnapshotRestored,
        ];

        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert!(!json.is_empty());
        }
    }
}
