//! Plan 60 Phase 4 — single audit `Recorder` substrate.
//!
//! Phase 4's framing: "Audit instrumentation lives in a single
//! `mvm-supervisor::audit::Recorder` that every other crate calls
//! — there is one audit path, not many." This module is that
//! unifier.
//!
//! Before Phase 4 there were three independent audit streams:
//!
//! - **`~/.mvm/audit/<tenant>.jsonl`** — plan-bound chain-signed
//!   events (`plan.admitted`, `plan.launched`, `plan.failed`)
//!   from plan 64. Shape: `AuditEntry` carries
//!   `plan_id`/`plan_version`/`image_*` mandatorily.
//! - **`~/.mvm/audit/secrets.jsonl`** — operator `mvmctl secret`
//!   CRUD audit from plan 63 W4. Ad-hoc JSON shape.
//! - **`~/.mvm/log/audit.jsonl`** — legacy LocalAudit stream
//!   (pre-plan-64; `mvmctl audit tail` defaults to this).
//!
//! Phase 4 unifies them through a typed [`EventCategory`] taxonomy
//! and a [`Recorder`] that wraps an [`crate::AuditSigner`] +
//! routes every emit through the same chain-signed envelope. The
//! per-stream files stay distinct on disk (operator-secret events
//! shouldn't pollute the plan chain), but the **emit surface** is
//! one type, one trait, one place to grep.
//!
//! ## Categories (9)
//!
//! Per ADR-002 + plan 60 §"Comprehensive audit catalog" (the
//! mvmctl audit tail's `cat` filter):
//!
//! | Category | Examples | Plan-bound? |
//! |---|---|---|
//! | [`EventCategory::Cmd`] | `cmd.up.invoked`, `cmd.template.built` | no |
//! | [`EventCategory::Lifecycle`] | `lifecycle.instance.created`, `lifecycle.instance.stopped` | usually |
//! | [`EventCategory::Secret`] | `secret.put`, `secret.get`, `secret.delete` | no |
//! | [`EventCategory::Flow`] | `flow.egress.allowed`, `flow.egress.denied` | yes |
//! | [`EventCategory::Plan`] | `plan.admitted`, `plan.launched`, `plan.failed` | yes |
//! | [`EventCategory::Policy`] | `policy.loaded`, `policy.refused` | partial |
//! | [`EventCategory::Key`] | `key.rotated`, `key.released` | partial |
//! | [`EventCategory::Host`] | `host.started`, `host.shutdown` | no |
//! | [`EventCategory::Audit`] | `audit.chain.verified`, `audit.chain.broken` | meta |
//!
//! "Plan-bound" categories MUST carry a `plan_id`/`plan_version`;
//! [`Recorder::record_plan_bound`] enforces this. Plan-less
//! categories use [`Recorder::record_unbound`] which constructs
//! an envelope with sentinel plan-id values.

use std::collections::BTreeMap;
use std::sync::Arc;

use mvm_plan::{ExecutionPlan, PlanId, TenantId};
use mvm_policy::PolicyBundle;

use crate::audit::{AuditEntry, AuditError, AuditSigner};

/// Canonical audit-event categories. The string form (the
/// `event_name` field's prefix) is the wire-stable identifier
/// downstream consumers grep against.
///
/// Adding a new category is a wire-format extension — bump the
/// `mvm_core::protocol::PROTOCOL_VERSION` if a consumer's
/// fixture set would break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCategory {
    /// CLI command invocations. `cmd.<verb>.<outcome>`.
    Cmd,
    /// VM / instance state transitions. `lifecycle.<resource>.<verb>`.
    Lifecycle,
    /// Secret CRUD via `mvmctl secret`. `secret.<verb>`.
    Secret,
    /// Network flow attempts (L4/L7 proxy + firewall).
    /// `flow.<layer>.<verb>`.
    Flow,
    /// ExecutionPlan lifecycle. `plan.<verb>`.
    Plan,
    /// Policy bundle operations. `policy.<verb>`.
    Policy,
    /// Encryption key rotation / release. `key.<verb>`.
    Key,
    /// Supervisor / mvm-hostd lifecycle. `host.<verb>`.
    Host,
    /// Meta-events about the audit stream itself. `audit.<verb>`.
    /// Used by `mvmctl audit verify` results, chain-rotation
    /// announcements, etc.
    Audit,
}

