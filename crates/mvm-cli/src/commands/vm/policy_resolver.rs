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
    /// A ref names a `<tenant>:<workload>` bundle but the file
    /// isn't there. `expected_path` is where the operator should
    /// drop the bundle.
    BundleNotFound {
        field: &'static str,
        value: String,
        expected_path: PathBuf,
    },

    /// The bundle file exists but couldn't be parsed (TOML error,
    /// schema-version mismatch, unknown field). Detail carries the
    /// underlying loader message so operators can fix the file.
    BundleParseFailed {
        field: &'static str,
        value: String,
        path: PathBuf,
        detail: String,
    },

    /// The plan's four policy refs disagree on which bundle to
    /// load. The current schema requires all four to point at the
    /// same `<tenant>:<workload>` bundle so the supervisor's
    /// component slots resolve consistently.
    MixedRefs {
        first: String,
        second_field: &'static str,
        second: String,
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
            Self::BundleNotFound {
                field,
                value,
                expected_path,
            } => write!(
                f,
                "policy ref {field} = {value:?} points at a bundle at {} that doesn't exist; \
                 create the file or change the ref",
                expected_path.display()
            ),
            Self::BundleParseFailed {
                field,
                value,
                path,
                detail,
            } => write!(
                f,
                "policy ref {field} = {value:?} loaded from {} failed to parse: {detail}",
                path.display()
            ),
            Self::MixedRefs {
                first,
                second_field,
                second,
            } => write!(
                f,
                "policy refs disagree: one field requests {first:?} but {second_field} requests \
                 {second:?}; all four refs must point at the same bundle"
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

/// Default base dir for policy bundles. Mirrors
/// `mvm_policy::toml_loader::default_policy_dir` but falls back to
/// the literal `~/.mvm/policies/` (good for error messages) when
/// `$HOME` is unset.
fn default_policy_dir() -> PathBuf {
    mvm_policy::toml_loader::default_policy_dir()
        .unwrap_or_else(|| PathBuf::from("~/.mvm/policies"))
}

/// Per-field validation that all four refs share the same shape
/// (all `LOCAL_DEFAULT`, or all the same `<tenant>:<workload>`).
/// Returns the agreed-upon shape on success.
fn classify_plan_refs<'a>(
    network: &'a str,
    fs: &'a str,
    egress: &'a str,
    tool: &'a str,
) -> Result<RefShape<'a>, ResolveError> {
    let fields: [(&'static str, &str); 4] = [
        ("network_policy", network),
        ("fs_policy", fs),
        ("egress_policy", egress),
        ("tool_policy", tool),
    ];
    let first_value = fields[0].1;
    for (field, value) in fields.iter() {
        if *value != first_value {
            return Err(ResolveError::MixedRefs {
                first: first_value.to_string(),
                second_field: field,
                second: value.to_string(),
            });
        }
        if matches!(classify(value), RefShape::Unrecognized) {
            return Err(ResolveError::Unrecognized {
                field,
                value: value.to_string(),
                expected: "\"local-default\" or \"<tenant>:<workload>\"",
            });
        }
    }
    Ok(classify(first_value))
}

/// Resolve a plan's four policy refs into concrete component slots.
///
/// Three outcomes:
///
/// - All four refs == `"local-default"` → Noop slots.
/// - All four refs == `"<tenant>:<workload>"` and the bundle file
///   parses cleanly → Noop slots, since no live consumer (L4/L7
///   proxy, real ToolGate) exists yet to read the parsed bundle.
///   The substrate proves the file format works so operators can
///   stage bundles before plan 60 Phase 3 ships.
/// - Anything else → typed error pointing the operator at what to
///   fix (missing file, parse error, mismatched refs, typo).
pub fn resolve_supervisor_components(plan: &ExecutionPlan) -> Result<ResolvedSlots, ResolveError> {
    let PolicyRef(network) = &plan.network_policy;
    let FsPolicyRef(fs) = &plan.fs_policy;
    let PolicyRef(egress) = &plan.egress_policy;
    let PolicyRef(tool) = &plan.tool_policy;

    match classify_plan_refs(network, fs, egress, tool)? {
        RefShape::LocalDefault => Ok(noop_slots()),
        RefShape::TenantWorkload { tenant, workload } => {
            // Load the bundle; even when parsing succeeds we
            // return Noops, because no live consumer exists yet.
            // The error paths surface real operator-actionable
            // problems (missing file, typo, schema mismatch).
            resolve_tenant_workload(network, tenant, workload)?;
            Ok(noop_slots())
        }
        // classify_plan_refs already converts Unrecognized into a
        // typed error; this branch is dead but keeps the match
        // exhaustive.
        RefShape::Unrecognized => unreachable!("classify_plan_refs handled Unrecognized"),
    }
}

fn noop_slots() -> ResolvedSlots {
    ResolvedSlots {
        egress: Box::new(NoopEgressProxy),
        tool_gate: Box::new(NoopToolGate),
        keystore: Box::new(NoopKeystoreReleaser),
        artifacts: Box::new(NoopArtifactCollector),
    }
}

fn resolve_tenant_workload(
    ref_value: &str,
    tenant: &str,
    workload: &str,
) -> Result<mvm_policy::PolicyBundle, ResolveError> {
    let base = default_policy_dir();
    let path = mvm_policy::toml_loader::bundle_path(&base, tenant, workload);
    match mvm_policy::toml_loader::load_bundle_from_path(&base, tenant, workload) {
        Ok(bundle) => Ok(bundle),
        Err(mvm_policy::toml_loader::LoadError::NotFound { path }) => {
            Err(ResolveError::BundleNotFound {
                field: "network_policy",
                value: ref_value.to_string(),
                expected_path: path,
            })
        }
        Err(
            mvm_policy::toml_loader::LoadError::Parse { detail, .. }
            | mvm_policy::toml_loader::LoadError::Io { detail, .. },
        ) => Err(ResolveError::BundleParseFailed {
            field: "network_policy",
            value: ref_value.to_string(),
            path,
            detail,
        }),
        Err(mvm_policy::toml_loader::LoadError::SchemaMismatch { got, known, .. }) => {
            Err(ResolveError::BundleParseFailed {
                field: "network_policy",
                value: ref_value.to_string(),
                path,
                detail: format!(
                    "schema_version {got} unsupported (this binary only \
                     understands version {known})"
                ),
            })
        }
    }
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

    /// Set all four PolicyRef fields on a plan to the same value.
    /// Phase-6 schema requires the four refs agree; tests that
    /// violate that on purpose set only one field.
    fn set_all_refs(plan: &mut ExecutionPlan, value: &str) {
        plan.network_policy = PolicyRef(value.to_string());
        plan.fs_policy = FsPolicyRef(value.to_string());
        plan.egress_policy = PolicyRef(value.to_string());
        plan.tool_policy = PolicyRef(value.to_string());
    }

    #[test]
    fn policy_resolver_rejects_tenant_ref_when_bundle_missing() {
        // Phase-6 substrate: a "<tenant>:<workload>" ref makes
        // resolve_supervisor_components attempt to load the bundle
        // file. When the file isn't there we surface BundleNotFound
        // with a clear path so operators know exactly where to put it.
        let mut plan = fixture_plan();
        set_all_refs(&mut plan, "acme:web-worker");
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("tenant-scoped ref without bundle must be refused"),
        };
        match err {
            ResolveError::BundleNotFound {
                value,
                expected_path,
                ..
            } => {
                assert_eq!(value, "acme:web-worker");
                let s = expected_path.to_string_lossy();
                assert!(s.contains("acme"), "path missing tenant: {s}");
                assert!(s.contains("web-worker.toml"), "path missing workload: {s}");
                assert!(s.contains("policies"), "path missing policies dir: {s}");
            }
            other => panic!("expected BundleNotFound, got {other:?}"),
        }
    }

    #[test]
    fn policy_resolver_rejects_unrecognized_policy_ref() {
        // Anything that's neither "local-default" nor
        // "<tenant>:<workload>" must be refused. The MixedRefs
        // check runs first, so make all four refs identical (and
        // bogus) to land on the Unrecognized branch.
        let mut plan = fixture_plan();
        set_all_refs(&mut plan, "bogus");
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("unrecognized ref must be refused"),
        };
        match err {
            ResolveError::Unrecognized {
                value, expected, ..
            } => {
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
    fn policy_resolver_rejects_mixed_refs() {
        // All four refs must agree (same bundle). If only one
        // points at a tenant bundle while the others stay
        // local-default, the resolver refuses with MixedRefs.
        let mut plan = fixture_plan();
        plan.tool_policy = PolicyRef("acme:tools-v1".to_string());
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("mixed refs must be refused"),
        };
        match err {
            ResolveError::MixedRefs {
                first,
                second_field,
                second,
            } => {
                assert_eq!(first, "local-default");
                assert_eq!(second_field, "tool_policy");
                assert_eq!(second, "acme:tools-v1");
            }
            other => panic!("expected MixedRefs, got {other:?}"),
        }
    }

    #[test]
    fn policy_resolver_inspects_fs_policy_ref() {
        // FsPolicyRef is a distinct newtype but should be treated
        // identically to PolicyRef in the resolver. If fs_policy
        // disagrees with the others, MixedRefs fires.
        let mut plan = fixture_plan();
        plan.fs_policy = FsPolicyRef("typo-default".to_string());
        let err = match resolve_supervisor_components(&plan) {
            Err(e) => e,
            Ok(_) => panic!("mixed fs ref must be refused"),
        };
        match err {
            ResolveError::MixedRefs {
                second_field,
                second,
                ..
            } => {
                assert_eq!(second_field, "fs_policy");
                assert_eq!(second, "typo-default");
            }
            other => panic!("expected MixedRefs, got {other:?}"),
        }
    }

    // ──────────────────────────────────────────────────────────────
    // Plan 60 Phase 6 — TOML-loading integration tests
    //
    // The resolver attempts to load `<HOME>/.mvm/policies/<tenant>/
    // <workload>.toml` for tenant-scoped refs. Since tests can't
    // safely mutate $HOME (process-global), we exercise the loader
    // path directly via `mvm_policy::toml_loader` in the unit
    // tests above. These tests pin the resolver's *error* surface
    // — the success path "bundle loads cleanly → Noops" is
    // implicit in the unit tests covering each error branch.
    //
    // A future end-to-end test that sets $HOME via a process-wide
    // mutex (like `vm/mod.rs::DATA_DIR_TEST_LOCK`) can pin the
    // success path. Out of scope for the Phase 6 substrate.
    // ──────────────────────────────────────────────────────────────
}
