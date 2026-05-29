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
use mvm_plan::bundle::{BundleResolver, TrustStore};
use mvm_plan::{
    ExecutionPlan, NonceStore, PlanId, PlanValidityError, SignedExecutionPlan, check_window,
    sign_plan, verify_plan, verify_plan_bundle,
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

/// Optional bundle-admission context for plans pinned to a
/// `.mvmpkg`. Carries the resolver (where to find the archive
/// bytes by sha256) and the trust store (which publisher pubkeys
/// to accept). `admit_for_run` ignores it when the plan has no
/// pin; rejects when the plan has a pin but the context is
/// `None` (operator misconfiguration); runs full re-verify when
/// both are present.
pub struct BundleAdmissionContext<'a> {
    pub resolver: &'a dyn BundleResolver,
    pub trust: &'a dyn TrustStore,
}

/// Run the full admission pipeline for an `mvmctl up` invocation.
///
/// On success, the caller proceeds to `backend.start()` knowing the
/// plan was signed under the host signer, verified with the host's
/// own public key, satisfies its own validity window, hasn't been
/// admitted before (replay protection), and — when the plan pins a
/// `.mvmpkg` bundle — the on-disk archive matches the pin
/// byte-for-byte and verifies under the trust store.
///
/// On failure, the user gets a clear error per failure class:
///   - `tenant must not be empty` / `vm_name must not be empty` —
///     synthesis-time guard
///   - `host signer at {path} has mode {found}; expected 0600` —
///     keystore guard
///   - `plan validity window violated: {detail}` — G4 window check
///   - `plan replay detected for signer {id}; nonce {hex}` — G4 nonce
///   - `bundle re-verify failed: {detail}` — pinned bundle missing,
///     unknown publisher, tampered, or sha256/sig/key_id mismatch
pub fn admit_for_run(
    input: &SynthesisInput<'_>,
    clock: &dyn Clock,
    ledger: &InMemoryNonceLedger,
    host_signer_keys_dir: Option<&std::path::Path>,
    bundle_ctx: Option<&BundleAdmissionContext<'_>>,
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

    // ADR-002 claim 9 — bundle re-verify at admit time. Only fires
    // when the plan pinned a bundle; missing context with a pinned
    // plan is operator misconfiguration (mvmctl up wasn't wired
    // with a resolver/trust store), so we refuse rather than skip
    // silently.
    if let Some(pin) = verified.bundle.as_ref() {
        let ctx = bundle_ctx.ok_or_else(|| {
            anyhow::anyhow!(
                "plan pins bundle {bundle} but no BundleAdmissionContext was provided — refuse",
                bundle = pin.bundle_sha256
            )
        })?;
        verify_plan_bundle(pin, ctx.resolver, ctx.trust)
            .with_context(|| format!("bundle re-verify for pin {}", pin.bundle_sha256))?;
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
    use mvm_plan::{PlanSeccompTier, SecretReleasePolicy};

    const FIXTURE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fixture_input(vm_name: &str) -> SynthesisInput<'_> {
        SynthesisInput {
            vm_name,
            tenant: None,
            backend_name: "firecracker",
            image_name: "img",
            image_sha256: FIXTURE_SHA,
            image_cosign_bundle: None,
            intent: None,
            seccomp_tier: PlanSeccompTier::Standard,
            network_policy_ref: None,
            fs_policy_ref: None,
            egress_policy_ref: None,
            tool_policy_ref: None,
            secret_release: SecretReleasePolicy::None,
            secrets: Vec::new(),
            audit_event_prefix: None,
            cpus: 1,
            mem_mib: 256,
            disk_mib: 0,
            boot_timeout_secs: 30,
            exec_timeout_secs: 0,
            destroy_on_exit: true,
            bundle_pin: None,
            deps_volume: None,
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
        let admitted = admit_for_run(
            &fixture_input("vm1"),
            &clock,
            &ledger,
            Some(dir.path()),
            None,
        )
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
            None,
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
            None,
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
        let a1 = admit_for_run(
            &fixture_input("vm1"),
            &clock,
            &ledger,
            Some(dir.path()),
            None,
        )
        .unwrap();
        let a2 = admit_for_run(
            &fixture_input("vm1"),
            &clock,
            &ledger,
            Some(dir.path()),
            None,
        )
        .unwrap();
        assert_ne!(a1.plan_id, a2.plan_id);
        assert_ne!(a1.plan.nonce, a2.plan.nonce);
    }

    // ── ADR-002 claim 9: admit-time bundle re-verify ─────────────
    //
    // Tests exercise the boundary between `synthesize_plan`'s
    // `bundle_pin` (the input) and `admit_for_run`'s
    // `BundleAdmissionContext` (the verifier seam). The mvm_plan
    // bundle module already tests every BundleVerifyError /
    // PlanBundleError variant in isolation; these tests prove the
    // wiring fires when admit_for_run sees a pinned plan.

    use mvm_plan::bundle::{
        BundleResolveError, BundleResolver, KeyId as BundleKeyId, PlanArtifact, TrustStore,
        bundle_sha256, write_bundle,
    };
    use std::collections::HashMap;

    struct FixedResolver(Vec<u8>);
    impl BundleResolver for FixedResolver {
        fn resolve(&self, _bundle_sha256: &str) -> Result<Vec<u8>, BundleResolveError> {
            Ok(self.0.clone())
        }
    }

    struct MapTrust(HashMap<BundleKeyId, ed25519_dalek::VerifyingKey>);
    impl TrustStore for MapTrust {
        fn lookup(&self, key_id: &BundleKeyId) -> Option<ed25519_dalek::VerifyingKey> {
            self.0.get(key_id).copied()
        }
    }

    /// Build a minimal signed bundle around `(kernel, rootfs)` bytes.
    /// Returns the archive plus the matching `PlanArtifact` pin.
    fn make_test_bundle(
        sk: &ed25519_dalek::SigningKey,
        kernel: &[u8],
        rootfs: &[u8],
    ) -> (Vec<u8>, PlanArtifact) {
        use mvm_plan::bundle::{
            ARTIFACTS_DIR, ArtifactRole, BUNDLE_SCHEMA_VERSION, BundleArtifact, BundleManifest,
            sha256_hex,
        };
        let key_id = BundleKeyId::from_pubkey(&sk.verifying_key());
        let make_art = |name: &str, role: ArtifactRole, bytes: &[u8]| BundleArtifact {
            name: name.to_string(),
            role,
            path: format!("{ARTIFACTS_DIR}/{name}"),
            sha256: sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
        };
        let manifest = BundleManifest {
            schema_version: BUNDLE_SCHEMA_VERSION,
            publisher: "test".to_string(),
            key_id: key_id.clone(),
            arch: "aarch64".to_string(),
            kernel_version: None,
            profile: None,
            workload_label: None,
            created_at: "2026-05-13T00:00:00Z".to_string(),
            labels: std::collections::BTreeMap::new(),
            artifacts: vec![
                make_art("vmlinux", ArtifactRole::Kernel, kernel),
                make_art("rootfs.ext4", ArtifactRole::Rootfs, rootfs),
            ],
            verity: None,
            resources: None,
        };
        let archive = write_bundle(
            &manifest,
            sk,
            vec![
                (format!("{ARTIFACTS_DIR}/vmlinux"), kernel.to_vec()),
                (format!("{ARTIFACTS_DIR}/rootfs.ext4"), rootfs.to_vec()),
            ],
        )
        .expect("write_bundle");

        // Recover the signature bytes from the archive for the pin.
        let mut sig_bytes: Vec<u8> = Vec::new();
        let mut a = tar::Archive::new(std::io::Cursor::new(&archive));
        for entry in a.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == "manifest.sig" {
                std::io::Read::read_to_end(&mut entry, &mut sig_bytes).unwrap();
                break;
            }
        }
        let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        let pin = PlanArtifact::new(bundle_sha256(&archive), &sig_arr, key_id);
        (archive, pin)
    }

    fn input_with_pin<'a>(vm_name: &'a str, pin: &PlanArtifact) -> SynthesisInput<'a> {
        let mut input = fixture_input(vm_name);
        input.bundle_pin = Some(pin.clone());
        input
    }

    #[test]
    fn admit_with_clean_pinned_bundle_passes() {
        // Generate the publisher key out of band, build a bundle,
        // enrol the pubkey in the trust store, hand admit_for_run a
        // matching pin + context.
        let dir = tempfile::tempdir().unwrap();
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let (archive, pin) = make_test_bundle(&sk, b"kernel-bytes", b"rootfs-bytes");
        let mut map = HashMap::new();
        let key_id = BundleKeyId::from_pubkey(&sk.verifying_key());
        map.insert(key_id, sk.verifying_key());
        let trust = MapTrust(map);
        let resolver = FixedResolver(archive);
        let ctx = BundleAdmissionContext {
            resolver: &resolver,
            trust: &trust,
        };
        let admitted = admit_for_run(
            &input_with_pin("vm-pinned", &pin),
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
            Some(&ctx),
        )
        .expect("clean pin admits");
        assert!(admitted.plan.bundle.is_some());
    }

    #[test]
    fn admit_with_pin_but_no_context_refuses() {
        // Publisher misconfiguration: plan carries a pin but the
        // mvmctl up path didn't wire a BundleAdmissionContext. The
        // admit path refuses rather than silently skipping the
        // re-verify step (fail closed, not fail open).
        let dir = tempfile::tempdir().unwrap();
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let (_archive, pin) = make_test_bundle(&sk, b"k", b"r");
        let err = admit_for_run(
            &input_with_pin("vm-no-ctx", &pin),
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
            None,
        )
        .expect_err("must refuse without context");
        let msg = format!("{err:#}");
        assert!(msg.contains("BundleAdmissionContext"), "got: {msg}");
    }

    #[test]
    fn admit_with_unknown_publisher_in_trust_store_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let (archive, pin) = make_test_bundle(&sk, b"k", b"r");
        // Empty trust store — publisher's key_id is unknown locally.
        let trust = MapTrust(HashMap::new());
        let resolver = FixedResolver(archive);
        let ctx = BundleAdmissionContext {
            resolver: &resolver,
            trust: &trust,
        };
        let err = admit_for_run(
            &input_with_pin("vm-untrusted", &pin),
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
            Some(&ctx),
        )
        .expect_err("must refuse unknown publisher");
        let msg = format!("{err:#}");
        // The error chain bubbles up the BundleVerifyError::UnknownKey
        // variant from the read_and_verify pass.
        assert!(
            err.chain().any(|e| e.to_string().contains("key_id")),
            "expected key_id mention; got: {msg}"
        );
    }

    #[test]
    fn admit_with_pin_mismatching_archive_refuses() {
        // Resolver returns a different archive than the pin describes.
        // The bundle_sha256 cross-check catches it before the
        // signature verify even runs.
        let dir = tempfile::tempdir().unwrap();
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let (_archive_a, pin_a) = make_test_bundle(&sk, b"kA", b"rA");
        let (archive_b, _pin_b) = make_test_bundle(&sk, b"kB", b"rB");
        let mut map = HashMap::new();
        map.insert(
            BundleKeyId::from_pubkey(&sk.verifying_key()),
            sk.verifying_key(),
        );
        let trust = MapTrust(map);
        let resolver = FixedResolver(archive_b);
        let ctx = BundleAdmissionContext {
            resolver: &resolver,
            trust: &trust,
        };
        let err = admit_for_run(
            &input_with_pin("vm-pin-drift", &pin_a),
            &SystemClock,
            &InMemoryNonceLedger::new(),
            Some(dir.path()),
            Some(&ctx),
        )
        .expect_err("must refuse pin drift");
        assert!(
            err.chain().any(|e| e.to_string().contains("sha256")),
            "expected sha256 mismatch chain; got {err:#}"
        );
    }
}