impl EventCategory {
    /// Canonical prefix string for event names in this category.
    /// Used both as the prefix on `event_name` (e.g. `plan.admitted`)
    /// and as the value of the `category` label so downstream
    /// indexers can `WHERE labels.category = "plan"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cmd => "cmd",
            Self::Lifecycle => "lifecycle",
            Self::Secret => "secret",
            Self::Flow => "flow",
            Self::Plan => "plan",
            Self::Policy => "policy",
            Self::Key => "key",
            Self::Host => "host",
            Self::Audit => "audit",
        }
    }

    /// Whether the category mandates plan context. Plan-bound
    /// categories error if recorded via [`Recorder::record_unbound`].
    /// Today: `plan` and `flow` are mandatorily plan-bound;
    /// `lifecycle`, `policy`, and `key` are *usually* plan-bound
    /// but allowed unbound for some lifecycle events; the rest
    /// are unbound.
    pub fn requires_plan_context(self) -> bool {
        matches!(self, Self::Plan | Self::Flow)
    }
}

impl std::fmt::Display for EventCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors the recorder surfaces beyond the underlying [`AuditError`].
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("category {category} requires plan context; use record_plan_bound")]
    MissingPlanContext { category: EventCategory },

    #[error(
        "event_name {got:?} doesn't start with category prefix {expected_prefix:?}.\
         (events must use the canonical prefix so consumers can filter by category.)"
    )]
    EventPrefixMismatch {
        got: String,
        expected_prefix: &'static str,
    },

    #[error(transparent)]
    Signer(#[from] AuditError),
}

/// Unified audit-event recorder. Owns an `Arc<dyn AuditSigner>`
/// (the chain-signing piece from plan 64 W4); every emit routes
/// through it. Cheap to clone; share one recorder across every
/// emit site in a binary.
#[derive(Clone)]
pub struct Recorder {
    signer: Arc<dyn AuditSigner>,
    /// Tenant used for unbound events (host / cmd / audit). Plan-
    /// bound events read tenant from the plan; this fallback is
    /// only consulted when no plan is in scope.
    default_tenant: TenantId,
}

impl Recorder {
    /// Build with an explicit signer + default tenant. The default
    /// tenant feeds unbound events (host startup, audit-verify
    /// outcomes); plan-bound events inherit from the plan.
    pub fn new(signer: Arc<dyn AuditSigner>, default_tenant: TenantId) -> Self {
        Self {
            signer,
            default_tenant,
        }
    }

    /// Emit a plan-bound event. The category's prefix must match
    /// the start of `event_name`. Returns the underlying
    /// [`AuditError`] if the signer's `sign_and_emit` fails.
    ///
    /// Examples:
    /// ```ignore
    /// recorder.record_plan_bound(
    ///     EventCategory::Plan,
    ///     "plan.admitted",
    ///     &plan,
    ///     None,
    ///     [("signer_id".to_string(), "host:localhost".to_string())],
    /// ).await?;
    /// ```
    pub async fn record_plan_bound(
        &self,
        category: EventCategory,
        event_name: impl Into<String>,
        plan: &ExecutionPlan,
        bundle: Option<&PolicyBundle>,
        extras: impl IntoIterator<Item = (String, String)>,
    ) -> Result<(), RecorderError> {
        let event_name = event_name.into();
        validate_event_prefix(category, &event_name)?;
        let merged = merge_extras(category, extras);
        let entry = AuditEntry::for_plan(plan, bundle, event_name, merged);
        self.signer.sign_and_emit(&entry).await?;
        Ok(())
    }

