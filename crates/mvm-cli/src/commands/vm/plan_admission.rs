//! Plan 64 W3 — plan-admission pipeline used by `mvmctl up`.
//!
//! Threads W1's `synthesize_plan` + W2's `host_signer` into the
//! supervisor-equivalent admission flow:
//!
//! ```text
//! cmd_run(args)
//!   ↓ synthesize_plan(args)        [W1]
//!   ↓ load_or_init_host_signer()    [W2]
//!   ↓ sign_plan(plan, signer)
//!   ↓ verify_plan(signed, trusted) — catches signing-time bugs
//!   ↓ check_window(plan, now)      — Plan-37 G4 validity window
//!   ↓ nonce_store.check_and_insert  — Plan-37 G4 replay protection
//!   ↓ return AdmittedPlan { plan, plan_id, signer_id, signed }
//!   ↓ caller invokes backend.start() as before
//! ```
//!
//! What this module does NOT do (intentional scope reduction from
//! plan 64 W3's original framing):
//!
//! - **Drive `Supervisor::launch`.** The supervisor's backend
//!   dispatch slot expects a `BackendLauncher` trait impl that
//!   wraps today's `AnyBackend::start()`; landing that wrapper
//!   means refactoring three call sites in 1084 lines of `up.rs`
//!   (the main path, the MVM_DIRECT_BOOT branch, and the `--watch`
//!   path). That refactor stays in plan 64's W3 scope but lands
//!   in a follow-up PR. **This module is the substrate that makes
//!   the eventual supervisor refactor a one-line change** —
//!   `admit_for_run` produces the `SignedExecutionPlan` the
//!   supervisor needs.
//!
//! - **Emit audit lines.** W4 wires `FileAuditSigner` onto the
//!   `AdmittedPlan`'s `plan_id`; this module is silent on audit.
//!
//! - **Resolve component slots.** W5 maps `PolicyRef → concrete
//!   EgressProxy/ToolGate/...`. This module returns the plan with
//!   refs unresolved.
//!
//! ## Test seam
//!
//! `admit_for_run` takes a `Clock` and a `NonceLedger` so tests can
//! drive the validity window + replay protection deterministically.
//! Production callers use `SystemClock` + the host's nonce store.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use mvm_plan::{
    ExecutionPlan, NonceStore, PlanId, PlanValidityError, SignedExecutionPlan, check_window,
    sign_plan, verify_plan,
};
use std::sync::Mutex;

use super::host_signer::host_signer_id;
use super::plan_builder::{SynthesisInput, synthesize_plan};

/// Abstracts wall-clock time so tests can drive `check_window`
/// deterministically.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock — reads the system wall-clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Production nonce ledger. Holds a `NonceStore` behind a mutex so
/// it's `Send + Sync`. In v0 we instantiate one per `mvmctl up` —
/// later when the supervisor is in-process, the ledger spans every
/// up call for the lifetime of the supervisor.
pub struct InMemoryNonceLedger {
    inner: Mutex<NonceStore>,
}

impl InMemoryNonceLedger {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(NonceStore::default()),
        }
    }
}

