use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Attestation report produced by an attestation provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationReport {
    /// Node identifier.
    pub node_id: String,
    /// Timestamp of attestation (RFC 3339).
    pub timestamp: String,
    /// Opaque attestation evidence (provider-specific).
    pub evidence: Vec<u8>,
    /// Provider type: "none", "tpm2", "sev-snp", "tdx", etc.
    pub provider: String,
}

/// Trait for node attestation providers.
///
/// Extension point for TPM2, AMD SEV-SNP, Intel TDX, or other
/// hardware attestation mechanisms. Implement this trait to plug
/// in platform-specific attestation.
pub trait AttestationProvider: Send + Sync {
    /// Produce an attestation report for this node.
    fn attest_node(&self) -> Result<AttestationReport>;

    /// Provider name (e.g., "none", "tpm2", "sev-snp").
    fn provider_name(&self) -> &str;
}

/// No-op attestation provider for environments without hardware attestation.
pub struct NoopAttestationProvider;

impl AttestationProvider for NoopAttestationProvider {
    fn attest_node(&self) -> Result<AttestationReport> {
        Ok(AttestationReport {
            node_id: String::new(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            evidence: Vec::new(),
            provider: "none".to_string(),
        })
    }

    fn provider_name(&self) -> &str {
        "none"
    }
}

/// Get the default attestation provider.
///
/// Currently always returns NoopAttestationProvider.
/// When hardware attestation is available, this will detect
/// TPM2/SEV-SNP/TDX and return the appropriate provider.
pub fn default_provider() -> Box<dyn AttestationProvider> {
    Box::new(NoopAttestationProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_provider_returns_valid_report() {
        let provider = NoopAttestationProvider;
        let report = provider.attest_node().unwrap();
        assert_eq!(report.provider, "none");
        assert!(report.evidence.is_empty());
        assert!(!report.timestamp.is_empty());
    }

    #[test]
    fn test_noop_provider_name() {
        let provider = NoopAttestationProvider;
        assert_eq!(provider.provider_name(), "none");
    }

    #[test]
    fn test_attestation_report_roundtrip() {
        let report = AttestationReport {
            node_id: "node-1".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            evidence: vec![0xDE, 0xAD],
            provider: "tpm2".to_string(),
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: AttestationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.node_id, "node-1");
        assert_eq!(parsed.provider, "tpm2");
        assert_eq!(parsed.evidence, vec![0xDE, 0xAD]);
    }

    #[test]
    fn test_default_provider_is_noop() {
        let provider = default_provider();
        assert_eq!(provider.provider_name(), "none");
    }
}
