//! Live boot orchestration for `mvmctl bench microvm-launch`. Kept
//! out of `bench.rs` so the pure stats/schema substrate stays
//! VM-free. See Plan 93 PR-10a
//! (`specs/plans/93-pr10a-live-bench-probe-impl-plan.md`).

use anyhow::{Context, Result};

use crate::commands::env::apple_container::ensure_default_microvm_image;

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
}
