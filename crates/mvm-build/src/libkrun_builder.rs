//! Libkrun-backed builder VM (Plan 72 W1 scaffolding).
//!
//! Plan 72 ADR-046 chose libkrun-direct (on macOS Apple Silicon /
//! Intel) and Firecracker (on Linux) as the replacement for the
//! microsandbox-backed builder VM. This module is the libkrun half.
//!
//! ## Status — Plan 72 W1 (scaffolding)
//!
//! What W1 ships:
//!
//! - The `LibkrunBuilderVm` struct + `BuilderVm` trait impl shape.
//! - Resource defaults matching Plan 72 §W1 (4 vCPU, 4 GiB RAM,
//!   64 GiB sparse virtio-blk for the persistent `/nix` store).
//! - Mount-validation (existence, UTF-8 representability for the
//!   libkrun C API boundary, artifact-dir creation).
//! - Host probe (`host_can_build`) that consults libkrun's
//!   `is_available()` so callers can sanity-check the environment
//!   before invoking `run_build`.
//!
//! What W1 does NOT ship (deferred to W2–W4):
//!
//! - The builder VM image acquisition (W2 — `nix/images/builder-vm/`
//!   flake + CI release artifact + `~/.cache/mvm/builder-vm/<arch>/`
//!   cache).
//! - The `mvm-builder-init` PID-1 binary (W3).
//! - virtio-fs / virtio-blk / vsock plumbing for `/work`, `/out`,
//!   `/job`, `/nix-store` mounts (W4).
//! - The actual `mvm_libkrun::start_enter` invocation +
//!   power-off detection + job-result extraction (W4 + W5 cutover).
//!
//! Until W2–W4 land, `run_build` returns
//! [`BuilderVmError::LibkrunNotShipped`] after validation, so callers
//! can wire dispatch and exercise the error path in tests; the
//! data-plane fills in incrementally.
//!
//! ## Feature gate
//!
//! Gated behind `backends-builder-vm-libkrun`. Default-off until W5's
//! cutover flips the polarity. Library consumers that don't need the
//! libkrun builder build with `default-features = false`.
//!
//! ## Not the runtime backend
//!
//! `LibkrunBackend` (`crates/mvm-backend/src/libkrun.rs`) is for
//! running user microVMs; this module is for building them. The two
//! share `mvm-libkrun`'s FFI but compose differently — the builder
//! mounts a workspace + persistent `/nix`-store disk and runs a
//! one-shot `nix build`, while the runtime mounts the user's rootfs
//! and runs the user's entrypoint.

use crate::builder_vm::{BuilderArtifacts, BuilderJob, BuilderMounts, BuilderVm, BuilderVmError};

/// Default vCPU count for the builder VM. Nix builds are
/// embarrassingly parallel at the derivation level; 4 cores is the
/// sweet spot on M-series Macs without saturating the host.
pub const DEFAULT_VCPUS: u8 = 4;

/// Default RAM in MiB. Nix evaluation peaks around 2.5 GiB for the
/// dev image's closure; 4 GiB leaves headroom for parallel
/// derivation builds without swapping.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// Default size of the persistent `/nix`-store virtio-blk image,
/// in MiB. 64 GiB sparse — the file only consumes the bytes the
/// in-VM ext4 actually writes, but capacity caps growth so a
/// runaway build can't fill the host disk.
pub const DEFAULT_NIX_STORE_MIB: u32 = 65536;

/// Where the workspace gets mounted inside the builder VM
/// (read-only virtio-fs). Plan 72 W4 wires this.
pub const GUEST_WORK_DIR: &str = "/work";

/// Where artifacts get extracted inside the builder VM (read-write
/// virtio-fs). Plan 72 W4 wires this.
pub const GUEST_OUT_DIR: &str = "/out";

/// Where the persistent Nix store lives inside the builder VM. The
/// `mvm-builder-init` PID-1 (Plan 72 W3) bind-mounts the virtio-blk
/// device at this path before exec-ing the build script.
pub const GUEST_NIX_DIR: &str = "/nix";

/// Where the per-build job spec lives inside the builder VM. The
/// host stages `cmd.sh`, `env`, and the eventual `result` file
/// under this path (read-write virtio-fs). Plan 72 W4 wires this.
pub const GUEST_JOB_DIR: &str = "/job";

