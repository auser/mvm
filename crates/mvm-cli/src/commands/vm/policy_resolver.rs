//! Plan 64 W5 — `PolicyRef → concrete component slot` resolver.
//!
//! `mvm-plan::ExecutionPlan` carries four policy refs that name (but
//! do not contain) the policy bundle a workload runs under:
//!
//! - `network_policy: PolicyRef`
//! - `fs_policy: FsPolicyRef`
//! - `egress_policy: PolicyRef`
//! - `tool_policy: PolicyRef`
//!
//! Each is a freeform string — `"local-default"` for the
//! single-tenant local dev posture, or `"<tenant>:<workload>"` for a
//! mvmd-managed policy bundle on disk at
//! `~/.mvm/policies/<tenant>/<workload>.toml`. The supervisor needs
//! four trait objects (`EgressProxy`, `ToolGate`, `KeystoreReleaser`,
//! `ArtifactCollector`) to make admission decisions; this resolver
//! is the function that turns a plan's refs into those objects.
//!
//! ## Today (substrate-only)
//!
//! Plan 60 Phase 3 owns the policy-bundle file format — the actual
//! TOML schema, the resolution rules, the cosign-bundle validation.
//! Until that lands, this resolver returns:
//!
//! - **All four refs == `"local-default"`** → `ResolvedSlots` of
//!   fail-closed `NoopEgressProxy` / `NoopToolGate` /
//!   `NoopKeystoreReleaser` / `NoopArtifactCollector`. The Noops
//!   error with `NotWired` on first consult; a misconfigured
//!   supervisor cannot accidentally pass tenant traffic through
//!   them.
//! - **Any ref shaped `"<tenant>:<workload>"`** →
//!   [`ResolveError::NotYetImplemented`], naming the policy file
//!   path that would have been loaded once Phase 3 ships.
//! - **Anything else** → [`ResolveError::Unrecognized`], naming the
//!   field and the value.
//!
//! ## No live consumer yet
//!
//! The W3 callsite (`up.rs::admit_plan_for_boot`) ships
//! `admit + backend.start()` rather than `Supervisor::launch`. The
//! `BackendLauncher` adapter that would consume `ResolvedSlots` via
//! `Supervisor::with_egress` / `with_tool_gate` / etc. lives in the
//! mvm-hostd lift (ADR-041 "negative / honest deferrals"). This
//! module exists as substrate so the lift is a one-line change.
//!
//! ## Out of scope (named in plan 64 W5 § "Don't do")
//!
//! - The TOML policy file format — plan 60 Phase 3.
//! - Wiring the resolver into `mvmctl up` — nowhere to wire, since
//!   `Supervisor::launch` isn't on the production path yet.
//! - The `BackendLauncher` adapter — explicit deferred from W3.
//!
//! ## Dead-code allow
//!
//! Every public item below is currently unused outside this module's
//! tests because `up.rs::admit_plan_for_boot` ships
//! `admit + backend.start()` rather than `Supervisor::launch` (W3
//! deferral). The `#![allow(dead_code)]` mirrors
//! `AdmittedPlan.signed`'s justification — keeping the surface
//! published stabilises the contract for the eventual mvm-hostd
//! consumer.

#![allow(dead_code)]

use std::path::PathBuf;

use mvm_plan::{ExecutionPlan, FsPolicyRef, PolicyRef};
use mvm_supervisor::{
    ArtifactCollector, EgressProxy, KeystoreReleaser, NoopArtifactCollector, NoopEgressProxy,
    NoopKeystoreReleaser, NoopToolGate, ToolGate,
};

/// The fixed identifier for the local-dev policy bundle. Any
/// `PolicyRef`/`FsPolicyRef` whose inner value equals this string
/// resolves to fail-closed Noops in v0.
pub const LOCAL_DEFAULT: &str = "local-default";

