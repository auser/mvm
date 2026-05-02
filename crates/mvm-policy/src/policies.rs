//! Sub-policy types referenced by `PolicyBundle`.
//!
//! Plan 37 Wave 1.2 lands the *shape* of each sub-policy as a
//! minimal placeholder. Real enforcement contracts arrive in later
//! waves:
//!
//! - Wave 2 fills `EgressPolicy` (L7 rules), `PiiPolicy` (detect /
//!   redact / refuse modes), and `ToolPolicy` (RPC allowlist).
//! - Wave 3 fills `KeyPolicy` (per-run secret grants) and
//!   `AuditPolicy` (chain signing, per-tenant streams).
//! - Wave 4 fills `NetworkPolicy` (per-tenant netns) and
//!   `ArtifactPolicy` retention sweeps.
//!
//! Every type uses `#[serde(deny_unknown_fields)]` so a future
//! field addition is a fail-closed schema bump for older verifiers,
//! and every type derives `Default` so `TenantOverlay`'s
//! `Option<T>` semantics ("None inherits from base") compose
//! cleanly with the bundle's resolution algorithm.

use serde::{Deserialize, Serialize};

/// Network policy. Wave 4 introduces per-tenant netns + bridge
/// allocation. Today: name-only stub matching the existing
/// `mvm-core::policy::network_policy` shape.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    /// Name of the network preset
    /// (`open` / `agent` / `tenant-isolated` / etc.). Stub.
    pub preset: Option<String>,
}

/// L7 egress policy. Plan 37 §15 differentiator. Wave 2.6 fills the
/// fields the `L7EgressProxy` actually consumes:
/// - `allow_list` is the (host, port) destination policy.
/// - `allow_plain_http` opens the plain-HTTP code path; **the
///   supervisor refuses to honour `true` for `Variant::Prod`** so
///   production workloads can never accidentally egress unencrypted.
/// - `body_cap_bytes` bounds the body read for plain-HTTP. `0` means
///   "use default" ([`DEFAULT_BODY_CAP_BYTES`], 16 MiB) — matches
///   AI-provider request sizes (long contexts + image uploads).
/// - `disabled_inspectors` lets operators turn off specific
///   inspectors by name (e.g., disable `pii_redactor` for an
///   analytics workload that scrubs upstream).
///
/// `mode` is retained for compatibility with Wave 1's stub; the
/// supervisor honours `mode = Some("open")` as a kill-switch that
/// skips the proxy entirely. New fields are `#[serde(default)]` so
/// older bundles continue to parse.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressPolicy {
    /// `open` — no proxy. Anything else routes through the L7 chain.
    pub mode: Option<String>,
    /// (host, port) allowlist consumed by `DestinationPolicy`.
    /// `port = 0` is the explicit "any port for this host" wildcard.
    #[serde(default)]
    pub allow_list: Vec<(String, u16)>,
    /// Whether plain HTTP (not just CONNECT/HTTPS) is permitted.
    /// **Forbidden for `Variant::Prod`** — the supervisor's
    /// `with_l7_egress` builder rejects this combination at policy
    /// load.
    #[serde(default)]
    pub allow_plain_http: bool,
    /// Body read cap for plain-HTTP (bytes). `0` means "use default"
    /// ([`DEFAULT_BODY_CAP_BYTES`]).
    #[serde(default)]
    pub body_cap_bytes: u64,
    /// Per-name inspector opt-out. Empty == every inspector enabled.
    /// Names match `Inspector::name()` strings: `destination_policy`,
    /// `ssrf_guard`, `secrets_scanner`, `injection_guard`,
    /// `pii_redactor`.
    #[serde(default)]
    pub disabled_inspectors: Vec<String>,
}

/// Default body cap when `EgressPolicy::body_cap_bytes` is 0.
/// 16 MiB — matches AI-provider request sizes (long contexts +
/// image uploads). Configurable per workload via the policy field.
pub const DEFAULT_BODY_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// PII redaction policy. Plan 37 §15.1.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiiPolicy {
    /// `disabled` / `detect` / `redact` / `refuse`. Stub.
    pub mode: Option<String>,
    /// Categories to act on (`email`, `cc_number`, `ssn`, ...).
    /// Empty means all categories the redactor knows about.
    pub categories: Vec<String>,
}

/// Tool-call allowlist. Plan 37 §2.2. Wave 2 wires the supervisor's
/// vsock RPC `ToolGate`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    /// Names of tools the workload is allowed to invoke. Stub.
    pub allowed: Vec<String>,
}

/// Artifact policy. Distinct from `mvm-plan::ArtifactPolicy` —
/// the plan field is a per-run snapshot; this is the bundle-side
/// source of truth that the supervisor's `ArtifactCollector` (Wave 3)
/// consults at workload exit.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPolicy {
    pub capture_paths: Vec<String>,
    pub retention_days: u32,
}

/// Key policy. Plan 37 §12. Wave 3 wires `KeystoreReleaser`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicy {
    /// 0 = no rotation; supervisor warns but accepts.
    pub rotation_interval_days: u32,
}

/// Audit policy. Plan 37 §22. Wave 3 wires chain signing + per-tenant
/// streams.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditPolicy {
    /// Whether the supervisor should chain-sign each entry into the
    /// previous's hash for tamper-evidence.
    pub chain_signing: bool,
    /// Per-tenant audit-stream destinations. Resolved by
    /// `AuditSigner` per Wave 3.
    pub stream_destinations: Vec<String>,
}