/// Libkrun-backed builder VM driver.
///
/// Configuration only — `run_build` consumes it to spin a per-job
/// VM, runs `nix build` inside, extracts the artifacts via the
/// `/out` virtio-fs mount, and tears the VM down. No persistent
/// state on the struct; the `/nix`-store image lives on the host
/// filesystem and survives across invocations.
#[derive(Debug, Clone, Copy)]
pub struct LibkrunBuilderVm {
    /// Guest vCPU count. See [`DEFAULT_VCPUS`].
    pub vcpus: u8,
    /// Guest RAM in MiB. See [`DEFAULT_MEMORY_MIB`].
    pub memory_mib: u32,
    /// Persistent `/nix`-store image size in MiB (sparse cap).
    /// See [`DEFAULT_NIX_STORE_MIB`].
    pub nix_store_mib: u32,
}

impl Default for LibkrunBuilderVm {
    fn default() -> Self {
        Self {
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            nix_store_mib: DEFAULT_NIX_STORE_MIB,
        }
    }
}

impl LibkrunBuilderVm {
    /// Override the default vCPU / RAM pair. Useful for CI runners
    /// or low-memory hosts that can't afford the 4 GiB default.
    pub fn with_resources(mut self, vcpus: u8, memory_mib: u32) -> Self {
        self.vcpus = vcpus;
        self.memory_mib = memory_mib;
        self
    }

    /// Override the default `/nix`-store image cap. Smaller for
    /// CI runners that build a known-small closure; larger for
    /// developer hosts that want to keep many tenants' artifacts
    /// in one warm store.
    pub fn with_nix_store_mib(mut self, mib: u32) -> Self {
        self.nix_store_mib = mib;
        self
    }

    /// Validate caller-supplied mount paths early. Catches issues
    /// that would otherwise surface as opaque libkrun C-API
    /// failures: missing directories, non-UTF-8 paths (libkrun's
    /// FFI takes `*const c_char` and we'd hit `CString::new`
    /// failures inside `mvm_libkrun::sys` otherwise), and
    /// uncreatable artifact dirs.
    ///
    /// Public-in-crate so unit tests can exercise it without
    /// triggering the W1 not-shipped trip-wire below.
    pub(crate) fn validate_mounts(&self, mounts: &BuilderMounts) -> Result<(), BuilderVmError> {
        // Reject non-UTF-8 paths first — libkrun's C API takes
        // `*const c_char` and we want the error message pinned to
        // the offending field rather than at a CString conversion
        // deep inside the FFI. Cheap predicate; runs before any
        // I/O so a test can exercise it on a synthetic path
        // without filesystem support for non-UTF-8 names (APFS).
        ensure_utf8_path(&mounts.flake_src, "flake_src")?;
        ensure_utf8_path(&mounts.artifact_out, "artifact_out")?;
        if let Some(store) = &mounts.host_nix_store {
            ensure_utf8_path(store, "host_nix_store")?;
        }
        if !mounts.flake_src.exists() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "flake source path does not exist: {}",
                mounts.flake_src.display()
            )));
        }
        if !mounts.flake_src.is_dir() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "flake source must be a directory: {}",
                mounts.flake_src.display()
            )));
        }
        std::fs::create_dir_all(&mounts.artifact_out).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating artifact_out {}: {e}",
                mounts.artifact_out.display()
            ))
        })?;
        Ok(())
    }

    /// Validate the job description. Both fields must be non-empty
    /// strings; `flake_ref` may include a `path:` or `git+` prefix
    /// but the prefix-less form is also accepted (libkrun runs the
    /// command verbatim inside the builder VM, where the file
    /// system is paths libkrun mounted).
    pub(crate) fn validate_job(&self, job: &BuilderJob) -> Result<(), BuilderVmError> {
        if job.flake_ref.trim().is_empty() {
            return Err(BuilderVmError::NixBuildFailed(
                "BuilderJob.flake_ref is empty".to_string(),
            ));
        }
        if job.attr_path.trim().is_empty() {
            return Err(BuilderVmError::NixBuildFailed(
                "BuilderJob.attr_path is empty".to_string(),
            ));
        }
        Ok(())
    }
}

