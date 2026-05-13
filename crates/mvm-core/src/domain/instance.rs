use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::idle_metrics::IdleMetrics;
use crate::pool::Role;

// ============================================================================
// Workspace volume attach + workload classification (cross-repo with mvmd)
// ============================================================================
//
// These types are the canonical mvm-core definitions for the workspace
// volume attach surface introduced in mvmd Phase 1057/1058 (plan 32 —
// `mvmd-integrations` memory service). They were defined locally in
// `mvmd-runtime` first and are promoted here so the protocol types can
// thread them without a circular dep. mvmd will drop its local copies
// and re-export from `mvm_core::instance` once it bumps its mvm pin.

/// Read/write mode for a workspace volume attached to an instance.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VolumeMode {
    ReadOnly,
    ReadWrite,
}

/// Request to attach a workspace-scoped volume into an instance at start.
///
/// Identity is `(workspace_id, name)`; the on-host backing file lives at
/// `/var/lib/mvm/workspaces/<workspace_id>/volumes/<name>.ext4` (mvmd's
/// `mvmd_runtime::vm::workspace::volumes` owns the layout). `mount_path`
/// is threaded through to the guest config-drive metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeAttach {
    /// Workspace that owns the volume.
    pub workspace_id: String,
    /// Volume name within the workspace (file stem of `<name>.ext4`).
    pub name: String,
    /// Mount path inside the guest.
    pub mount_path: String,
    pub mode: VolumeMode,
}

/// Workload class — drives sleep policy, auto-provision rules, and
/// resource defaults. Sandbox is the user-controlled ephemeral default;
/// Service is workspace-owned, auto-provisioned, long-running (e.g. the
/// per-workspace memory service).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    /// User-controlled, ephemeral. Default for sandboxes.
    #[default]
    Sandbox,
    /// Auto-provisioned, workspace-owned, long-running.
    Service,
}

// ============================================================================
// Desired instance state (declarative target the reconciler drives toward)
// ============================================================================

/// Declarative per-instance desired state.
///
/// Carries identity + the workspace/volume/class metadata that the
/// scheduler and `mvm-hostd` need to materialize an instance. Distinct
/// from [`InstanceState`], which is the *observed* runtime state.
///
/// Backward compatibility: every field added after the initial shape
/// MUST carry `#[serde(default)]` so older serialized payloads keep
/// deserializing cleanly. Tested via
/// `test_desired_instance_backward_compat`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredInstance {
    pub instance_id: String,
    pub pool_id: String,
    pub tenant_id: String,
    /// Workspace this instance belongs to.
    /// Required for service-class workloads; optional for sandbox-class
    /// during migration (None = workspace-unknown, treated as
    /// tenant-only).
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Workspace-scoped volumes to attach at instance start.
    #[serde(default)]
    pub volumes: Vec<VolumeAttach>,
    /// Workload class — drives sleep policy, auto-provision rules, etc.
    #[serde(default)]
    pub workload_class: WorkloadClass,
}

/// Instance lifecycle status. Only instances have runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Created,
    Ready,
    Running,
    Warm,
    Sleeping,
    Stopped,
    Destroyed,
}

impl std::fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Ready => write!(f, "ready"),
            Self::Running => write!(f, "running"),
            Self::Warm => write!(f, "warm"),
            Self::Sleeping => write!(f, "sleeping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Destroyed => write!(f, "destroyed"),
        }
    }
}

/// Per-instance network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceNet {
    /// TAP device name: "tn<net_id>i<offset>"
    pub tap_dev: String,
    /// Deterministic MAC: "02:xx:xx:xx:xx:xx"
    pub mac: String,
    /// Guest IP within tenant subnet, e.g. "10.240.3.5"
    pub guest_ip: String,
    /// Tenant gateway, e.g. "10.240.3.1"
    pub gateway_ip: String,
    /// CIDR prefix length from tenant subnet, e.g. 24
    pub cidr: u8,
}