/// Trait-object bundle the supervisor consumes via its
/// `with_egress` / `with_tool_gate` / `with_keystore` /
/// `with_artifact_collector` builder calls.
///
/// Each field is a `Box<dyn Trait>` so the resolver can return
/// either a Noop or a future real impl without leaking the concrete
/// type to callers. v0 always returns Noops on the happy path.
pub struct ResolvedSlots {
    pub egress: Box<dyn EgressProxy>,
    pub tool_gate: Box<dyn ToolGate>,
    pub keystore: Box<dyn KeystoreReleaser>,
    pub artifacts: Box<dyn ArtifactCollector>,
}

/// Errors `resolve_supervisor_components` can return.
#[derive(Debug)]
pub enum ResolveError {
    /// A ref names a `<tenant>:<workload>` bundle the policy-bundle
    /// loader cannot honor until plan 60 Phase 3 ships. The
    /// embedded `expected_path` is where the file *would* live;
    /// callers can surface it to the user as "create this file
    /// once Phase 3 lands".
    NotYetImplemented {
        field: &'static str,
        value: String,
        expected_path: PathBuf,
    },

    /// A ref's shape doesn't match `"local-default"` or
    /// `"<tenant>:<workload>"`. We refuse rather than fall back to
    /// Noops so that a typo (`"locale-default"`) fails loudly at
    /// admission instead of silently passing traffic through Noop
    /// slots that error on first consult.
    Unrecognized {
        field: &'static str,
        value: String,
        expected: &'static str,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotYetImplemented {
                field,
                value,
                expected_path,
            } => write!(
                f,
                "policy ref {field} = {value:?} would load from {} but the policy-bundle loader \
                 is not yet implemented (plan 60 Phase 3)",
                expected_path.display()
            ),
            Self::Unrecognized {
                field,
                value,
                expected,
            } => write!(
                f,
                "policy ref {field} = {value:?} is not recognized (expected {expected})"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Classify a single ref into the v0 buckets. Pure string inspection
/// — no I/O.
enum RefShape<'a> {
    LocalDefault,
    TenantWorkload { tenant: &'a str, workload: &'a str },
    Unrecognized,
}

fn classify(value: &str) -> RefShape<'_> {
    if value == LOCAL_DEFAULT {
        return RefShape::LocalDefault;
    }
    // `<tenant>:<workload>` — exactly one colon, both halves
    // non-empty, no path separators (keeps the resolved file path
    // confined to `~/.mvm/policies/`).
    if let Some((tenant, workload)) = value.split_once(':')
        && !tenant.is_empty()
        && !workload.is_empty()
        && !tenant.contains('/')
        && !workload.contains('/')
        && !tenant.contains('\\')
        && !workload.contains('\\')
    {
        return RefShape::TenantWorkload { tenant, workload };
    }
    RefShape::Unrecognized
}

/// Where the policy bundle for `<tenant>:<workload>` would live.
/// Used only for the `NotYetImplemented` error message; no I/O.
fn expected_policy_path(tenant: &str, workload: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("~"), PathBuf::from);
    home.join(".mvm")
        .join("policies")
        .join(tenant)
        .join(format!("{workload}.toml"))
}

/// Inspect each ref; return `Ok(())` if it's `"local-default"`, the
/// matching `ResolveError` otherwise.
fn check_ref(field: &'static str, value: &str) -> Result<(), ResolveError> {
    match classify(value) {
        RefShape::LocalDefault => Ok(()),
        RefShape::TenantWorkload { tenant, workload } => Err(ResolveError::NotYetImplemented {
            field,
            value: value.to_string(),
            expected_path: expected_policy_path(tenant, workload),
        }),
        RefShape::Unrecognized => Err(ResolveError::Unrecognized {
            field,
            value: value.to_string(),
            expected: "\"local-default\" or \"<tenant>:<workload>\"",
        }),
    }
}