/// Reject a path that isn't UTF-8 representable. Internal helper —
/// the libkrun FFI requires CString-convertible paths and we want
/// the failure pinned to the offending field with a useful name.
fn ensure_utf8_path(p: &std::path::Path, field: &str) -> Result<(), BuilderVmError> {
    p.to_str().ok_or_else(|| {
        BuilderVmError::ExtractionFailed(format!("{field} has non-UTF-8 bytes: {p:?}"))
    })?;
    Ok(())
}

impl BuilderVm for LibkrunBuilderVm {
    fn host_can_build(&self) -> Result<bool, BuilderVmError> {
        // libkrun never satisfies the "host can build Linux
        // derivations directly" predicate — by definition we run
        // the VM. Returning `false` makes the dispatch in
        // `ensure_dev_image` fall through to `run_build` rather
        // than short-circuiting to host Nix (forbidden anyway per
        // CLAUDE.md §"Host Nix is never used by mvmctl"). When
        // libkrun isn't installed the call site can still consult
        // `mvm_libkrun::is_available()` for a clearer error.
        Ok(false)
    }

    fn run_build(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        // Step 1: catch bad input now, so W2–W4 don't have to
        // re-validate on the FFI boundary.
        self.validate_mounts(mounts)?;
        self.validate_job(job)?;

        // Step 2: confirm libkrun is actually installed. Even
        // when the `backends-builder-vm-libkrun` feature is on,
        // the user might be on a host without the shared
        // library; surface that here rather than at the FFI.
        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::MicrosandboxUnavailable(format!(
                "libkrun shared library not found on host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        // Step 3 (W2): resolve the builder VM image (kernel +
        //   rootfs.ext4) — `nix/images/builder-vm/flake.nix` in a
        //   source checkout, downloaded prebuilt for installed
        //   binaries.
        // Step 4 (W3): stage `cmd.sh` + `env` under the job dir,
        //   allocate the per-build `/nix`-store sparse image if
        //   the cache miss.
        // Step 5 (W4): build the `KrunContext` with virtio-fs +
        //   virtio-blk + vsock mounts, call
        //   `mvm_libkrun::start_enter`, poll for power-off,
        //   read `/job/result`.
        // Step 6 (W4): validate `mounts.artifact_out` now
        //   contains `vmlinux` + `rootfs.ext4` and construct
        //   `BuilderArtifacts`.
        let _ = (job, mounts);
        Err(BuilderVmError::LibkrunNotShipped)
    }

    fn cleanup(&self) -> Result<(), BuilderVmError> {
        // Plan 72 W6 hygiene: prune old job dirs under
        // `~/.cache/mvm/builder-vm/jobs/` past N days. No-op
        // until W6 picks the retention policy.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ok_mounts(scratch: &TempDir) -> BuilderMounts {
        let flake = scratch.path().join("flake");
        std::fs::create_dir_all(&flake).unwrap();
        let out = scratch.path().join("out");
        BuilderMounts {
            flake_src: flake,
            host_nix_store: None,
            artifact_out: out,
        }
    }

    fn ok_job() -> BuilderJob {
        BuilderJob {
            flake_ref: "path:/work".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        }
    }

    #[test]
    fn defaults_match_plan_72_w1() {
        let vm = LibkrunBuilderVm::default();
        assert_eq!(vm.vcpus, 4);
        assert_eq!(vm.memory_mib, 4096);
        assert_eq!(vm.nix_store_mib, 65536);
    }

    #[test]
    fn host_can_build_always_false() {
        // libkrun never short-circuits to host Nix.
        let vm = LibkrunBuilderVm::default();
        assert!(!vm.host_can_build().unwrap());
    }

    #[test]
    fn validate_mounts_rejects_missing_flake_src() {
        let scratch = TempDir::new().unwrap();
        let mounts = BuilderMounts {
            flake_src: scratch.path().join("does-not-exist"),
            host_nix_store: None,
            artifact_out: scratch.path().join("out"),
        };
        let err = LibkrunBuilderVm::default()
            .validate_mounts(&mounts)
            .unwrap_err();
        assert!(matches!(err, BuilderVmError::ExtractionFailed(_)));
        assert!(format!("{err}").contains("does not exist"));
    }

    #[test]
    fn validate_mounts_rejects_flake_src_that_is_a_file() {
        let scratch = TempDir::new().unwrap();
        let file = scratch.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let mounts = BuilderMounts {
            flake_src: file,
            host_nix_store: None,
            artifact_out: scratch.path().join("out"),
        };
        let err = LibkrunBuilderVm::default()
            .validate_mounts(&mounts)
            .unwrap_err();
        assert!(format!("{err}").contains("must be a directory"));
    }

    #[test]
    fn validate_mounts_creates_artifact_out_if_missing() {
        let scratch = TempDir::new().unwrap();
        let mounts = ok_mounts(&scratch);
        assert!(!mounts.artifact_out.exists());
        LibkrunBuilderVm::default()
            .validate_mounts(&mounts)
            .unwrap();
        assert!(mounts.artifact_out.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn validate_mounts_rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::PathBuf;

        // Synthesize a PathBuf with non-UTF-8 bytes in memory.
        // 0xFF is invalid UTF-8 (RFC 3629 says the byte cannot
        // appear in a valid UTF-8 sequence). We don't touch the
        // filesystem because APFS refuses to create files with
        // non-UTF-8 names; the validator's UTF-8 check runs
        // before any I/O so this still exercises the right path.
        let raw = OsStr::from_bytes(b"/tmp/non-utf8-\xff");
        let bad_path = PathBuf::from(raw);
        let mounts = BuilderMounts {
            flake_src: bad_path,
            host_nix_store: None,
            artifact_out: std::env::temp_dir().join("mvm-plan72-w1-utf8-test-out"),
        };
        let err = LibkrunBuilderVm::default()
            .validate_mounts(&mounts)
            .unwrap_err();
        assert!(
            format!("{err}").contains("non-UTF-8 bytes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_job_rejects_empty_flake_ref() {
        let job = BuilderJob {
            flake_ref: "".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let err = LibkrunBuilderVm::default().validate_job(&job).unwrap_err();
        assert!(format!("{err}").contains("flake_ref"));
    }

    #[test]
    fn validate_job_rejects_whitespace_only_attr_path() {
        let job = BuilderJob {
            flake_ref: "path:/work".to_string(),
            attr_path: "   ".to_string(),
        };
        let err = LibkrunBuilderVm::default().validate_job(&job).unwrap_err();
        assert!(format!("{err}").contains("attr_path"));
    }

    #[test]
    fn run_build_validates_before_returning_not_shipped() {
        // Bad input → validation error, not the W1 trip-wire.
        let scratch = TempDir::new().unwrap();
        let mounts = BuilderMounts {
            flake_src: scratch.path().join("missing"),
            host_nix_store: None,
            artifact_out: scratch.path().join("out"),
        };
        let err = LibkrunBuilderVm::default()
            .run_build(&ok_job(), &mounts)
            .unwrap_err();
        assert!(matches!(err, BuilderVmError::ExtractionFailed(_)));
    }

    #[test]
    fn run_build_returns_not_shipped_or_unavailable_on_clean_input() {
        // Good input → either LibkrunNotShipped (libkrun installed,
        // W2–W4 not yet implemented) or MicrosandboxUnavailable
        // (libkrun not installed). The CI matrix runs both states;
        // either outcome is the W1 acceptance shape.
        let scratch = TempDir::new().unwrap();
        let mounts = ok_mounts(&scratch);
        let err = LibkrunBuilderVm::default()
            .run_build(&ok_job(), &mounts)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BuilderVmError::LibkrunNotShipped | BuilderVmError::MicrosandboxUnavailable(_)
            ),
            "unexpected error variant: {err:?}"
        );
    }

    #[test]
    fn with_resources_overrides() {
        let vm = LibkrunBuilderVm::default().with_resources(2, 2048);
        assert_eq!(vm.vcpus, 2);
        assert_eq!(vm.memory_mib, 2048);
        assert_eq!(vm.nix_store_mib, 65536); // unchanged
    }

    #[test]
    fn with_nix_store_mib_overrides() {
        let vm = LibkrunBuilderVm::default().with_nix_store_mib(8192);
        assert_eq!(vm.nix_store_mib, 8192);
    }
}