impl Default for InMemoryNonceLedger {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a successful admission. Carries everything the caller
/// needs to hand to the backend (the plan + its id), to W4's audit
/// chain (the plan again, for `AuditEntry::for_plan`), and — for
/// downstream consumers that want the canonical envelope — the
/// `SignedExecutionPlan` itself.
///
/// `signed` carries a `#[allow(dead_code)]` because the only current
/// consumer is in-module tests proving the envelope round-trips
/// through `verify_plan`. Cross-process consumers (a future
/// `mvm-hostd` lift, or `mvmctl plan show` once it lands) will read
/// the envelope verbatim. Keeping the field on the struct stabilises
/// the surface for those callers.
#[derive(Debug)]
pub struct AdmittedPlan {
    pub plan: ExecutionPlan,
    pub plan_id: PlanId,
    pub signer_id: String,
    #[allow(dead_code)]
    pub signed: SignedExecutionPlan,
}

/// Run the full admission pipeline for an `mvmctl up` invocation.
///
/// On success, the caller proceeds to `backend.start()` knowing the
/// plan was signed under the host signer, verified with the host's
/// own public key, satisfies its own validity window, and hasn't been
/// admitted before (replay protection).
///
/// On failure, the user gets a clear error per failure class:
///   - `tenant must not be empty` / `vm_name must not be empty` —
///     synthesis-time guard
///   - `host signer at {path} has mode {found}; expected 0600` —
///     keystore guard
///   - `plan validity window violated: {detail}` — G4 window check
///   - `plan replay detected for signer {id}; nonce {hex}` — G4 nonce
pub fn admit_for_run(
    input: &SynthesisInput<'_>,
    clock: &dyn Clock,
    ledger: &InMemoryNonceLedger,
    host_signer_keys_dir: Option<&std::path::Path>,
) -> Result<AdmittedPlan> {
    // Build the unsigned plan first. Synthesis failures are caught
    // before we touch the keystore — keeps "signed bad plan" from
    // being an outcome.
    let plan = synthesize_plan(input).context("synthesizing plan")?;

    // Load or generate the host signer. W2's load_or_init refuses
    // loose perms; that error propagates verbatim.
    let signer = match host_signer_keys_dir {
        Some(dir) => super::host_signer::load_or_init_at(dir)?,
        None => super::host_signer::load_or_init()?,
    };
    let signer_id = host_signer_id();

    // Sign + verify roundtrip. Verifying our own signature catches
    // wire-format bugs that would otherwise surface at a real
    // verifier (mvmd's supervisor, an upstream consumer's mvm).
    let signed = sign_plan(&plan, &signer.signing, &signer_id);
    let trusted: [(&str, &VerifyingKey); 1] = [(&signer_id, &signer.verifying)];
    let verified = verify_plan(&signed, &trusted).context("verifying just-signed plan")?;

    // Validity window — refuses plans whose now() is outside
    // [valid_from, valid_until). For freshly-synthesized plans this
    // can only fire if the host's clock changed during signing or if
    // someone overrode the validity window in W1's defaults.
    let now = clock.now();
    check_window(&verified, now).map_err(|e| match e {
        PlanValidityError::NotYetValid { .. } | PlanValidityError::Expired { .. } => {
            anyhow::anyhow!("plan validity window violated: {e}")
        }
        other => anyhow::anyhow!("plan validity error: {other}"),
    })?;

    // Replay protection: insert (signer_id, nonce). A second admit_for_run
    // call within the validity window with the same nonce gets refused.
    // Synthesis generates fresh nonces, so this only fires on the
    // pathological "same plan submitted twice" case.
    {
        let mut store = ledger.inner.lock().expect("nonce store mutex poisoned");
        store
            .check_and_insert(&signer_id, &verified)
            .context("replay protection check")?;
    }

    Ok(AdmittedPlan {
        plan_id: verified.plan_id.clone(),
        signer_id,
        plan: verified,
        signed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FIXTURE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fixture_input(vm_name: &str) -> SynthesisInput<'_> {
        SynthesisInput {
            vm_name,
            tenant: None,
            backend_name: "firecracker",
            image_name: "img",
            image_sha256: FIXTURE_SHA,
            image_cosign_bundle: None,
            cpus: 1,
            mem_mib: 256,
            disk_mib: 0,
            boot_timeout_secs: 30,
            exec_timeout_secs: 0,
            destroy_on_exit: true,
        }
    }

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    #[test]
    fn happy_path_returns_admitted_plan_with_plan_id() {
        let dir = tempfile::tempdir().unwrap();
        let clock = SystemClock;
        let ledger = InMemoryNonceLedger::new();
        let admitted = admit_for_run(&fixture_input("vm1"), &clock, &ledger, Some(dir.path()))
            .expect("happy path");
        assert!(!admitted.plan_id.0.is_empty());
        assert!(admitted.signer_id.starts_with("host:"));
        // The signed envelope must be re-verifiable with the public
        // half of the host signer.
        let signer = super::super::host_signer::load_or_init_at(dir.path()).unwrap();
        let trusted: [(&str, &ed25519_dalek::VerifyingKey); 1] =
            [(&admitted.signer_id, &signer.verifying)];
        let recovered = mvm_plan::verify_plan(&admitted.signed, &trusted).unwrap();
        assert_eq!(recovered.plan_id, admitted.plan_id);
    }

    #[test]
    fn rejects_replay_within_validity_window() {
        let dir = tempfile::tempdir().unwrap();
        // We can't naturally replay because synthesize_plan generates a
        // fresh nonce each call — instead, build the plan once, sign,
        // then ask the ledger to admit twice. The second call must
        // refuse with nonce-replay.
        let plan = synthesize_plan(&fixture_input("vm1")).unwrap();
        let signer = super::super::host_signer::load_or_init_at(dir.path()).unwrap();
        let signer_id = host_signer_id();
        let signed = sign_plan(&plan, &signer.signing, &signer_id);
        let verified = mvm_plan::verify_plan(&signed, &[(&signer_id, &signer.verifying)]).unwrap();

        let ledger = InMemoryNonceLedger::new();
        {
            let mut store = ledger.inner.lock().unwrap();
            assert!(store.check_and_insert(&signer_id, &verified).is_ok());
            assert!(
                store.check_and_insert(&signer_id, &verified).is_err(),
                "second insert of same (signer, nonce) must fail"
            );
        }
    }

    #[test]
    fn rejects_plan_outside_validity_window() {
        // Construct a fixed clock 30 minutes in the future — past
        // the plan's 10-minute window from synthesis.
        let now_plus_30 = Utc.with_ymd_and_hms(2099, 1, 1, 12, 30, 0).unwrap();
        // Override time by constructing a plan with a known window
        // and a FixedClock outside it.
        let dir = tempfile::tempdir().unwrap();
        // To exercise the window check deterministically, we
        // pre-build a stale signed plan and feed it directly through
        // the check (synthesize_plan can't make a stale plan because
        // it uses Utc::now()).
        let mut plan = synthesize_plan(&fixture_input("vm1")).unwrap();
        plan.valid_from = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        plan.valid_until = Utc.with_ymd_and_hms(2000, 1, 1, 0, 10, 0).unwrap();
        let signer = super::super::host_signer::load_or_init_at(dir.path()).unwrap();
        let signed = sign_plan(&plan, &signer.signing, &host_signer_id());
        let verified = verify_plan(&signed, &[(&host_signer_id(), &signer.verifying)]).unwrap();
        let _clock = FixedClock(now_plus_30);
        // Inline the window check (admit_for_run does it after
        // synthesis; we're proving the underlying assert).
        assert!(check_window(&verified, now_plus_30).is_err());
    }

    #[test]
    fn admitted_plan_signed_field_round_trips_through_verify() {
        let dir = tempfile::tempdir().unwrap();
        let admitted = admit_for_run(
            &fixture_input("vm1"),
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
        )
        .unwrap();
        // The signed field is what W4's audit signer will hash;
        // proving it round-trips here closes the contract.
        let signer = super::super::host_signer::load_or_init_at(dir.path()).unwrap();
        let trusted: [(&str, &ed25519_dalek::VerifyingKey); 1] =
            [(&admitted.signer_id, &signer.verifying)];
        assert!(verify_plan(&admitted.signed, &trusted).is_ok());
    }

    #[test]
    fn propagates_synthesis_failures() {
        let dir = tempfile::tempdir().unwrap();
        let err = admit_for_run(
            &fixture_input(""), // empty vm_name fails synthesis
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
        )
        .expect_err("must refuse");
        assert!(
            err.to_string().contains("vm_name")
                || err.chain().any(|e| e.to_string().contains("vm_name"))
        );
    }

    #[test]
    fn two_distinct_admit_calls_produce_distinct_plan_ids_and_nonces() {
        let dir = tempfile::tempdir().unwrap();
        let clock = SystemClock;
        let ledger = InMemoryNonceLedger::new();
        let a1 = admit_for_run(&fixture_input("vm1"), &clock, &ledger, Some(dir.path())).unwrap();
        let a2 = admit_for_run(&fixture_input("vm1"), &clock, &ledger, Some(dir.path())).unwrap();
        assert_ne!(a1.plan_id, a2.plan_id);
        assert_ne!(a1.plan.nonce, a2.plan.nonce);
    }
}
