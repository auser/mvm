//! Live boot orchestration for `mvmctl bench microvm-launch`. Kept
//! out of `bench.rs` so the pure stats/schema substrate stays
//! VM-free. See Plan 93 PR-10a
//! (`specs/plans/93-pr10a-live-bench-probe-impl-plan.md`).

use anyhow::{Context, Result};

use mvm_plan::{PlanSeccompTier, SecretReleasePolicy};

use crate::commands::env::apple_container::ensure_default_microvm_image;
use crate::commands::vm::plan_admission::{
    AdmittedPlan, InMemoryNonceLedger, SystemClock, admit_for_run,
};
use crate::commands::vm::plan_builder::SynthesisInput;

/// Resolved inputs for one benchmarked boot. `kernel`/`rootfs` come
/// from the same `ensure_default_microvm_image()` `mvmctl up` uses —
/// the canonical runtime image, NOT the dev-shell rootfs.
// Fields are read by Task 5's live `boot_measure_once` + Task 9's
// HostDescriptor kernel-sha; until then only the test reads them.
#[allow(dead_code)]
pub struct ProbeImage {
    pub kernel: String,
    pub rootfs: String,
}

/// Resolve the canonical default-microvm image (kernel + rootfs) the
/// same way `mvmctl up` does. No artifact override flags: the bench
/// measures the real runtime launch path, so it pins to one canonical
/// target (a `HostDescriptor`-comparable baseline).
#[allow(dead_code)]
pub fn resolve_probe_image() -> Result<ProbeImage> {
    let (kernel, rootfs) =
        ensure_default_microvm_image().context("resolving default-microvm bench image")?;
    Ok(ProbeImage { kernel, rootfs })
}

/// Synthesize → sign → verify → window → nonce a minimal plan for the
/// probe's boot, mirroring `up.rs::admit_plan_for_boot` minus bundle /
/// deps / policy. `keys_dir` is the host-signer directory; production
/// callers pass `~/.mvm/keys/`, tests pass a tempdir so they never
/// touch the real user's home. Drives the real claim-8 admission path
/// — the bench must never benchmark a boot that bypasses admission.
#[allow(dead_code)]
pub fn admit_probe_plan(
    rootfs: &std::path::Path,
    vm_name: &str,
    keys_dir: &std::path::Path,
) -> Result<AdmittedPlan> {
    let sha = mvm_security::image_verify::sha256_file(rootfs)
        .with_context(|| format!("hashing probe rootfs {}", rootfs.display()))?;
    let input = SynthesisInput {
        vm_name,
        tenant: Some("bench"),
        backend_name: "libkrun",
        image_name: vm_name,
        image_sha256: &sha,
        image_cosign_bundle: None,
        intent: None,
        seccomp_tier: PlanSeccompTier::Standard,
        network_policy_ref: None,
        fs_policy_ref: None,
        egress_policy_ref: None,
        tool_policy_ref: None,
        secret_release: SecretReleasePolicy::default(),
        secrets: Vec::new(),
        audit_event_prefix: None,
        cpus: 2,
        mem_mib: 512,
        disk_mib: 0,
        boot_timeout_secs: 60,
        exec_timeout_secs: 0,
        destroy_on_exit: true,
        bundle_pin: None,
        deps_volume: None,
    };
    let ledger = InMemoryNonceLedger::new();
    admit_for_run(&input, &SystemClock, &ledger, Some(keys_dir), None)
        .context("admitting probe plan")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "touches ~/.cache/mvm; run on a host with the image cached"]
    fn resolve_probe_image_returns_existing_paths() {
        let img = resolve_probe_image().unwrap();
        assert!(std::path::Path::new(&img.kernel).exists());
        assert!(std::path::Path::new(&img.rootfs).exists());
    }

    #[test]
    fn admit_probe_plan_produces_admitted_plan_with_tempdir_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"not a real rootfs but hashable").unwrap();
        let admitted = admit_probe_plan(&rootfs, "bench-probe", tmp.path()).unwrap();
        // The admitted plan binds the workload name we passed.
        assert_eq!(admitted.plan.image.name, "bench-probe");
    }
}