/// Full instance state, persisted at instances/<id>/instance.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceState {
    pub instance_id: String,
    pub pool_id: String,
    pub tenant_id: String,
    pub status: InstanceStatus,
    pub net: InstanceNet,
    /// Role inherited from pool at creation time.
    #[serde(default)]
    pub role: Role,
    pub revision_hash: Option<String>,
    pub firecracker_pid: Option<u32>,
    pub last_started_at: Option<String>,
    pub last_stopped_at: Option<String>,
    #[serde(default)]
    pub idle_metrics: IdleMetrics,
    pub healthy: Option<bool>,
    pub last_health_check_at: Option<String>,
    pub manual_override_until: Option<String>,
    /// Config drive version currently mounted.
    #[serde(default)]
    pub config_version: Option<u64>,
    /// Secrets epoch currently mounted.
    #[serde(default)]
    pub secrets_epoch: Option<u64>,
    /// Timestamp when instance entered Running status.
    #[serde(default)]
    pub entered_running_at: Option<String>,
    /// Timestamp when instance entered Warm status.
    #[serde(default)]
    pub entered_warm_at: Option<String>,
    /// Timestamp of last work activity (from guest agent or metrics).
    #[serde(default)]
    pub last_busy_at: Option<String>,
    /// Caller-supplied metadata. Tenant-controlled; validated via
    /// `mvm_security::policy::InputValidator`. Echoed in audit events
    /// and webhook bodies, so charset/length constraints are
    /// load-bearing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// RFC 3339 wall-clock time after which the supervisor reaper will
    /// tear this instance down. `None` = no TTL.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// When `true` (the default), connecting to a `Sleeping` instance
    /// auto-resumes it. When `false`, callers must `mvmctl resume`
    /// explicitly.
    #[serde(default = "default_auto_resume")]
    pub auto_resume: bool,
}

fn default_auto_resume() -> bool {
    true
}

