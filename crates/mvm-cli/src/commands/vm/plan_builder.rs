//! Plan 64 W1 — `ExecutionPlan` synthesis from `mvmctl up` CLI args.
//!
//! Turns the surface-level CLI shape (flake ref, name, cpus, memory,
//! volumes, ports, secrets, etc.) into a typed `mvm_plan::ExecutionPlan`
//! the supervisor can verify, audit, and gate on.
//!
//! ## What lives here
//!
//! - [`synthesize_plan`] — the one entry point. Takes a borrowed
//!   `SynthesisInput` and produces an `ExecutionPlan` ready to sign.
//! - Internal helpers for resource budgets and validity windows.
//!
//! ## What does NOT live here
//!
//! - **Signing.** That's W2's `signer` module — `synthesize_plan`
//!   builds the unsigned plan; the caller signs.
//! - **Backend dispatch.** W3 wires the supervisor to `BackendLauncher`;
//!   this module is plan-shape-only, no I/O.
//! - **Policy resolution.** W5's `policy_resolver` turns the plan's
//!   `PolicyRef` fields into concrete supervisor components.
//!
//! ## Field source map (plan field → CLI input)
//!
//! | Plan field | Where it comes from |
//! |---|---|
//! | `plan_id` | fresh `Uuid::new_v4()` per invocation |
//! | `plan_version` | always 1 for synthesized plans (mvmd revisions get higher numbers) |
//! | `tenant` | `--tenant` flag or default `"local"` |
//! | `workload` | derived from `--name` or flake ref leaf |
//! | `runtime_profile` | hypervisor flag mapped to a profile name |
//! | `image` | computed lazily from rootfs SHA-256 (filled by caller after build) |
//! | `resources` | `--cpus`, `--memory`, `--ttl` |
//! | `*_policy` / `fs_policy` | `"local-default"` (W5 resolver maps to Noops) |
//! | `valid_from`/`valid_until` | now + 10 min window |
//! | `nonce` | fresh 128 bits from `OsRng` per invocation |
//! | everything else | conservative defaults (no attestation, destroy-on-exit, etc.) |

use anyhow::Result;
use chrono::{Duration, Utc};
use mvm_plan::{
    ArtifactPolicy, AttestationMode, AttestationRequirement, ExecutionPlan, FsPolicyRef,
    KeyRotationSpec, Nonce, PlanId, PolicyRef, PostRunLifecycle, Resources, RuntimeProfileRef,
    SCHEMA_VERSION, SignedImageRef, TenantId, TimeoutSpec, WorkloadId,
};
use rand::RngCore;
use std::collections::BTreeMap;

/// Default tenant for single-host runs. ADR-002's "one guest = one
/// workload" model means the tenant boundary is the host itself unless
/// mvmd's multi-tenant control plane is wired in.
pub const DEFAULT_TENANT: &str = "local";

/// Default policy name resolved by W5's policy_resolver to a Noop
/// component-slot set. Production deployments override via the
/// supervisor's policy bundle.
pub const DEFAULT_POLICY_REF: &str = "local-default";

/// Plan validity window from `now`. 10 minutes is long enough that
/// boot + signature verification + state machine walk finishes well
/// within the window; short enough that a captured plan can't be
/// replayed hours later.
pub const VALIDITY_WINDOW_MINUTES: i64 = 10;

/// Caller-supplied input. We take a struct rather than the 10
/// individual fields the workspace clippy `too_many_arguments` lint
/// would otherwise force into a refactor anyway.
#[derive(Debug, Clone)]
pub struct SynthesisInput<'a> {
    /// VM name (post-validation). Synthesized plans use this verbatim
    /// as the `WorkloadId`.
    pub vm_name: &'a str,
    /// Optional tenant override. `None` → `DEFAULT_TENANT`.
    pub tenant: Option<&'a str>,
    /// Resolved runtime profile (`firecracker` / `microsandbox` /
    /// `apple-container` / `cloud-hypervisor`).
    pub backend_name: &'a str,
    /// Image reference for `SignedImageRef`. `sha256` is the
    /// lowercase-hex digest of the rootfs (computed by `mvm-security::
    /// image_verify::hash_artifact` or upstream Nix).
    pub image_name: &'a str,
    pub image_sha256: &'a str,
    pub image_cosign_bundle: Option<&'a str>,
    /// vCPU count.
    pub cpus: u32,
    /// Memory budget in MiB.
    pub mem_mib: u64,
    /// Disk budget in MiB. 0 = no explicit cap (supervisor falls back
    /// to whatever the image carries).
    pub disk_mib: u64,
    /// Boot-timeout seconds. Conservative default 60s on capable hosts.
    pub boot_timeout_secs: u32,
    /// Exec-timeout seconds. 0 = unbounded.
    pub exec_timeout_secs: u32,
    /// Whether the post-run lifecycle should destroy the VM on exit.
    /// True for one-shot CLI workloads; false for daemon-shape services.
    pub destroy_on_exit: bool,
    /// Optional pin to a content-addressed `.mvmpkg` bundle. When
    /// set, the synthesised plan carries the pin and the supervisor's
    /// admit path re-verifies the archive against this triple before
    /// backend dispatch. Sprint 52 W2 follow-on substrate — populating
    /// it from `mvmctl up` flags is the next step.
    pub bundle_pin: Option<mvm_plan::bundle::PlanArtifact>,
}