    /// Emit an event without plan context. Refuses if the
    /// category is `requires_plan_context()`. Useful for `cmd`,
    /// `host`, `secret`, `audit` events.
    ///
    /// Sentinel `plan_id` / `plan_version` / `image_*` values let
    /// the entry flow through the same chain-signer without
    /// requiring a plan to exist.
    pub async fn record_unbound(
        &self,
        category: EventCategory,
        event_name: impl Into<String>,
        extras: impl IntoIterator<Item = (String, String)>,
    ) -> Result<(), RecorderError> {
        if category.requires_plan_context() {
            return Err(RecorderError::MissingPlanContext { category });
        }
        let event_name = event_name.into();
        validate_event_prefix(category, &event_name)?;
        let entry = self.unbound_entry(category, event_name, extras);
        self.signer.sign_and_emit(&entry).await?;
        Ok(())
    }

    fn unbound_entry(
        &self,
        category: EventCategory,
        event_name: String,
        extras: impl IntoIterator<Item = (String, String)>,
    ) -> AuditEntry {
        let labels = merge_extras(category, extras);
        AuditEntry {
            timestamp: chrono::Utc::now(),
            tenant: self.default_tenant.clone(),
            plan_id: PlanId(UNBOUND_PLAN_ID.to_string()),
            plan_version: 0,
            bundle_id: None,
            bundle_version: None,
            image_name: UNBOUND_IMAGE_NAME.to_string(),
            image_sha256: UNBOUND_IMAGE_SHA256.to_string(),
            event: event_name,
            labels,
        }
    }
}

/// Sentinel `plan_id` for unbound events. Recognizable in the
/// audit stream so consumers can filter "real plans" cleanly.
pub const UNBOUND_PLAN_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Sentinel `image_name` for unbound events.
pub const UNBOUND_IMAGE_NAME: &str = "<unbound>";

/// Sentinel `image_sha256` — 64 zero hex chars matches the field's
/// length constraint in plan 64.
pub const UNBOUND_IMAGE_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

fn validate_event_prefix(category: EventCategory, event_name: &str) -> Result<(), RecorderError> {
    let prefix = category.as_str();
    if !event_name.starts_with(prefix)
        || event_name
            .as_bytes()
            .get(prefix.len())
            .is_none_or(|&c| c != b'.')
    {
        return Err(RecorderError::EventPrefixMismatch {
            got: event_name.to_string(),
            expected_prefix: prefix,
        });
    }
    Ok(())
}