/// Validate that a state transition is allowed.
///
/// Returns Ok(()) if the transition is valid, Err with explanation otherwise.
/// Enforces the state machine defined in the spec.
pub fn validate_transition(from: InstanceStatus, to: InstanceStatus) -> Result<()> {
    // Any state -> Destroyed is always allowed
    if to == InstanceStatus::Destroyed {
        return Ok(());
    }

    let valid = matches!(
        (from, to),
        // Build completes
        (InstanceStatus::Created, InstanceStatus::Ready)
        // Start
        | (InstanceStatus::Ready, InstanceStatus::Running)
        // Pause vCPUs
        | (InstanceStatus::Running, InstanceStatus::Warm)
        // Stop from running
        | (InstanceStatus::Running, InstanceStatus::Stopped)
        // Resume from warm
        | (InstanceStatus::Warm, InstanceStatus::Running)
        // Snapshot + shutdown
        | (InstanceStatus::Warm, InstanceStatus::Sleeping)
        // Stop from warm
        | (InstanceStatus::Warm, InstanceStatus::Stopped)
        // Wake from snapshot
        | (InstanceStatus::Sleeping, InstanceStatus::Running)
        // Stop from sleeping (discard snapshot)
        | (InstanceStatus::Sleeping, InstanceStatus::Stopped)
        // Fresh boot from stopped
        | (InstanceStatus::Stopped, InstanceStatus::Running)
        // Rebuild
        | (InstanceStatus::Ready, InstanceStatus::Ready)
    );

    if valid {
        Ok(())
    } else {
        bail!("Invalid state transition: {} -> {}", from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_transitions() {
        assert!(validate_transition(InstanceStatus::Created, InstanceStatus::Ready).is_ok());
        assert!(validate_transition(InstanceStatus::Ready, InstanceStatus::Running).is_ok());
        assert!(validate_transition(InstanceStatus::Running, InstanceStatus::Warm).is_ok());
        assert!(validate_transition(InstanceStatus::Running, InstanceStatus::Stopped).is_ok());
        assert!(validate_transition(InstanceStatus::Warm, InstanceStatus::Running).is_ok());
        assert!(validate_transition(InstanceStatus::Warm, InstanceStatus::Sleeping).is_ok());
        assert!(validate_transition(InstanceStatus::Warm, InstanceStatus::Stopped).is_ok());
        assert!(validate_transition(InstanceStatus::Sleeping, InstanceStatus::Running).is_ok());
        assert!(validate_transition(InstanceStatus::Sleeping, InstanceStatus::Stopped).is_ok());
        assert!(validate_transition(InstanceStatus::Stopped, InstanceStatus::Running).is_ok());
        assert!(validate_transition(InstanceStatus::Ready, InstanceStatus::Ready).is_ok());
    }

    #[test]
    fn test_destroyed_from_any() {
        for status in [
            InstanceStatus::Created,
            InstanceStatus::Ready,
            InstanceStatus::Running,
            InstanceStatus::Warm,
            InstanceStatus::Sleeping,
            InstanceStatus::Stopped,
        ] {
            assert!(
                validate_transition(status, InstanceStatus::Destroyed).is_ok(),
                "{} -> Destroyed should be valid",
                status,
            );
        }
    }

    #[test]
    fn test_invalid_transitions() {
        assert!(validate_transition(InstanceStatus::Created, InstanceStatus::Running).is_err());
        assert!(validate_transition(InstanceStatus::Created, InstanceStatus::Warm).is_err());
        assert!(validate_transition(InstanceStatus::Running, InstanceStatus::Sleeping).is_err());
        assert!(validate_transition(InstanceStatus::Sleeping, InstanceStatus::Warm).is_err());
        assert!(validate_transition(InstanceStatus::Stopped, InstanceStatus::Warm).is_err());
        assert!(validate_transition(InstanceStatus::Stopped, InstanceStatus::Sleeping).is_err());
    }

    #[test]
    fn test_instance_state_json_roundtrip() {
        let state = InstanceState {
            instance_id: "i-a3f7b2c1".to_string(),
            pool_id: "workers".to_string(),
            tenant_id: "acme".to_string(),
            status: InstanceStatus::Running,
            net: InstanceNet {
                tap_dev: "tn3i5".to_string(),
                mac: "02:fc:00:03:00:05".to_string(),
                guest_ip: "10.240.3.5".to_string(),
                gateway_ip: "10.240.3.1".to_string(),
                cidr: 24,
            },
            role: Role::Gateway,
            revision_hash: Some("abc123".to_string()),
            firecracker_pid: Some(12345),
            last_started_at: Some("2025-01-01T00:00:00Z".to_string()),
            last_stopped_at: None,
            idle_metrics: IdleMetrics::default(),
            healthy: Some(true),
            last_health_check_at: None,
            manual_override_until: None,
            config_version: Some(3),
            secrets_epoch: Some(1),
            entered_running_at: Some("2025-01-01T00:00:00Z".to_string()),
            entered_warm_at: None,
            last_busy_at: None,
            tags: {
                let mut m = BTreeMap::new();
                m.insert("job".to_string(), "etl".to_string());
                m
            },
            expires_at: Some("2025-01-02T00:00:00Z".to_string()),
            auto_resume: false,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: InstanceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.instance_id, "i-a3f7b2c1");
        assert_eq!(parsed.status, InstanceStatus::Running);
        assert_eq!(parsed.net.tap_dev, "tn3i5");
        assert_eq!(parsed.role, Role::Gateway);
        assert_eq!(parsed.config_version, Some(3));
        assert_eq!(
            parsed.entered_running_at.as_deref(),
            Some("2025-01-01T00:00:00Z")
        );
        assert_eq!(parsed.tags.get("job").map(String::as_str), Some("etl"));
        assert_eq!(parsed.expires_at.as_deref(), Some("2025-01-02T00:00:00Z"));
        assert!(!parsed.auto_resume);
    }

    #[test]
    fn test_instance_state_backward_compat() {
        // JSON without new fields should deserialize with defaults
        let json = r#"{
            "instance_id": "i-test",
            "pool_id": "workers",
            "tenant_id": "acme",
            "status": "running",
            "net": {
                "tap_dev": "tn3i5",
                "mac": "02:fc:00:03:00:05",
                "guest_ip": "10.240.3.5",
                "gateway_ip": "10.240.3.1",
                "cidr": 24
            },
            "revision_hash": null,
            "firecracker_pid": null,
            "last_started_at": null,
            "last_stopped_at": null,
            "healthy": null,
            "last_health_check_at": null,
            "manual_override_until": null
        }"#;
        let parsed: InstanceState = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.role, Role::Worker);
        assert_eq!(parsed.config_version, None);
        assert_eq!(parsed.secrets_epoch, None);
        assert_eq!(parsed.entered_running_at, None);
        assert_eq!(parsed.entered_warm_at, None);
        assert_eq!(parsed.last_busy_at, None);
        // The new sandbox-SDK fields default cleanly on legacy records:
        // tags is empty, no TTL, auto_resume defaults to true so existing
        // callers see the documented "connect wakes a sleeper" semantic.
        assert!(parsed.tags.is_empty());
        assert_eq!(parsed.expires_at, None);
        assert!(parsed.auto_resume);
    }

    #[test]
    fn test_status_display() {
        assert_eq!(InstanceStatus::Running.to_string(), "running");
        assert_eq!(InstanceStatus::Sleeping.to_string(), "sleeping");
        assert_eq!(InstanceStatus::Destroyed.to_string(), "destroyed");
    }

    // ------------- VolumeMode / VolumeAttach / WorkloadClass -------------

    #[test]
    fn test_volume_mode_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&VolumeMode::ReadOnly).unwrap(),
            "\"read_only\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeMode::ReadWrite).unwrap(),
            "\"read_write\""
        );
        let parsed: VolumeMode = serde_json::from_str("\"read_only\"").unwrap();
        assert_eq!(parsed, VolumeMode::ReadOnly);
    }

    #[test]
    fn test_workload_class_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&WorkloadClass::Sandbox).unwrap(),
            "\"sandbox\""
        );
        assert_eq!(
            serde_json::to_string(&WorkloadClass::Service).unwrap(),
            "\"service\""
        );
    }

    #[test]
    fn test_workload_class_default_is_sandbox() {
        assert_eq!(WorkloadClass::default(), WorkloadClass::Sandbox);
    }

    #[test]
    fn test_volume_attach_roundtrip() {
        let attach = VolumeAttach {
            workspace_id: "ws-prod".to_string(),
            name: "memory".to_string(),
            mount_path: "/var/lib/memory".to_string(),
            mode: VolumeMode::ReadWrite,
        };
        let json = serde_json::to_string(&attach).unwrap();
        let parsed: VolumeAttach = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, attach);
    }

    // ------------- DesiredInstance -------------

    #[test]
    fn test_desired_instance_roundtrip_full() {
        let di = DesiredInstance {
            instance_id: "i-abc123".to_string(),
            pool_id: "memory-svc".to_string(),
            tenant_id: "acme".to_string(),
            workspace_id: Some("ws-prod".to_string()),
            volumes: vec![VolumeAttach {
                workspace_id: "ws-prod".to_string(),
                name: "memory".to_string(),
                mount_path: "/var/lib/memory".to_string(),
                mode: VolumeMode::ReadWrite,
            }],
            workload_class: WorkloadClass::Service,
        };
        let json = serde_json::to_string(&di).unwrap();
        let parsed: DesiredInstance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, di);
    }

    #[test]
    fn test_desired_instance_backward_compat() {
        // Pre-workspace JSON (no workspace_id, volumes, workload_class)
        // must still deserialize with defaults — sandbox-class sandboxes
        // produced before Phase 1058 land here.
        let json = r#"{
            "instance_id": "i-legacy",
            "pool_id": "workers",
            "tenant_id": "acme"
        }"#;
        let parsed: DesiredInstance = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.instance_id, "i-legacy");
        assert_eq!(parsed.workspace_id, None);
        assert!(parsed.volumes.is_empty());
        assert_eq!(parsed.workload_class, WorkloadClass::Sandbox);
    }

    #[test]
    fn test_desired_instance_partial_compat() {
        // Mid-migration payload: workspace_id present, but no volumes
        // and no workload_class — should default sensibly.
        let json = r#"{
            "instance_id": "i-mid",
            "pool_id": "workers",
            "tenant_id": "acme",
            "workspace_id": "ws-prod"
        }"#;
        let parsed: DesiredInstance = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.workspace_id.as_deref(), Some("ws-prod"));
        assert!(parsed.volumes.is_empty());
        assert_eq!(parsed.workload_class, WorkloadClass::Sandbox);
    }
}
