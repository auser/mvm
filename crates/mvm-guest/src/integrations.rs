use serde::{Deserialize, Serialize};

// ============================================================================
// Integration state model — structured layout for OpenClaw integration
// session state on the data disk. The guest agent checkpoints integration
// state before sleep and restores it on wake.
// ============================================================================

/// Base path inside the guest where integration state is stored.
pub const INTEGRATIONS_BASE_PATH: &str = "/data/integrations";

/// Manifest listing active integrations for an instance.
/// Written to the config drive so the guest agent knows which
/// integrations to manage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationManifest {
    pub integrations: Vec<IntegrationEntry>,
}

/// An individual integration to manage on this instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationEntry {
    /// Integration name (e.g. "whatsapp", "telegram", "slack", "imessage").
    /// Used as the directory name under /data/integrations/.
    pub name: String,
    /// Optional command to run before sleep to checkpoint state.
    /// If None, the integration manager only ensures state dirs exist.
    #[serde(default)]
    pub checkpoint_cmd: Option<String>,
    /// Optional command to run after wake to restore state.
    #[serde(default)]
    pub restore_cmd: Option<String>,
    /// If true, sleep is blocked until this integration successfully checkpoints.
    /// If false, checkpoint failure is logged but sleep proceeds.
    #[serde(default)]
    pub critical: bool,
}

/// Runtime status of a single integration on a guest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// Integration is running and connected.
    Active,
    /// Integration is paused (e.g. during checkpoint).
    Paused,
    /// Integration has an error.
    Error(String),
    /// Integration is not yet initialized.
    #[default]
    Pending,
}

/// Full state report for a single integration (returned by guest agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationStateReport {
    pub name: String,
    pub status: IntegrationStatus,
    /// ISO timestamp of last successful checkpoint.
    #[serde(default)]
    pub last_checkpoint_at: Option<String>,
    /// Bytes of state data on disk.
    #[serde(default)]
    pub state_size_bytes: u64,
}

impl IntegrationManifest {
    /// Parse from JSON.
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

/// Generate the guest-side state directory path for an integration.
pub fn integration_state_dir(name: &str) -> String {
    format!("{}/{}/state", INTEGRATIONS_BASE_PATH, name)
}

/// Generate the guest-side checkpoint marker path for an integration.
pub fn integration_checkpoint_path(name: &str) -> String {
    format!("{}/{}/checkpoint", INTEGRATIONS_BASE_PATH, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_serde_roundtrip() {
        let manifest = IntegrationManifest {
            integrations: vec![
                IntegrationEntry {
                    name: "whatsapp".to_string(),
                    checkpoint_cmd: Some("/opt/openclaw/bin/whatsapp-checkpoint".to_string()),
                    restore_cmd: Some("/opt/openclaw/bin/whatsapp-restore".to_string()),
                    critical: true,
                },
                IntegrationEntry {
                    name: "slack".to_string(),
                    checkpoint_cmd: None,
                    restore_cmd: None,
                    critical: false,
                },
            ],
        };

        let json = manifest.to_json().unwrap();
        let parsed = IntegrationManifest::from_json(&json).unwrap();
        assert_eq!(parsed.integrations.len(), 2);
        assert_eq!(parsed.integrations[0].name, "whatsapp");
        assert!(parsed.integrations[0].critical);
        assert!(parsed.integrations[0].checkpoint_cmd.is_some());
        assert_eq!(parsed.integrations[1].name, "slack");
        assert!(!parsed.integrations[1].critical);
    }

    #[test]
    fn test_empty_manifest() {
        let manifest = IntegrationManifest::default();
        let json = manifest.to_json().unwrap();
        let parsed = IntegrationManifest::from_json(&json).unwrap();
        assert!(parsed.integrations.is_empty());
    }

    #[test]
    fn test_integration_status_serde() {
        let variants = vec![
            (IntegrationStatus::Active, "\"active\""),
            (IntegrationStatus::Paused, "\"paused\""),
            (IntegrationStatus::Pending, "\"pending\""),
            (
                IntegrationStatus::Error("conn lost".to_string()),
                "{\"error\":\"conn lost\"}",
            ),
        ];

        for (status, expected) in &variants {
            let json = serde_json::to_string(status).unwrap();
            assert_eq!(&json, expected);
            let parsed: IntegrationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, status);
        }
    }

    #[test]
    fn test_integration_state_report_roundtrip() {
        let report = IntegrationStateReport {
            name: "telegram".to_string(),
            status: IntegrationStatus::Active,
            last_checkpoint_at: Some("2025-06-01T12:00:00Z".to_string()),
            state_size_bytes: 4096,
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: IntegrationStateReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "telegram");
        assert_eq!(parsed.status, IntegrationStatus::Active);
        assert_eq!(parsed.state_size_bytes, 4096);
    }

    #[test]
    fn test_state_dir_paths() {
        assert_eq!(
            integration_state_dir("whatsapp"),
            "/data/integrations/whatsapp/state"
        );
        assert_eq!(
            integration_checkpoint_path("telegram"),
            "/data/integrations/telegram/checkpoint"
        );
    }

    #[test]
    fn test_integration_status_default() {
        assert_eq!(IntegrationStatus::default(), IntegrationStatus::Pending);
    }

    #[test]
    fn test_manifest_backward_compat() {
        // JSON without optional fields should parse fine
        let json = r#"{"integrations": [{"name": "signal"}]}"#;
        let parsed = IntegrationManifest::from_json(json).unwrap();
        assert_eq!(parsed.integrations.len(), 1);
        assert_eq!(parsed.integrations[0].name, "signal");
        assert!(parsed.integrations[0].checkpoint_cmd.is_none());
        assert!(!parsed.integrations[0].critical);
    }
}