/// Build an unsigned `ExecutionPlan` from CLI-shaped input.
///
/// Generates a fresh `plan_id` (UUIDv4) and `nonce` (128 random bits)
/// per invocation; the validity window starts at the call site's
/// `now()` and lasts `VALIDITY_WINDOW_MINUTES`. The caller signs the
/// returned plan via [`mvm_plan::sign_plan`] before passing it to the
/// supervisor.
pub fn synthesize_plan(input: &SynthesisInput<'_>) -> Result<ExecutionPlan> {
    let plan_id = PlanId(uuid::Uuid::new_v4().to_string());
    let nonce = fresh_nonce();
    let now = Utc::now();

    let tenant_str = input.tenant.unwrap_or(DEFAULT_TENANT);
    if tenant_str.is_empty() {
        anyhow::bail!("tenant must not be empty");
    }
    if input.vm_name.is_empty() {
        anyhow::bail!("vm_name must not be empty");
    }
    if input.image_sha256.len() != 64 {
        anyhow::bail!(
            "image_sha256 must be a 64-character lowercase hex digest, got {} chars",
            input.image_sha256.len()
        );
    }

    let resources = Resources {
        cpus: input.cpus.max(1),
        mem_mib: input.mem_mib.max(64),
        disk_mib: input.disk_mib,
        timeouts: TimeoutSpec {
            boot_secs: input.boot_timeout_secs.max(1),
            exec_secs: input.exec_timeout_secs,
        },
    };

    let image = SignedImageRef {
        name: input.image_name.to_string(),
        sha256: input.image_sha256.to_string(),
        cosign_bundle: input.image_cosign_bundle.map(str::to_string),
    };

    Ok(ExecutionPlan {
        schema_version: SCHEMA_VERSION,
        plan_id,
        plan_version: 1,
        tenant: TenantId(tenant_str.to_string()),
        workload: WorkloadId(input.vm_name.to_string()),
        runtime_profile: RuntimeProfileRef(input.backend_name.to_string()),
        image,
        resources,
        network_policy: PolicyRef(DEFAULT_POLICY_REF.to_string()),
        fs_policy: FsPolicyRef(DEFAULT_POLICY_REF.to_string()),
        secrets: Vec::new(),
        egress_policy: PolicyRef(DEFAULT_POLICY_REF.to_string()),
        tool_policy: PolicyRef(DEFAULT_POLICY_REF.to_string()),
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
            destroy_on_exit: input.destroy_on_exit,
            snapshot_on_idle: false,
            idle_secs: 0,
        },
        valid_from: now,
        valid_until: now + Duration::minutes(VALIDITY_WINDOW_MINUTES),
        nonce,
        bundle: input.bundle_pin.clone(),
    })
}