/// Resolve a plan's four policy refs into concrete component slots.
///
/// In v0 the only path that succeeds is all four refs equal to
/// [`LOCAL_DEFAULT`]; any other configuration returns a typed error.
/// See the module docs for the rationale.
pub fn resolve_supervisor_components(plan: &ExecutionPlan) -> Result<ResolvedSlots, ResolveError> {
    let PolicyRef(network) = &plan.network_policy;
    let FsPolicyRef(fs) = &plan.fs_policy;
    let PolicyRef(egress) = &plan.egress_policy;
    let PolicyRef(tool) = &plan.tool_policy;

    check_ref("network_policy", network)?;
    check_ref("fs_policy", fs)?;
    check_ref("egress_policy", egress)?;
    check_ref("tool_policy", tool)?;

    Ok(ResolvedSlots {
        egress: Box::new(NoopEgressProxy),
        tool_gate: Box::new(NoopToolGate),
        keystore: Box::new(NoopKeystoreReleaser),
        artifacts: Box::new(NoopArtifactCollector),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mvm_plan::{
        ArtifactPolicy, AttestationMode, AttestationRequirement, KeyRotationSpec, Nonce, PlanId,
        PostRunLifecycle, Resources, RuntimeProfileRef, SCHEMA_VERSION, SignedImageRef, TenantId,
        TimeoutSpec, WorkloadId,
    };
    use std::collections::BTreeMap;

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
            network_policy: PolicyRef(LOCAL_DEFAULT.to_string()),
            fs_policy: FsPolicyRef(LOCAL_DEFAULT.to_string()),
            secrets: Vec::new(),
            egress_policy: PolicyRef(LOCAL_DEFAULT.to_string()),
            tool_policy: PolicyRef(LOCAL_DEFAULT.to_string()),
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

    #[test]
    fn policy_resolver_returns_noops_for_local_default() {
        // All four PolicyRef fields == "local-default" — happy path.
        // The Noops fail-closed on consult; this test verifies we got
        // *back* a ResolvedSlots and that each slot is in fact the
        // Noop variant by exercising its `NotWired` error.
        let plan = fixture_plan();
        let slots = resolve_supervisor_components(&plan).expect("local-default must resolve");

        // Hit each Noop and assert it errors with NotWired. This is
        // the strongest assertion we can make without inspecting
        // private type identity — and matches how a real
        // misconfigured supervisor would discover it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let egress_err = slots
                .egress
                .inspect("example.com", "/")
                .await
                .expect_err("Noop egress must error");
            assert!(
                matches!(egress_err, mvm_supervisor::EgressError::NotWired),
                "unexpected egress err: {egress_err:?}"
            );

            let tool_err = slots
                .tool_gate
                .check("anything")
                .await
                .expect_err("Noop tool gate must error");
            assert!(
                matches!(tool_err, mvm_supervisor::ToolError::NotWired),
                "unexpected tool err: {tool_err:?}"
            );

            let revoke_err = slots
                .keystore
                .revoke("anything")
                .await
                .expect_err("Noop keystore must error");
            assert!(
                matches!(revoke_err, mvm_supervisor::KeystoreError::NotWired),
                "unexpected keystore err: {revoke_err:?}"
            );

            let collect_err = slots
                .artifacts
                .collect(&plan.plan_id)
                .await
                .expect_err("Noop artifact collector must error");
            assert!(
                matches!(collect_err, mvm_supervisor::ArtifactError::NotWired),
                "unexpected artifact err: {collect_err:?}"
            );
        });
    }

    #[test]
    fn policy_resolver_rejects_tenant_policy_ref_with_clear_error() {
        // A `<tenant>:<workload>` ref must return NotYetImplemented
        // and the error message must name the policy file path that
        // would have been loaded once plan 60 Phase 3 ships.
        let mut plan = fixture_plan();
        plan.network_policy = PolicyRef("acme:web-worker".to_string());
        // `ResolvedSlots` doesn't impl Debug (its trait-object fields
        // can't), so we can't use `.expect_err()` — match the Result
        // explicitly to surface the error.
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("tenant-scoped ref must be refused in v0"),
        };
        match err {
            ResolveError::NotYetImplemented {
                field,
                value,
                expected_path,
            } => {
                assert_eq!(field, "network_policy");
                assert_eq!(value, "acme:web-worker");
                // Path must mention both tenant and workload, plus
                // the .toml suffix the eventual loader expects.
                let s = expected_path.to_string_lossy();
                assert!(s.contains("acme"), "path missing tenant: {s}");
                assert!(s.contains("web-worker.toml"), "path missing workload: {s}");
                assert!(s.contains(".mvm"), "path missing .mvm dir: {s}");
                assert!(s.contains("policies"), "path missing policies dir: {s}");
            }
            other => panic!("expected NotYetImplemented, got {other:?}"),
        }
    }

    #[test]
    fn policy_resolver_rejects_unrecognized_policy_ref() {
        // Anything that's neither "local-default" nor
        // "<tenant>:<workload>" must be refused, and the error must
        // name the field that carried the bad ref so the operator
        // can find it in the plan.
        let mut plan = fixture_plan();
        plan.egress_policy = PolicyRef("bogus".to_string());
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("unrecognized ref must be refused"),
        };
        match err {
            ResolveError::Unrecognized {
                field,
                value,
                expected,
            } => {
                assert_eq!(field, "egress_policy");
                assert_eq!(value, "bogus");
                assert!(expected.contains("local-default"));
                assert!(expected.contains("tenant"));
            }
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    #[test]
    fn policy_resolver_signature_returns_box_dyn_trait_objects() {
        // Compile-time check: the slots inside ResolvedSlots can be
        // moved into builder functions that accept
        // `Box<dyn EgressProxy>`, etc. This proves the eventual
        // `Supervisor::with_egress(self, Box<dyn EgressProxy>)` lift
        // is just a `.with_egress(slots.egress)` call away — no
        // adapter layer needed.
        fn take_egress(_: Box<dyn EgressProxy>) {}
        fn take_tool_gate(_: Box<dyn ToolGate>) {}
        fn take_keystore(_: Box<dyn KeystoreReleaser>) {}
        fn take_artifacts(_: Box<dyn ArtifactCollector>) {}

        let slots = resolve_supervisor_components(&fixture_plan()).unwrap();
        take_egress(slots.egress);
        take_tool_gate(slots.tool_gate);
        take_keystore(slots.keystore);
        take_artifacts(slots.artifacts);
    }

    #[test]
    fn policy_resolver_rejects_when_only_one_field_is_tenant_scoped() {
        // Mixed refs must fail on the first non-local field;
        // confirms the resolver inspects *every* field, not just
        // network_policy.
        let mut plan = fixture_plan();
        plan.tool_policy = PolicyRef("acme:tools-v1".to_string());
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("mixed refs must be refused"),
        };
        match err {
            ResolveError::NotYetImplemented { field, .. } => {
                assert_eq!(field, "tool_policy");
            }
            other => panic!("expected NotYetImplemented, got {other:?}"),
        }
    }

    #[test]
    fn policy_resolver_rejects_fs_policy_ref_independently() {
        // FsPolicyRef is a distinct newtype but obeys the same
        // shape rules; ensure the resolver doesn't skip it.
        let mut plan = fixture_plan();
        plan.fs_policy = FsPolicyRef("typo-default".to_string());
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("typo must be refused"),
        };
        match err {
            ResolveError::Unrecognized { field, value, .. } => {
                assert_eq!(field, "fs_policy");
                assert_eq!(value, "typo-default");
            }
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    // Integration test note: `supervisor_with_resolved_slots_carries_plan_id_to_audit`
    // is the eventual cross-module test that proves the resolver's
    // output flows through `Supervisor::launch` to the audit chain
    // with the correct plan_id. It cannot land here because
    // `Supervisor::launch` is not on the production callsite yet
    // (W3 shipped `admit + backend.start()`; ADR-041 documents the
    // deferral). The test lands together with the mvm-hostd lift.
}