fn merge_extras(
    category: EventCategory,
    extras: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("category".to_string(), category.as_str().to_string());
    labels.extend(extras);
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::CapturingAuditSigner;
    use mvm_plan::{
        ArtifactPolicy, AttestationMode, AttestationRequirement, FsPolicyRef, KeyRotationSpec,
        Nonce, PolicyRef, PostRunLifecycle, Resources, RuntimeProfileRef, SCHEMA_VERSION,
        SignedImageRef, TimeoutSpec, WorkloadId,
    };

    fn fixture_plan() -> ExecutionPlan {
        let now = chrono::Utc::now();
        ExecutionPlan {
            schema_version: SCHEMA_VERSION,
            plan_id: PlanId("plan-test".to_string()),
            plan_version: 1,
            tenant: TenantId("local".to_string()),
            workload: WorkloadId("vm-test".to_string()),
            runtime_profile: RuntimeProfileRef("firecracker".to_string()),
            image: SignedImageRef {
                name: "vm-test".to_string(),
                sha256: "a".repeat(64),
                cosign_bundle: None,
            },
            resources: Resources {
                cpus: 1,
                mem_mib: 128,
                disk_mib: 0,
                timeouts: TimeoutSpec {
                    boot_secs: 30,
                    exec_secs: 0,
                },
            },
            network_policy: PolicyRef("local-default".to_string()),
            fs_policy: FsPolicyRef("local-default".to_string()),
            secrets: Vec::new(),
            egress_policy: PolicyRef("local-default".to_string()),
            tool_policy: PolicyRef("local-default".to_string()),
            artifact_policy: ArtifactPolicy {
                capture_paths: Vec::new(),
                retention_days: 0,
            },
            audit_labels: BTreeMap::new(),
            key_rotation: KeyRotationSpec { interval_days: 0 },
            attestation: AttestationRequirement {
                mode: AttestationMode::Noop,
            },
            release_pin: None,
            post_run: PostRunLifecycle {
                destroy_on_exit: true,
                snapshot_on_idle: false,
                idle_secs: 0,
            },
            valid_from: now,
            valid_until: now + chrono::Duration::minutes(10),
            nonce: Nonce::from_bytes([0u8; 16]),
        }
    }

    fn fixture_recorder() -> (Recorder, Arc<CapturingAuditSigner>) {
        let signer = Arc::new(CapturingAuditSigner::new());
        let recorder = Recorder::new(signer.clone(), TenantId("local".to_string()));
        (recorder, signer)
    }

    // ──────────────────────────────────────────────────────────────
    // Category taxonomy invariants
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn every_category_has_a_distinct_prefix() {
        let all = [
            EventCategory::Cmd,
            EventCategory::Lifecycle,
            EventCategory::Secret,
            EventCategory::Flow,
            EventCategory::Plan,
            EventCategory::Policy,
            EventCategory::Key,
            EventCategory::Host,
            EventCategory::Audit,
        ];
        let mut prefixes: Vec<&'static str> = all.iter().map(|c| c.as_str()).collect();
        prefixes.sort();
        let original_len = prefixes.len();
        prefixes.dedup();
        assert_eq!(
            prefixes.len(),
            original_len,
            "duplicate category prefixes — wire-stable identifiers must be unique"
        );
    }

    #[test]
    fn category_prefix_strings_are_stable() {
        // Pin every category's wire-stable string so a refactor
        // can't silently rename. Consumers in the wild grep
        // these literal strings.
        assert_eq!(EventCategory::Cmd.as_str(), "cmd");
        assert_eq!(EventCategory::Lifecycle.as_str(), "lifecycle");
        assert_eq!(EventCategory::Secret.as_str(), "secret");
        assert_eq!(EventCategory::Flow.as_str(), "flow");
        assert_eq!(EventCategory::Plan.as_str(), "plan");
        assert_eq!(EventCategory::Policy.as_str(), "policy");
        assert_eq!(EventCategory::Key.as_str(), "key");
        assert_eq!(EventCategory::Host.as_str(), "host");
        assert_eq!(EventCategory::Audit.as_str(), "audit");
    }

    #[test]
    fn plan_and_flow_require_plan_context() {
        assert!(EventCategory::Plan.requires_plan_context());
        assert!(EventCategory::Flow.requires_plan_context());
        assert!(!EventCategory::Cmd.requires_plan_context());
        assert!(!EventCategory::Host.requires_plan_context());
        assert!(!EventCategory::Secret.requires_plan_context());
    }

    #[test]
    fn display_renders_prefix() {
        assert_eq!(format!("{}", EventCategory::Plan), "plan");
    }

    // ──────────────────────────────────────────────────────────────
    // Event-prefix validation
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn record_plan_bound_accepts_matching_prefix() {
        let (recorder, signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            recorder
                .record_plan_bound(EventCategory::Plan, "plan.admitted", &plan, None, [])
                .await
                .unwrap();
        });
        assert_eq!(signer.entries().len(), 1);
        assert_eq!(signer.entries()[0].event, "plan.admitted");
    }

    #[test]
    fn record_plan_bound_rejects_missing_prefix() {
        let (recorder, signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            recorder
                .record_plan_bound(
                    EventCategory::Plan,
                    "admitted", // missing "plan." prefix
                    &plan,
                    None,
                    [],
                )
                .await
                .unwrap_err()
        });
        assert!(matches!(err, RecorderError::EventPrefixMismatch { .. }));
        assert!(
            signer.entries().is_empty(),
            "no entry should have been emitted"
        );
    }

    #[test]
    fn record_plan_bound_rejects_wrong_prefix() {
        let (recorder, _signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            recorder
                .record_plan_bound(
                    EventCategory::Plan,
                    "policy.something", // wrong category prefix
                    &plan,
                    None,
                    [],
                )
                .await
                .unwrap_err()
        });
        assert!(matches!(err, RecorderError::EventPrefixMismatch { .. }));
    }

    #[test]
    fn record_plan_bound_rejects_prefix_without_dot_separator() {
        // `planet.x` shouldn't pass just because it starts with `plan`.
        let (recorder, _signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            recorder
                .record_plan_bound(EventCategory::Plan, "planet.exploded", &plan, None, [])
                .await
                .unwrap_err()
        });
        assert!(matches!(err, RecorderError::EventPrefixMismatch { .. }));
    }

    // ──────────────────────────────────────────────────────────────
    // Plan-bound vs unbound enforcement
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn record_unbound_refuses_plan_bound_category() {
        let (recorder, _signer) = fixture_recorder();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            recorder
                .record_unbound(EventCategory::Plan, "plan.admitted", [])
                .await
                .unwrap_err()
        });
        assert!(matches!(err, RecorderError::MissingPlanContext { .. }));
    }

    #[test]
    fn record_unbound_accepts_host_event() {
        let (recorder, signer) = fixture_recorder();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            recorder
                .record_unbound(
                    EventCategory::Host,
                    "host.started",
                    [("version".to_string(), "0.14.0".to_string())],
                )
                .await
                .unwrap();
        });
        let entries = signer.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "host.started");
        assert_eq!(entries[0].plan_id.0, UNBOUND_PLAN_ID);
        assert_eq!(entries[0].image_name, UNBOUND_IMAGE_NAME);
        assert_eq!(entries[0].image_sha256, UNBOUND_IMAGE_SHA256);
        assert_eq!(
            entries[0].labels.get("version"),
            Some(&"0.14.0".to_string())
        );
        assert_eq!(entries[0].labels.get("category"), Some(&"host".to_string()));
    }

    #[test]
    fn record_unbound_uses_default_tenant() {
        let (recorder, signer) = fixture_recorder();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            recorder
                .record_unbound(EventCategory::Cmd, "cmd.up.invoked", [])
                .await
                .unwrap();
        });
        assert_eq!(signer.entries()[0].tenant.0, "local");
    }

    // ──────────────────────────────────────────────────────────────
    // Label injection
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn category_label_is_added_automatically() {
        // Operator-supplied extras don't need to redundantly pass
        // `category` — the recorder adds it.
        let (recorder, signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            recorder
                .record_plan_bound(EventCategory::Plan, "plan.admitted", &plan, None, [])
                .await
                .unwrap();
        });
        assert_eq!(
            signer.entries()[0].labels.get("category"),
            Some(&"plan".to_string())
        );
    }

    #[test]
    fn extras_override_category_label_if_caller_insists() {
        // The merge order: category-label first, extras second.
        // BTreeMap::extend overwrites on collision, so a caller
        // that passes `category=other` wins. This is intentional —
        // if you really need an off-taxonomy label, you can have
        // it; the test pins the behavior so a refactor of
        // merge_extras can't silently change ordering.
        let (recorder, signer) = fixture_recorder();
        let plan = fixture_plan();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            recorder
                .record_plan_bound(
                    EventCategory::Plan,
                    "plan.admitted",
                    &plan,
                    None,
                    [("category".to_string(), "override".to_string())],
                )
                .await
                .unwrap();
        });
        assert_eq!(
            signer.entries()[0].labels.get("category"),
            Some(&"override".to_string())
        );
    }
}