/// Generate a fresh 128-bit nonce from `OsRng`. `mvm_plan::Nonce`
/// wraps a 32-character lowercase hex string (i.e., 16 bytes = 128
/// bits) — match that here so the wire format roundtrips.
fn fresh_nonce() -> Nonce {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    Nonce::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(vm_name: &str) -> SynthesisInput<'_> {
        SynthesisInput {
            vm_name,
            tenant: None,
            backend_name: "firecracker",
            image_name: "myimage",
            image_sha256: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            image_cosign_bundle: None,
            cpus: 2,
            mem_mib: 512,
            disk_mib: 0,
            boot_timeout_secs: 60,
            exec_timeout_secs: 0,
            destroy_on_exit: false,
            bundle_pin: None,
        }
    }

    #[test]
    fn carries_cli_resource_overrides() {
        let mut inp = input("myvm");
        inp.cpus = 4;
        inp.mem_mib = 2048;
        inp.boot_timeout_secs = 120;
        inp.exec_timeout_secs = 600;
        let plan = synthesize_plan(&inp).unwrap();
        assert_eq!(plan.resources.cpus, 4);
        assert_eq!(plan.resources.mem_mib, 2048);
        assert_eq!(plan.resources.timeouts.boot_secs, 120);
        assert_eq!(plan.resources.timeouts.exec_secs, 600);
    }

    #[test]
    fn defaults_tenant_to_local() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        assert_eq!(plan.tenant.0, DEFAULT_TENANT);
    }

    #[test]
    fn honors_explicit_tenant_override() {
        let mut inp = input("myvm");
        inp.tenant = Some("acme");
        let plan = synthesize_plan(&inp).unwrap();
        assert_eq!(plan.tenant.0, "acme");
    }

    #[test]
    fn workload_is_vm_name_verbatim() {
        let plan = synthesize_plan(&input("my-special-vm")).unwrap();
        assert_eq!(plan.workload.0, "my-special-vm");
    }

    #[test]
    fn round_trips_through_serde() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        let json = serde_json::to_string(&plan).expect("plan serializes");
        let parsed: ExecutionPlan = serde_json::from_str(&json).expect("plan parses");
        assert_eq!(parsed, plan);
    }

    #[test]
    fn generates_unique_plan_id_per_call() {
        let p1 = synthesize_plan(&input("myvm")).unwrap();
        let p2 = synthesize_plan(&input("myvm")).unwrap();
        assert_ne!(p1.plan_id, p2.plan_id);
    }

    #[test]
    fn generates_unique_nonce_per_call() {
        let p1 = synthesize_plan(&input("myvm")).unwrap();
        let p2 = synthesize_plan(&input("myvm")).unwrap();
        assert_ne!(p1.nonce, p2.nonce);
    }

    #[test]
    fn nonce_is_32_hex_chars() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        let hex = plan.nonce.as_hex();
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }

    #[test]
    fn validity_window_is_default_10_minutes() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        let elapsed = plan.valid_until - plan.valid_from;
        assert_eq!(elapsed.num_minutes(), VALIDITY_WINDOW_MINUTES);
    }

    #[test]
    fn enforces_minimum_cpus_of_one() {
        let mut inp = input("myvm");
        inp.cpus = 0;
        let plan = synthesize_plan(&inp).unwrap();
        assert_eq!(plan.resources.cpus, 1, "CPUs floor at 1");
    }

    #[test]
    fn enforces_minimum_memory_of_64mib() {
        let mut inp = input("myvm");
        inp.mem_mib = 0;
        let plan = synthesize_plan(&inp).unwrap();
        assert_eq!(plan.resources.mem_mib, 64, "memory floor at 64MiB");
    }

    #[test]
    fn rejects_empty_vm_name() {
        let err = synthesize_plan(&input("")).unwrap_err();
        assert!(err.to_string().contains("vm_name"));
    }

    #[test]
    fn rejects_empty_tenant() {
        let mut inp = input("myvm");
        inp.tenant = Some("");
        let err = synthesize_plan(&inp).unwrap_err();
        assert!(err.to_string().contains("tenant"));
    }

    #[test]
    fn rejects_wrong_length_sha256() {
        let mut inp = input("myvm");
        inp.image_sha256 = "deadbeef";
        let err = synthesize_plan(&inp).unwrap_err();
        assert!(err.to_string().contains("64-character"));
    }

    #[test]
    fn defaults_attestation_to_noop_and_no_release_pin() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        assert_eq!(plan.attestation.mode, AttestationMode::Noop);
        assert!(plan.release_pin.is_none());
    }

    #[test]
    fn all_policy_refs_default_to_local_default() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        assert_eq!(plan.network_policy.0, DEFAULT_POLICY_REF);
        assert_eq!(plan.fs_policy.0, DEFAULT_POLICY_REF);
        assert_eq!(plan.egress_policy.0, DEFAULT_POLICY_REF);
        assert_eq!(plan.tool_policy.0, DEFAULT_POLICY_REF);
    }

    #[test]
    fn schema_version_is_pinned() {
        let plan = synthesize_plan(&input("myvm")).unwrap();
        assert_eq!(plan.schema_version, SCHEMA_VERSION);
    }
}
