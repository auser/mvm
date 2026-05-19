//! Libkrun-backed builder VM.
//!
//! Plan 72 ADR-046 chose libkrun-direct (on macOS Apple Silicon /
//! Intel) and Firecracker (on Linux) as the replacement for the
//! libkrun-backed builder VM. This module is the libkrun half;
//! W1 → W4 of the migration shipped the launcher end-to-end.
//!
//! ## What `LibkrunBuilderVm` does
//!
//! Given a populated builder VM image cache and `mvm-libkrun-supervisor`
//! on PATH, `run_build` runs a one-shot `nix build` against the
//! caller's `BuilderJob` and returns `BuilderArtifacts`. The
//! pipeline (in [`BuilderVm::run_build`]):
//!
//! 1. Validate mounts + job (`validate_mounts`, `validate_job`).
//! 2. Check `mvm_libkrun::is_available()` — bail with install hint
//!    if libkrun isn't on the host.
//! 3. Locate `mvm-libkrun-supervisor` (env override / next-to-exe /
//!    PATH).
//! 4. Read the builder VM image from
//!    `~/.cache/mvm/builder-vm/<arch>/` — vmlinux + rootfs.ext4 +
//!    cmdline.txt + manifest.json, the shape Plan 72 W2's flake
//!    emits. Populated by Plan 72 W5's `bootstrap_builder_vm_image`
//!    (`mvm-cli::commands::env::apple_container`).
//! 5. Allocate / reuse the persistent `/nix-store-<arch>.img`
//!    sparse virtio-blk image (64 GiB cap by default; idempotent
//!    across invocations so the warm Nix store survives).
//! 6. Stage `<job_dir>/cmd.sh` with the shell-escaped flake_ref +
//!    attr_path, plus the canonical `nix build` invocation.
//! 7. Build a `KrunContext`: kernel + rootfs + cmdline + per-VM
//!    vsock dir + virtio-blk (Nix store) + virtio-fs (work / out /
//!    job).
//! 8. Spawn `mvm-libkrun-supervisor`, pipe the `SupervisorConfig`
//!    JSON to stdin, **wait** for it to exit (unlike
//!    `LibkrunBackend::start` which returns after the PID file
//!    appears — the builder is a one-shot).
//! 9. Read `<job_dir>/result` (JSON: `{exit_code, stderr_tail}`)
//!    that `mvm-builder-init` wrote.
//! 10. Validate the artifact dir now contains `rootfs.ext4` (and
//!     optionally `vmlinux`); return `BuilderArtifacts`.
//!
//! ## Feature gate
//!
//! Gated behind `builder-vm`. Default-off until
//! Plan 72 W5.B / W5.C cutover flips `ensure_dev_image` to dispatch
//! through `LibkrunBuilderVm`. Library consumers that don't need
//! the libkrun builder build with `default-features = false`.
//!
//! ## Not the runtime backend
//!
//! `LibkrunBackend` (`crates/mvm-backend/src/libkrun.rs`) is for
//! running user microVMs; this module is for building them. The
//! two share `mvm-libkrun`'s FFI but compose differently — the
//! builder mounts a workspace + persistent `/nix`-store disk and
//! runs a one-shot `nix build`, while the runtime mounts the
//! user's rootfs and runs the user's entrypoint.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mvm_libkrun::{KrunContext, SupervisorConfig};
use serde::Deserialize;

use crate::builder_vm::{BuilderArtifacts, BuilderJob, BuilderMounts, BuilderVm, BuilderVmError};

/// Default vCPU count for the builder VM. Nix builds are
/// embarrassingly parallel at the derivation level; 4 cores is the
/// sweet spot on M-series Macs without saturating the host.
pub const DEFAULT_VCPUS: u8 = 4;

/// Default RAM in MiB. Nix evaluation peaks around 2.5 GiB for the
/// dev image's closure, but in-VM nix builds compiling rustc + std +
/// the 800-crate vendor tree (plus the kernel build for the builder
/// VM image's TSI kernel) peak around 5-6 GiB. 8 GiB leaves headroom
/// without OOM-killing the GCC link step. Plan 72 W5.D bullet 9.
pub const DEFAULT_MEMORY_MIB: u32 = 8192;

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
#[derive(Debug, Clone)]
pub struct LibkrunBuilderVm {
    /// Guest vCPU count. See [`DEFAULT_VCPUS`].
    pub vcpus: u8,
    /// Guest RAM in MiB. See [`DEFAULT_MEMORY_MIB`].
    pub memory_mib: u32,
    /// Persistent `/nix`-store image size in MiB (sparse cap).
    /// See [`DEFAULT_NIX_STORE_MIB`].
    pub nix_store_mib: u32,
    /// Optional caller-supplied bootstrap image. When set, `run_build`
    /// boots from this kernel/rootfs/cmdline instead of looking up the
    /// builder VM image in `~/.cache/mvm/builder-vm/<arch>/`.
    ///
    /// This is the Stage 0 escape from the chicken-and-egg on source
    /// checkouts: a local dev image that contains `/sbin/mvm-builder-init`
    /// can build the real builder VM image without downloading a
    /// published builder-VM artifact.
    pub image_override: Option<BuilderVmImage>,
}

/// Additional virtio-blk device passed to a one-shot builder shell
/// job. Devices appear after the builder VM's persistent Nix-store
/// disk; the first extra disk here is `/dev/vdc` in the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderExtraDisk {
    pub id: String,
    pub path: PathBuf,
    pub read_only: bool,
}

/// Generic builder-VM shell job.
///
/// This is intentionally narrower than [`BuilderJob`]: it is for
/// in-tree infrastructure commands that need the Linux builder
/// boundary but do not produce Nix build artifacts. Plan 85 Phase B
/// uses it to run `mkfs.ext4` and copy an OCI-unpacked rootfs into a
/// writable virtio-blk image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderShellJob {
    pub work_dir: PathBuf,
    pub artifact_out: PathBuf,
    pub script: String,
    pub extra_disks: Vec<BuilderExtraDisk>,
}

/// Result metadata from a one-shot builder shell job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderShellResult {
    pub job_dir: PathBuf,
    pub vm_state_dir: PathBuf,
}

impl Default for LibkrunBuilderVm {
    fn default() -> Self {
        Self {
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            nix_store_mib: DEFAULT_NIX_STORE_MIB,
            image_override: None,
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

    /// Boot from a caller-supplied kernel/rootfs/cmdline instead of
    /// resolving the builder VM image from `~/.cache/mvm/builder-vm/`.
    pub fn with_image_override(mut self, image: BuilderVmImage) -> Self {
        self.image_override = Some(image);
        self
    }

    /// Run an in-tree shell script inside the existing builder VM
    /// boundary. The script is staged as `/job/cmd.sh`; `/work`
    /// points at [`BuilderShellJob::work_dir`], `/out` points at
    /// [`BuilderShellJob::artifact_out`], and callers may attach
    /// additional writable or read-only virtio-blk disks.
    pub fn run_shell_script(
        &self,
        job: &BuilderShellJob,
    ) -> Result<BuilderShellResult, BuilderVmError> {
        self.validate_shell_job(job)?;

        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::LibkrunUnavailable(format!(
                "libkrun shared library not found on host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        let supervisor_path = resolve_supervisor_path()?;
        let image = match &self.image_override {
            Some(image) => image.clone(),
            None => ensure_builder_vm_image()?,
        };
        let nix_store_img = ensure_nix_store_image(host_arch_tag(), u64::from(self.nix_store_mib))?;

        let job_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&job_id);
        stage_shell_job_dir(&job_dir, &job.script)?;

        let vm_name = format!("mvm-builder-vm-{job_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating builder VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;
        let console_log = vm_state_dir.join("console.log");

        let mut krun = KrunContext::new(
            &vm_name,
            path_to_str(&image.kernel_path, "kernel_path")?,
            path_to_str(&image.rootfs_path, "rootfs_path")?,
        )
        .with_resources(self.vcpus, self.memory_mib)
        .with_cmdline(&image.cmdline)
        .with_console_output(path_to_str(&console_log, "console_log")?)
        .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
        .add_disk(
            "nix-store",
            path_to_str(&nix_store_img, "nix_store_img")?,
            false,
        )
        .add_virtio_fs("work", path_to_str(&job.work_dir, "work_dir")?)
        .add_virtio_fs("out", path_to_str(&job.artifact_out, "artifact_out")?)
        .add_virtio_fs("job", path_to_str(&job_dir, "job_dir")?);

        for disk in &job.extra_disks {
            krun = krun.add_disk(
                disk.id.as_str(),
                path_to_str(&disk.path, "extra_disk")?,
                disk.read_only,
            );
        }

        let cfg = SupervisorConfig {
            krun,
            vm_state_dir: path_to_str(&vm_state_dir, "vm_state_dir")?.to_string(),
            pid_file_name: Some("builder.pid".to_string()),
        };
        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg, &vm_state_dir)?;
        if exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "supervisor exited with non-zero status ({exit_code}); \
                 guest stderr at {}",
                vm_state_dir.display()
            )));
        }

        let result = read_job_result(&job_dir)?;
        if result.exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "guest shell job exited {} — stderr tail:\n{}",
                result.exit_code, result.stderr_tail
            )));
        }

        Ok(BuilderShellResult {
            job_dir,
            vm_state_dir,
        })
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

    /// Validate the job description. For [`BuilderJob::Flake`] both
    /// fields must be non-empty strings (`flake_ref` may include a
    /// `path:` or `git+` prefix but the prefix-less form is also
    /// accepted — libkrun runs the command verbatim inside the
    /// builder VM). For [`BuilderJob::Install`] the spec path must
    /// exist on the host; B.2 will fill in the rest of the
    /// install-pipeline validation against the parsed spec.
    pub(crate) fn validate_job(&self, job: &BuilderJob) -> Result<(), BuilderVmError> {
        match job {
            BuilderJob::Flake {
                flake_ref,
                attr_path,
            } => {
                if flake_ref.trim().is_empty() {
                    return Err(BuilderVmError::NixBuildFailed(
                        "BuilderJob.flake_ref is empty".to_string(),
                    ));
                }
                if attr_path.trim().is_empty() {
                    return Err(BuilderVmError::NixBuildFailed(
                        "BuilderJob.attr_path is empty".to_string(),
                    ));
                }
            }
            BuilderJob::Install { spec_path } => {
                // The install pipeline ships in Plan 73 Followup B.2;
                // validate the file exists so a future caller's
                // typo surfaces here rather than as an opaque
                // NotYetImplemented mid-pipeline.
                if !spec_path.is_file() {
                    return Err(BuilderVmError::ExtractionFailed(format!(
                        "BuilderJob::Install spec_path does not exist or is not a file: {}",
                        spec_path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_shell_job(&self, job: &BuilderShellJob) -> Result<(), BuilderVmError> {
        ensure_utf8_path(&job.work_dir, "work_dir")?;
        ensure_utf8_path(&job.artifact_out, "artifact_out")?;
        for disk in &job.extra_disks {
            ensure_utf8_path(&disk.path, "extra_disk")?;
            if disk.id.trim().is_empty() {
                return Err(BuilderVmError::ExtractionFailed(
                    "extra disk id is empty".to_string(),
                ));
            }
            if !disk.path.is_file() {
                return Err(BuilderVmError::ExtractionFailed(format!(
                    "extra disk path does not exist or is not a file: {}",
                    disk.path.display()
                )));
            }
        }
        if !job.work_dir.is_dir() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "shell job work_dir must be a directory: {}",
                job.work_dir.display()
            )));
        }
        std::fs::create_dir_all(&job.artifact_out).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating artifact_out {}: {e}",
                job.artifact_out.display()
            ))
        })?;
        if job.script.trim().is_empty() {
            return Err(BuilderVmError::NixBuildFailed(
                "builder shell script is empty".to_string(),
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
    fn run_build(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        // 1. Validate caller-supplied inputs early; clearer
        //    errors than failing inside the libkrun FFI.
        self.validate_mounts(mounts)?;
        self.validate_job(job)?;

        // 2. Refuse to proceed on a host without libkrun. The
        //    `builder-vm` feature being compiled
        //    in doesn't imply the runtime library is installed.
        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::LibkrunUnavailable(format!(
                "libkrun shared library not found on host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        // 3. Find the supervisor binary up front. Failing now is
        //    a much better UX than spawning the supervisor with a
        //    stale PATH and discovering at child-exit time.
        let supervisor_path = resolve_supervisor_path()?;

        // 4. Find or initialise the builder VM image (kernel +
        //    rootfs.ext4 + canonical cmdline) the W2 flake
        //    produces. Stage 0 callers can provide a bootstrap
        //    image so a fresh source checkout can build the builder
        //    image cache without first downloading it.
        let image = match &self.image_override {
            Some(image) => image.clone(),
            None => ensure_builder_vm_image()?,
        };

        // 5. Allocate / locate the persistent `/nix-store`
        //    virtio-blk image. First build on a host pays the
        //    sparse-allocate cost; subsequent builds reuse the
        //    warm Nix store.
        let nix_store_img = ensure_nix_store_image(host_arch_tag(), u64::from(self.nix_store_mib))?;

        // 6. Stage the per-build job dir. Flake jobs get
        //    `cmd.sh`; install jobs get `install_spec.json`.
        //    `mvm-builder-init` (Plan 72 W3 + Plan 73 Followup
        //    B.2) dispatches based on which file it sees.
        let job_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&job_id);
        stage_job_dir(&job_dir, job)?;

        // 7. Build the `KrunContext` libkrun consumes. Three
        //    virtio-fs shares (work / out / job), one virtio-blk
        //    (Nix store), and the canonical cmdline pinned at the
        //    flake output. The mount layout is identical for
        //    flake + install jobs — the guest decides what to do
        //    with each share based on the staged job files.
        let vm_name = format!("mvm-builder-vm-{job_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating builder VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;

        // Route the guest's serial console to a per-VM log file so
        // failures of the in-VM cmd.sh / mvm-builder-init produce a
        // readable transcript. Without this, libkrun discards the
        // hvc0 output silently and "supervisor running, then exits 1"
        // is the only observable signal.
        let console_log = vm_state_dir.join("console.log");
        let krun = KrunContext::new(
            &vm_name,
            path_to_str(&image.kernel_path, "kernel_path")?,
            path_to_str(&image.rootfs_path, "rootfs_path")?,
        )
        .with_resources(self.vcpus, self.memory_mib)
        .with_cmdline(&image.cmdline)
        .with_console_output(path_to_str(&console_log, "console_log")?)
        .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
        .add_disk(
            "nix-store",
            path_to_str(&nix_store_img, "nix_store_img")?,
            false,
        )
        .add_virtio_fs("work", path_to_str(&mounts.flake_src, "flake_src")?)
        .add_virtio_fs("out", path_to_str(&mounts.artifact_out, "artifact_out")?)
        .add_virtio_fs("job", path_to_str(&job_dir, "job_dir")?);

        // 8. Drive the supervisor: pipe `SupervisorConfig` to
        //    stdin and **wait** for the child to exit. Unlike
        //    `LibkrunBackend::start` (returns immediately after
        //    the PID file appears), the builder is a one-shot —
        //    we want the supervisor to live until the guest
        //    powers off, then collect the result.
        let cfg = SupervisorConfig {
            krun,
            vm_state_dir: path_to_str(&vm_state_dir, "vm_state_dir")?.to_string(),
            pid_file_name: Some("builder.pid".to_string()),
        };
        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg, &vm_state_dir)?;
        if exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "supervisor exited with non-zero status ({exit_code}); \
                 guest stderr at {}",
                vm_state_dir.display()
            )));
        }

        // 9. Per-variant result parsing + artifact validation.
        //    Flake jobs read `<job_dir>/result` (legacy shape);
        //    install jobs read `<artifact_out>/result.json` and
        //    return a different artifact variant.
        match job {
            BuilderJob::Flake { .. } => finalize_flake_job(&job_dir, &mounts.artifact_out, &job_id),
            BuilderJob::Install { .. } => finalize_install_job(&mounts.artifact_out),
        }
    }

    fn cleanup(&self) -> Result<(), BuilderVmError> {
        // Plan 72 W6 hygiene: prune old job dirs under
        // `~/.cache/mvm/builder-vm/jobs/` past N days. No-op
        // until W6 picks the retention policy.
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────
// Helpers — kept in one place at the bottom of the file rather
// than scattered through `impl` blocks so the run_build pipeline
// reads top-down.
// ─────────────────────────────────────────────────────────────────

/// Resolved builder VM image — either the W2 flake output the
/// libkrun launcher boots into, or a caller-supplied Stage 0 image.
#[derive(Debug, Clone)]
pub struct BuilderVmImage {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub cmdline: String,
}

impl BuilderVmImage {
    pub fn new(kernel_path: PathBuf, rootfs_path: PathBuf, cmdline: String) -> Self {
        Self {
            kernel_path,
            rootfs_path,
            cmdline,
        }
    }
}

/// Parsed `/job/result` written by `mvm-builder-init` (Plan 72 W3).
/// Shape matches the JSON `mvm-builder-init::linux::write_result`
/// emits.
#[derive(Debug, Deserialize)]
struct JobResult {
    exit_code: i32,
    #[serde(default)]
    stderr_tail: String,
}

/// Host architecture tag used as a cache-key segment for
/// per-arch builder VM images. `aarch64` on Apple Silicon /
/// ARM Linux, `x86_64` everywhere else. Plan 72 W2's flake
/// emits both per release.
fn host_arch_tag() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

/// `~/.cache/mvm/builder-vm/`. Wrapper around
/// `mvm_core::config::mvm_cache_dir()` to keep the per-arch
/// subdirs in one place. Created lazily by callers — this
/// function does not touch the filesystem.
fn builder_vm_cache_dir() -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_cache_dir()).join("builder-vm")
}

/// Find the builder VM image (kernel + rootfs + cmdline) in
/// the host cache. The W2 flake's `packages.<system>.default`
/// produces exactly the `vmlinux` / `rootfs.ext4` / `cmdline.txt`
/// files this loads. Plan 72 W5 cutover wires the build-or-
/// download step that populates this cache; today it errors
/// when missing with an actionable hint.
fn ensure_builder_vm_image() -> Result<BuilderVmImage, BuilderVmError> {
    let arch_dir = builder_vm_cache_dir().join(host_arch_tag());
    let kernel_path = arch_dir.join("vmlinux");
    let rootfs_path = arch_dir.join("rootfs.ext4");
    let cmdline_path = arch_dir.join("cmdline.txt");

    if !kernel_path.is_file() || !rootfs_path.is_file() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "builder VM image not found at {}. \
             Populate the cache by running `nix build ./nix/images/builder-vm#packages.{}-linux.default` \
             on a host with Nix and copying `result/{{vmlinux,rootfs.ext4,cmdline.txt}}` to {}/, \
             or wait for Plan 72 W5 to wire the Stage 0 bootstrap.",
            arch_dir.display(),
            host_arch_tag(),
            arch_dir.display(),
        )));
    }

    let cmdline = std::fs::read_to_string(&cmdline_path)
        .map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "{} missing or unreadable ({e}). The builder VM cache is poisoned; delete {} and re-run `mvmctl dev up` to re-bootstrap.",
                cmdline_path.display(),
                arch_dir.display(),
            ))
        })?
        .trim()
        .to_string();

    Ok(BuilderVmImage {
        kernel_path,
        rootfs_path,
        cmdline,
    })
}

/// Find or create the persistent `/nix-store` sparse image.
/// virtio-blk attaches this as `/dev/vdb` in the guest;
/// `mvm-builder-init` formats it ext4 on first boot.
///
/// `size_mib` is the sparse cap — the file consumes only the
/// bytes the in-VM ext4 actually writes. Caller-controlled
/// because dev hosts may want a smaller cap than CI runners.
fn ensure_nix_store_image(arch: &str, size_mib: u64) -> Result<PathBuf, BuilderVmError> {
    let dir = builder_vm_cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating builder cache dir {}: {e}",
            dir.display()
        ))
    })?;
    let path = dir.join(format!("nix-store-{arch}.img"));
    if path.is_file() {
        return Ok(path);
    }

    // Allocate a sparse file: open with O_CREAT, seek to size-1,
    // write a zero byte. The filesystem records the size but
    // doesn't allocate the blocks until something writes them
    // (true on APFS + ext4). Avoids paying multi-GiB at provision
    // time for a store that may never fill up.
    let size_bytes = size_mib.checked_mul(1024 * 1024).ok_or_else(|| {
        BuilderVmError::ExtractionFailed(format!(
            "nix-store size_mib overflowed multiplying to bytes: {size_mib}"
        ))
    })?;
    let f = std::fs::File::create(&path)
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("create {}: {e}", path.display())))?;
    f.set_len(size_bytes).map_err(|e| {
        let _ = std::fs::remove_file(&path);
        BuilderVmError::ExtractionFailed(format!(
            "set_len({size_bytes}) on {}: {e}",
            path.display()
        ))
    })?;
    drop(f);
    Ok(path)
}

/// Monotonic per-process job ID. Combines a UNIX timestamp
/// with the current PID so two concurrent invocations on one
/// host don't clobber each other's job dirs even if they hit
/// the same second.
fn unique_job_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{now:013}-{pid}")
}

/// Filename of the install-spec JSON the host stages for
/// install jobs. Matches the constant `mvm-builder-init` checks
/// for inside the VM — keep these in sync.
pub(crate) const INSTALL_SPEC_FILENAME: &str = "install_spec.json";

/// Filename of the install report `mvm-builder-init` writes into
/// `artifact_out/` after the install pipeline finishes. The host
/// reads + parses this to decide whether the install succeeded.
pub(crate) const INSTALL_RESULT_FILENAME: &str = "result.json";

/// Stage the per-job dir for the given [`BuilderJob`].
///
/// - [`BuilderJob::Flake`]: writes `<job_dir>/cmd.sh` with the
///   `nix build` script the guest's PID 1 dispatches via
///   `/bin/sh -eu`.
/// - [`BuilderJob::Install`]: copies the caller's install spec
///   to `<job_dir>/install_spec.json`. `mvm-builder-init` probes
///   for this filename first; when present it routes through the
///   app-deps install pipeline (Plan 73 Followup B.2) instead of
///   running `cmd.sh`.
///
/// The two modes are mutually exclusive — install jobs don't
/// emit a `cmd.sh`, flake jobs don't emit an install spec.
fn stage_job_dir(job_dir: &Path, job: &BuilderJob) -> Result<(), BuilderVmError> {
    std::fs::create_dir_all(job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("creating job dir {}: {e}", job_dir.display()))
    })?;

    let (flake_ref, attr_path) = match job {
        BuilderJob::Flake {
            flake_ref,
            attr_path,
        } => (flake_ref.as_str(), attr_path.as_str()),
        BuilderJob::Install { spec_path } => {
            // Copy the caller's spec into the per-job dir so the
            // virtio-fs share carries it into the guest at
            // `/job/install_spec.json`. `mvm-builder-init`
            // (Plan 73 Followup B.2) detects that filename and
            // dispatches through the install pipeline instead of
            // running cmd.sh.
            let dst = job_dir.join(INSTALL_SPEC_FILENAME);
            std::fs::copy(spec_path, &dst).map_err(|e| {
                BuilderVmError::ExtractionFailed(format!(
                    "copying install spec {} -> {}: {e}",
                    spec_path.display(),
                    dst.display()
                ))
            })?;
            return Ok(());
        }
    };

    // Render the `cmd.sh` content. flake_ref and attr_path are
    // user-controlled; emit them inside `'…'` quoted shell
    // variables, escaping any embedded `'` with the standard
    // `'\''` close-quote / escape / open-quote dance.
    let body = format!(
        r#"#!/bin/sh
# mvm-builder-vm cmd.sh — emitted by LibkrunBuilderVm (Plan 72 W4).
# Runs inside the libkrun builder VM under `/bin/sh -eu`. The
# host wires /work (workspace), /out (artifact dir), /job (this
# dir) as virtio-fs shares; /nix is a persistent virtio-blk
# overlay handled by mvm-builder-init.
set -eu

FLAKE_REF='{flake_ref}'
ATTR_PATH='{attr_path}'

# Point HOME and Nix's cache/state dirs at the writable tmpfs (`/tmp`).
# The rootfs is mounted `ro`, so nix tries `~/.cache/nix` and bails
# with "creating directory '//.cache/nix': Read-only file system"
# when HOME stays at the default `/`. /tmp is tmpfs, lives only for
# this VM's lifetime — fine for a single-shot build.
export HOME=/tmp
export XDG_CACHE_HOME=/tmp/.cache
export XDG_STATE_HOME=/tmp/.local/state
mkdir -p /tmp/.cache /tmp/.local/state

# CA certs for TLS to cache.nixos.org / api.github.com.
export CURL_CA_BUNDLE=/etc/ssl/certs/ca-bundle.crt
export NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt
export SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt

cd /work
# `experimental-features` enables nix-command + flakes. `sandbox =
# false` + `build-users-group =` is mandatory inside the builder
# VM: there are no `nixbld*` accounts in the rootfs and no kernel
# user-ns isolation for build sandboxes, so every derivation would
# otherwise fail with "the group 'nixbld' specified in
# 'build-users-group' does not exist". The builder VM IS the
# isolation boundary, so an in-guest sandbox is redundant.
export NIX_CONFIG="experimental-features = nix-command flakes
sandbox = false
build-users-group =
substituters = https://cache.nixos.org/
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
# Plan 72 W0's flake convention: workspace-path env var so
# flakes that reference the workspace root don't depend on
# relative-path resolution against the store-copied flake dir.
export MVM_WORKSPACE_PATH=/work

echo "mvm-builder-vm: filesystem space before nix build:" >&2
df -h /nix /tmp >&2 || true

# `--impure` is what unblocks builds inside the VM when the
# flake has path inputs; `--no-write-lock-file` keeps the
# read-only `/work` mount from tripping EROFS.
# `--print-build-logs --keep-going` dumps every failing build's
# stderr inline (default nix only prints the last 10 lines and
# cascades up). We tee stderr to /job/nix-build.log so the host
# can read the actual root cause when a deep dependency fails.
set +e
nix build "${{FLAKE_REF}}#${{ATTR_PATH}}" \
    --no-link --print-out-paths --no-write-lock-file --impure \
    --print-build-logs --keep-going \
    > /job/nix-stdout.log 2> /job/nix-stderr.log
NIX_RC=$?
set -e
NIX_OUT=$(cat /job/nix-stdout.log)
if [ "$NIX_RC" -ne 0 ]; then
    echo "mvm-builder-vm: filesystem space after failed nix build:" >&2
    df -h /nix /tmp >&2 || true
    echo "nix build exited $NIX_RC; tail of stderr:" >&2
    tail -200 /job/nix-stderr.log >&2
    exit $NIX_RC
fi

if [ -z "$NIX_OUT" ]; then
    echo "nix build emitted no /nix/store output path" >&2
    exit 1
fi
printf '%s\n' "$NIX_OUT" > /job/store-path

# Copy the artifacts the host expects into /out. We accept
# either `vmlinux` (the canonical name our flakes use) or
# `Image` / `bzImage` (raw kernel format names) for
# robustness across flake conventions.
if   [ -f "$NIX_OUT/vmlinux" ]; then cp -L "$NIX_OUT/vmlinux" /out/vmlinux
elif [ -f "$NIX_OUT/Image"   ]; then cp -L "$NIX_OUT/Image"   /out/vmlinux
elif [ -f "$NIX_OUT/bzImage" ]; then cp -L "$NIX_OUT/bzImage" /out/vmlinux
fi
if [ -f "$NIX_OUT/rootfs.ext4" ]; then
    cp -L "$NIX_OUT/rootfs.ext4" /out/rootfs.ext4
else
    echo "no rootfs.ext4 in nix build output at $NIX_OUT" >&2
    exit 1
fi

# Permissions for the host-side reader. Ignore failures —
# virtio-fs may map the uid such that chmod is a no-op.
chmod 0644 /out/rootfs.ext4 2>/dev/null || true
[ -f /out/vmlinux ] && chmod 0644 /out/vmlinux 2>/dev/null || true
"#,
        flake_ref = shell_single_quote_escape(flake_ref),
        attr_path = shell_single_quote_escape(attr_path),
    );

    let cmd_path = job_dir.join("cmd.sh");
    std::fs::write(&cmd_path, body).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("writing {}: {e}", cmd_path.display()))
    })?;
    Ok(())
}

fn stage_shell_job_dir(job_dir: &Path, script: &str) -> Result<(), BuilderVmError> {
    std::fs::create_dir_all(job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("creating job dir {}: {e}", job_dir.display()))
    })?;
    let cmd_path = job_dir.join("cmd.sh");
    std::fs::write(&cmd_path, script).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("writing {}: {e}", cmd_path.display()))
    })?;
    Ok(())
}

/// Escape a string for inclusion inside `'…'` single quotes
/// in POSIX shell. The only character that can't appear inside
/// single quotes is `'` itself; we close the quote, emit `\'`,
/// then reopen. Standard sh-escape pattern.
fn shell_single_quote_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Finalize a flake build: read `<job_dir>/result`, validate the
/// `rootfs.ext4` (and optional `vmlinux`) landed in
/// `artifact_out`, return a [`BuilderArtifacts::Image`].
fn finalize_flake_job(
    job_dir: &Path,
    artifact_out: &Path,
    job_id: &str,
) -> Result<BuilderArtifacts, BuilderVmError> {
    let result = read_job_result(job_dir)?;
    if result.exit_code != 0 {
        return Err(BuilderVmError::NixBuildFailed(format!(
            "guest cmd.sh exited {} — stderr tail:\n{}",
            result.exit_code, result.stderr_tail
        )));
    }

    let rootfs_path = artifact_out.join("rootfs.ext4");
    if !rootfs_path.is_file() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "builder VM exited cleanly but {} was not written",
            rootfs_path.display()
        )));
    }
    let kernel_path_out = artifact_out.join("vmlinux");
    let kernel_path = if kernel_path_out.is_file() {
        Some(kernel_path_out)
    } else {
        None
    };

    Ok(BuilderArtifacts::Image {
        rootfs_path,
        kernel_path,
        revision_hash: read_revision_hash(job_dir).unwrap_or_else(|| job_id.to_string()),
        lock_hash: None,
        accessible: None,
    })
}

/// Read `/job/store-path` and extract the leading Nix store hash
/// from `/nix/store/<hash>-<name>`. Older guest images may not
/// write the sidecar; those callers fall back to the unique job id.
fn read_revision_hash(job_dir: &Path) -> Option<String> {
    let body = std::fs::read_to_string(job_dir.join("store-path")).ok()?;
    extract_nix_store_hash(body.trim()).map(str::to_string)
}

fn extract_nix_store_hash(store_path: &str) -> Option<&str> {
    let name = store_path.strip_prefix("/nix/store/")?;
    let (hash, _rest) = name.split_once('-')?;
    if hash.is_empty() { None } else { Some(hash) }
}

/// Finalize an install job (Plan 73 Followup B.2): validate the
/// install report `mvm-builder-init` wrote to
/// `<artifact_out>/result.json`, fail closed on `installer_exit_code
/// != 0`, and return [`BuilderArtifacts::InstallVolume`] pointing
/// at the directory. Sealing the volume (via
/// `mvm_sdk::compile::deps_audit::seal_volume`) and renaming into
/// the deps cache is the orchestrator's job
/// (`mvm_build::app_deps::install_app_deps`) — keeping it out of
/// the builder VM means the same code path covers fresh installs
/// and cache rehydrations.
fn finalize_install_job(artifact_out: &Path) -> Result<BuilderArtifacts, BuilderVmError> {
    let result_path = artifact_out.join(INSTALL_RESULT_FILENAME);
    if !result_path.is_file() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "install job VM exited cleanly but {} was not written",
            result_path.display()
        )));
    }
    let body = std::fs::read_to_string(&result_path).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("reading {}: {e}", result_path.display()))
    })?;
    let report: InstallResultReport = serde_json::from_str(&body).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "parsing {} as JSON: {e}\nbody:\n{body}",
            result_path.display()
        ))
    })?;

    if report.installer_exit_code != 0 {
        let reason = report
            .failure_reason
            .clone()
            .unwrap_or_else(|| format!("installer exited {}", report.installer_exit_code));
        return Err(BuilderVmError::NixBuildFailed(format!(
            "install pipeline failed inside builder VM: {reason}"
        )));
    }

    // The four sealed-volume artifacts must all be present —
    // mvm-builder-init emits stubs on missing optional tooling
    // (SBOM / CVE) so absence here means the guest crashed mid-
    // pipeline. seal_volume would catch this too, but failing
    // closed at the builder layer pins the error to the right
    // diagnostic message.
    for name in ["content", "sbom.cdx.json", "fetch.log", "cve.json"] {
        let p = artifact_out.join(name);
        if !p.exists() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "install job VM exited cleanly but sealed-volume artifact {} is missing",
                p.display()
            )));
        }
    }

    Ok(BuilderArtifacts::InstallVolume {
        volume_dir: artifact_out.to_path_buf(),
        result_json_path: result_path,
    })
}

/// Parsed shape of `<artifact_out>/result.json` — the install
/// report `mvm-builder-init::install::InstallReport::to_json`
/// emits. Field set kept in sync with the writer; an additive
/// change to the writer (B.2.x egress allowlist diagnostics, for
/// example) needs a matching `#[serde(default)]` field here.
#[derive(Debug, Deserialize)]
struct InstallResultReport {
    installer_exit_code: i32,
    /// Set when `mvm-builder-init` synthesizes a failure report
    /// (e.g. installer binary missing on PATH). Surfaced in the
    /// host-side error message.
    #[serde(default)]
    failure_reason: Option<String>,
}

/// Read and parse `<job_dir>/result`. The guest's PID 1
/// writes this on every code path that reaches `power_off`.
fn read_job_result(job_dir: &Path) -> Result<JobResult, BuilderVmError> {
    let path = job_dir.join("result");
    let body = std::fs::read_to_string(&path).map_err(|e| {
        BuilderVmError::NixBuildFailed(format!(
            "guest did not write {}: {e} \
             (the VM may have crashed before mvm-builder-init could finalize)",
            path.display()
        ))
    })?;
    serde_json::from_str::<JobResult>(&body).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "parsing {} as JSON: {e}\nbody:\n{body}",
            path.display()
        ))
    })
}

/// Locate the `mvm-libkrun-supervisor` binary. Mirrors the
/// resolver in `mvm-backend::libkrun::resolve_supervisor_path`
/// (kept local rather than re-exported to keep the dep graph
/// flat). Order: env override → next to current_exe → PATH.
fn resolve_supervisor_path() -> Result<PathBuf, BuilderVmError> {
    if let Some(p) = std::env::var_os("MVM_LIBKRUN_SUPERVISOR_PATH") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(BuilderVmError::LibkrunUnavailable(format!(
            "MVM_LIBKRUN_SUPERVISOR_PATH points at {} which is not a file",
            path.display()
        )));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("mvm-libkrun-supervisor");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = which::which("mvm-libkrun-supervisor") {
        return Ok(path);
    }
    Err(BuilderVmError::LibkrunUnavailable(
        "mvm-libkrun-supervisor binary not found. \
         Looked for: $MVM_LIBKRUN_SUPERVISOR_PATH, alongside the current exe, and on $PATH. \
         Install via `cargo install --path crates/mvm-libkrun --features libkrun-sys` \
         or set MVM_LIBKRUN_SUPERVISOR_PATH=/abs/path/to/the/binary."
            .to_string(),
    ))
}

/// Spawn `mvm-libkrun-supervisor`, pipe a `SupervisorConfig`
/// JSON document to its stdin, then **wait** for it to exit.
/// Returns the child's exit code (0 on clean guest power-off
/// per libkrun's `start_enter` semantics; non-zero if the
/// supervisor errored before or during the guest run).
///
/// Distinct from `mvm-backend::LibkrunBackend::start` which
/// only waits for the PID file to appear and then returns —
/// that consumer wants a long-lived background VM. The
/// builder VM is a one-shot; the caller can't make progress
/// until the build finishes.
fn spawn_supervisor_and_wait(
    supervisor_path: &Path,
    cfg: &SupervisorConfig,
    vm_state_dir: &Path,
) -> Result<i32, BuilderVmError> {
    let json = serde_json::to_string(cfg).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("serialize SupervisorConfig: {e}"))
    })?;

    let mut child = Command::new(supervisor_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            BuilderVmError::LibkrunUnavailable(format!("spawn {}: {e}", supervisor_path.display()))
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(
                "supervisor stdin was not piped (unreachable — Stdio::piped() requested)"
                    .to_string(),
            )
        })?
        .write_all(json.as_bytes())
        .map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "writing SupervisorConfig to supervisor stdin: {e}"
            ))
        })?;

    let timeout = builder_vm_timeout()?;
    // Plan 77 W6 — concurrent kernel-panic detector. The libkrun
    // supervisor blocks in `krun_start_enter` until the VM cleanly
    // exits; a kernel panic at PID 1 doesn't trigger a clean exit, so
    // a plain `child.wait()` would hang indefinitely. We tail the VM's
    // console log for the kernel's stable panic banner and kill the
    // supervisor on detection. When `console_output_path` is None
    // (callers that opted out of console capture), behavior is
    // unchanged — plain wait, plain exit code.
    let console_log = cfg.krun.console_output_path.as_deref().map(PathBuf::from);
    match wait_with_panic_detector_until(
        &mut child,
        console_log.as_deref(),
        DEFAULT_PANIC_POLL_INTERVAL,
        Some(timeout),
    ) {
        Ok(WaitOutcome::Clean(code)) => Ok(code),
        Ok(WaitOutcome::KernelPanic {
            panic_line,
            console_log_path,
        }) => Err(BuilderVmError::SeedKernelPanic {
            panic_line,
            console_log_path,
        }),
        Ok(WaitOutcome::Timeout) => Err(BuilderVmError::NixBuildFailed(format!(
            "builder VM exceeded {} seconds wall-clock; killed. Console log at {}/console.log.",
            timeout.as_secs(),
            vm_state_dir.display(),
        ))),
        Err(e) => Err(BuilderVmError::ExtractionFailed(format!(
            "wait on supervisor child: {e}"
        ))),
    }
}

/// Plan 77 W6 — outcome of [`wait_with_panic_detector`]. `KernelPanic`
/// short-circuits the normal exit-code path with the captured banner
/// line so the caller can map it to [`BuilderVmError::SeedKernelPanic`]
/// without a separate side channel.
#[derive(Debug)]
enum WaitOutcome {
    Clean(i32),
    KernelPanic {
        panic_line: String,
        console_log_path: String,
    },
    Timeout,
}

/// Poll interval for the panic-detector watcher in production. Keeping
/// this short (100 ms) is what makes a panic surface in well under a
/// second — the kernel's panic banner is written via printk well
/// before the supervisor's blocking `start_enter` would have otherwise
/// noticed anything wrong.
const DEFAULT_PANIC_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Block on `child` while concurrently tailing `console_log` (if any)
/// for the kernel's stable panic banner `Kernel panic - not syncing`.
/// On detection, kill the child and return [`WaitOutcome::KernelPanic`]
/// with the captured line. When `console_log` is `None`, falls back to
/// a plain `child.wait`.
///
/// `poll_interval` is the watcher's sleep between log-tail polls;
/// production calls pass [`DEFAULT_PANIC_POLL_INTERVAL`]. Tests pass a
/// shorter interval (e.g. 10 ms) to keep wall-clock under control.
///
/// The watcher runs on its own thread. The main thread loops
/// `Child::try_wait` so it can break out either on child exit or on
/// the watcher signaling a panic. When the watcher signals a panic,
/// the main thread calls `Child::kill` to unblock the libkrun
/// supervisor (libkrun's `krun_start_enter` runs on the supervisor's
/// main thread inside `exit()` — SIGKILL is the only reliable
/// signal, which matches the gotcha documented in
/// `reference_libkrun_gotchas.md`).
#[cfg(test)]
fn wait_with_panic_detector(
    child: &mut Child,
    console_log: Option<&Path>,
    poll_interval: Duration,
) -> std::io::Result<WaitOutcome> {
    wait_with_panic_detector_until(child, console_log, poll_interval, None)
}

fn wait_with_panic_detector_until(
    child: &mut Child,
    console_log: Option<&Path>,
    poll_interval: Duration,
    timeout: Option<Duration>,
) -> std::io::Result<WaitOutcome> {
    let deadline = timeout.map(|duration| Instant::now() + duration);

    let panic_line: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = console_log.map(|console_log| {
        let watcher_panic = Arc::clone(&panic_line);
        let watcher_stop = Arc::clone(&stop);
        let watcher_path = console_log.to_path_buf();
        std::thread::spawn(move || {
            panic_watcher(&watcher_path, &watcher_panic, &watcher_stop, poll_interval);
        })
    });

    let wait_result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if panic_line
            .lock()
            .expect("panic detector state lock poisoned")
            .is_some()
        {
            // Best-effort kill; the supervisor is wedged inside
            // libkrun's `start_enter` so SIGKILL is the only reliable
            // signal. If the kill itself fails (already exited, etc.),
            // fall through to `wait` so we still reap the zombie.
            let _ = child.kill();
            match child.wait() {
                Ok(status) => break Ok(status),
                Err(e) => break Err(e),
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let _ = child.kill();
            let _ = child.wait();
            stop.store(true, Ordering::SeqCst);
            if let Some(watcher) = watcher {
                let _ = watcher.join();
            }
            return Ok(WaitOutcome::Timeout);
        }
        let sleep = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .map(|remaining| remaining.min(poll_interval))
            .unwrap_or(poll_interval);
        std::thread::sleep(sleep);
    };

    // Signal the watcher to exit and join it. Even on `Err` paths we
    // need the join — without it the spawned thread could outlive the
    // function with a live `&Path` to a tempdir the caller is about
    // to drop.
    stop.store(true, Ordering::SeqCst);
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }

    let status = wait_result?;
    let captured = panic_line
        .lock()
        .expect("panic detector state lock poisoned")
        .take();
    match captured {
        Some(line) => Ok(WaitOutcome::KernelPanic {
            panic_line: line,
            console_log_path: console_log
                .expect("panic line can only be captured when console log exists")
                .display()
                .to_string(),
        }),
        None => Ok(WaitOutcome::Clean(status.code().unwrap_or(-1))),
    }
}

/// The kernel's panic banner. Stable in upstream `kernel/panic.c` for
/// the last ~decade; substring match keeps us robust to colour codes,
/// log-level prefixes, and trailing detail.
const KERNEL_PANIC_BANNER: &str = "Kernel panic - not syncing";

/// Maximum bytes of unmatched tail we keep buffered between polls.
/// A panic line is short (~150 bytes) so 4 KiB is plenty of slack to
/// handle a partial last line spanning multiple reads.
const PANIC_WATCHER_BUFFER_CAP: usize = 4096;

/// Tail `console_log` for the kernel panic banner. On match, stores
/// the matching line into `panic_line` and returns. Polls every
/// `poll_interval` until either a match is found or `stop` is set.
///
/// Two robustness details that matter:
///
/// 1. **The console log doesn't exist when we start.** libkrun creates
///    the file on the first hvc0 byte the guest writes, ~100 ms after
///    `start_enter`. The watcher retries the `File::open` on every
///    poll until it succeeds — `console_log.exists()` is the cheap
///    pre-check.
///
/// 2. **Reads can split a line across polls.** A panic banner that
///    arrives at the same instant as the poll could be partially read
///    on one tick and completed on the next. We buffer unmatched tail
///    bytes (capped at [`PANIC_WATCHER_BUFFER_CAP`]) so the substring
///    match across reads still succeeds.
fn panic_watcher(
    console_log: &Path,
    panic_line: &Arc<Mutex<Option<String>>>,
    stop: &Arc<AtomicBool>,
    poll_interval: Duration,
) {
    let mut file: Option<std::fs::File> = None;
    let mut buf: Vec<u8> = Vec::new();

    while !stop.load(Ordering::SeqCst) {
        if file.is_none() && console_log.exists() {
            file = std::fs::File::open(console_log).ok();
        }
        if let Some(ref mut f) = file {
            let mut chunk = Vec::new();
            // Best-effort; an IO error here just defers detection to
            // the next poll. The supervisor's eventual exit still
            // unblocks the main thread.
            if f.read_to_end(&mut chunk).is_ok() && !chunk.is_empty() {
                buf.extend_from_slice(&chunk);
                if let Some(line) = find_panic_line_in(&buf) {
                    *panic_line.lock().unwrap() = Some(line);
                    return;
                }
                // Trim buf to the last PANIC_WATCHER_BUFFER_CAP bytes
                // so a multi-minute build's console output doesn't
                // grow the watcher's memory without bound. The banner
                // is much shorter than the cap so a truncated buffer
                // never severs a pending match.
                if buf.len() > PANIC_WATCHER_BUFFER_CAP {
                    let start = buf.len() - PANIC_WATCHER_BUFFER_CAP;
                    buf.drain(0..start);
                }
            }
        }
        std::thread::sleep(poll_interval);
    }
}

/// Scan `buf` for the kernel panic banner. On match, return the
/// containing line decoded as a UTF-8 string (lossy decoding — kernel
/// log output is ASCII in practice but we don't want a stray non-UTF-8
/// byte to silently drop the panic detection).
fn find_panic_line_in(buf: &[u8]) -> Option<String> {
    // Cheap pre-check before the lossy UTF-8 conversion.
    let needle = KERNEL_PANIC_BANNER.as_bytes();
    let idx = buf.windows(needle.len()).position(|w| w == needle)?;
    // Walk back to the previous newline (or start of buffer) and
    // forward to the next newline so the returned line is the full
    // banner with its detail (the kernel writes
    // `Kernel panic - not syncing: Requested init ... failed (error N).`
    // on a single line).
    let line_start = buf[..idx]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = buf[idx..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| idx + i)
        .unwrap_or(buf.len());
    let line = String::from_utf8_lossy(&buf[line_start..line_end])
        .trim_end_matches('\r')
        .to_string();
    Some(line)
}

fn builder_vm_timeout() -> Result<Duration, BuilderVmError> {
    let Some(raw) = std::env::var_os("MVM_BUILDER_VM_TIMEOUT_SECS") else {
        return Ok(Duration::from_secs(30 * 60));
    };
    let raw = raw.to_string_lossy();
    let secs = raw.parse::<u64>().map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "MVM_BUILDER_VM_TIMEOUT_SECS must be an integer number of seconds, got {raw:?}: {e}"
        ))
    })?;
    if secs == 0 {
        return Err(BuilderVmError::ExtractionFailed(
            "MVM_BUILDER_VM_TIMEOUT_SECS must be greater than zero".to_string(),
        ));
    }
    Ok(Duration::from_secs(secs))
}

/// Render a Path as a `&str` or surface a clear error if it
/// contains non-UTF-8 bytes. libkrun's C API takes
/// `*const c_char`; rejecting non-UTF-8 here pins the failure
/// to the offending field rather than a CString conversion
/// deep inside the FFI.
fn path_to_str<'a>(p: &'a Path, field: &str) -> Result<&'a str, BuilderVmError> {
    p.to_str().ok_or_else(|| {
        BuilderVmError::ExtractionFailed(format!("{field} has non-UTF-8 bytes: {p:?}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use tempfile::TempDir;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
        BuilderJob::Flake {
            flake_ref: "path:/work".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        }
    }

    #[test]
    fn defaults_match_plan_72_w1() {
        let vm = LibkrunBuilderVm::default();
        assert_eq!(vm.vcpus, 4);
        // Plan 72 W5.D bullet 9 bumped this from 4 GiB to 8 GiB
        // (in-VM nix builds peak ~5-6 GiB and OOM-kill the link step
        // at the lower default). Hardcoded here so a regression that
        // accidentally reverts the bump fails fast.
        assert_eq!(vm.memory_mib, 8192);
        assert_eq!(vm.nix_store_mib, 65536);
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
        let job = BuilderJob::Flake {
            flake_ref: "".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let err = LibkrunBuilderVm::default().validate_job(&job).unwrap_err();
        assert!(format!("{err}").contains("flake_ref"));
    }

    #[test]
    fn validate_job_rejects_whitespace_only_attr_path() {
        let job = BuilderJob::Flake {
            flake_ref: "path:/work".to_string(),
            attr_path: "   ".to_string(),
        };
        let err = LibkrunBuilderVm::default().validate_job(&job).unwrap_err();
        assert!(format!("{err}").contains("attr_path"));
    }

    #[test]
    fn validate_job_rejects_install_with_missing_spec() {
        // The Install variant validates that spec_path actually
        // exists — Followup B.2 will read it inside the VM, so the
        // host needs the file present before dispatch.
        let job = BuilderJob::Install {
            spec_path: PathBuf::from("/definitely/does/not/exist.json"),
        };
        let err = LibkrunBuilderVm::default().validate_job(&job).unwrap_err();
        assert!(
            matches!(err, BuilderVmError::ExtractionFailed(_)),
            "got {err:?}"
        );
        assert!(
            format!("{err}").contains("spec_path"),
            "expected spec_path in error: {err}"
        );
    }

    #[test]
    fn validate_job_accepts_install_with_existing_spec() {
        // Smoke-test the happy path of Install validation. We
        // don't construct a real spec — the parsing arrives in
        // B.2. We only check that a real file passes.
        let scratch = TempDir::new().unwrap();
        let spec_path = scratch.path().join("spec.json");
        std::fs::write(&spec_path, b"{}").unwrap();
        let job = BuilderJob::Install { spec_path };
        LibkrunBuilderVm::default().validate_job(&job).unwrap();
    }

    #[test]
    fn run_build_surfaces_environment_gaps_for_install_variant() {
        // Plan 73 Followup B.2 wires the Install variant — passing
        // input validation no longer trips NotYetImplemented. With
        // a CI runner that doesn't carry libkrun + the supervisor
        // binary, run_build now surfaces the same environment-gap
        // shape as the Flake variant. Asserts the wiring proceeds
        // past validation rather than short-circuiting.
        let scratch = TempDir::new().unwrap();
        let spec_path = scratch.path().join("spec.json");
        std::fs::write(&spec_path, b"{}").unwrap();
        let mounts = ok_mounts(&scratch);
        let err = LibkrunBuilderVm::default()
            .run_build(&BuilderJob::Install { spec_path }, &mounts)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BuilderVmError::LibkrunUnavailable(_) | BuilderVmError::ExtractionFailed(_)
            ),
            "unexpected error variant: {err:?}"
        );
    }

    #[test]
    fn stage_job_dir_install_copies_spec_into_job_dir() {
        // Plan 73 Followup B.2: install jobs stage
        // `<job_dir>/install_spec.json` rather than `cmd.sh`. The
        // guest's `mvm-builder-init` probes for that filename and
        // dispatches through the install pipeline.
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job-1");
        let spec_path = scratch.path().join("spec.json");
        let spec_body = br#"{"language":"python","lockfile_relative_path":"uv.lock","source_mount":"/work","gate":"dev"}"#;
        std::fs::write(&spec_path, spec_body).unwrap();
        stage_job_dir(&job_dir, &BuilderJob::Install { spec_path }).expect("stage ok");
        let dst = job_dir.join(INSTALL_SPEC_FILENAME);
        assert!(dst.is_file(), "install_spec.json must be staged at {dst:?}");
        let on_disk = std::fs::read(&dst).unwrap();
        assert_eq!(on_disk, spec_body, "spec bytes must round-trip verbatim");
        // No cmd.sh is emitted for install jobs.
        assert!(
            !job_dir.join("cmd.sh").exists(),
            "install jobs must not stage cmd.sh"
        );
    }

    #[test]
    fn finalize_install_job_requires_result_json() {
        // Empty artifact dir → ExtractionFailed pointing at the
        // missing result.json. Surfaces guest crashes that
        // prevented mvm-builder-init from finalizing the report.
        let scratch = TempDir::new().unwrap();
        let err = finalize_install_job(scratch.path()).unwrap_err();
        assert!(matches!(err, BuilderVmError::ExtractionFailed(_)));
        assert!(err.to_string().contains("result.json"), "got {err}");
    }

    #[test]
    fn finalize_install_job_rejects_nonzero_installer_exit() {
        let scratch = TempDir::new().unwrap();
        // Populate enough of the layout that the missing-artifacts
        // check doesn't trip first.
        std::fs::create_dir_all(scratch.path().join("content")).unwrap();
        std::fs::write(scratch.path().join("sbom.cdx.json"), b"{}").unwrap();
        std::fs::write(scratch.path().join("fetch.log"), b"").unwrap();
        std::fs::write(scratch.path().join("cve.json"), b"{}").unwrap();
        std::fs::write(
            scratch.path().join(INSTALL_RESULT_FILENAME),
            br#"{"installer_exit_code":1,"sbom_emitted":false,"cve_emitted":false,"language":"python","gate":"dev","content_path":"/out/content","sbom_path":"/out/sbom.cdx.json","fetch_log_path":"/out/fetch.log","cve_path":"/out/cve.json","failure_reason":"lockfile not found"}"#,
        )
        .unwrap();
        let err = finalize_install_job(scratch.path()).unwrap_err();
        match err {
            BuilderVmError::NixBuildFailed(msg) => {
                assert!(msg.contains("lockfile not found"), "got {msg}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn finalize_install_job_returns_install_volume_on_happy_path() {
        let scratch = TempDir::new().unwrap();
        std::fs::create_dir_all(scratch.path().join("content")).unwrap();
        std::fs::write(scratch.path().join("sbom.cdx.json"), b"{}").unwrap();
        std::fs::write(scratch.path().join("fetch.log"), b"").unwrap();
        std::fs::write(scratch.path().join("cve.json"), b"{}").unwrap();
        std::fs::write(
            scratch.path().join(INSTALL_RESULT_FILENAME),
            br#"{"installer_exit_code":0,"sbom_emitted":true,"cve_emitted":true,"language":"python","gate":"prod","content_path":"/out/content","sbom_path":"/out/sbom.cdx.json","fetch_log_path":"/out/fetch.log","cve_path":"/out/cve.json"}"#,
        )
        .unwrap();
        let art = finalize_install_job(scratch.path()).unwrap();
        match art {
            BuilderArtifacts::InstallVolume {
                volume_dir,
                result_json_path,
            } => {
                assert_eq!(volume_dir, scratch.path());
                assert_eq!(
                    result_json_path,
                    scratch.path().join(INSTALL_RESULT_FILENAME)
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn finalize_install_job_rejects_missing_sealed_artifact() {
        let scratch = TempDir::new().unwrap();
        // result.json says success, but the sealed-volume sidecars
        // are missing. Fail closed so seal_volume doesn't later
        // chase a half-populated dir.
        std::fs::write(
            scratch.path().join(INSTALL_RESULT_FILENAME),
            br#"{"installer_exit_code":0,"sbom_emitted":true,"cve_emitted":true,"language":"python","gate":"dev","content_path":"/out/content","sbom_path":"/out/sbom.cdx.json","fetch_log_path":"/out/fetch.log","cve_path":"/out/cve.json"}"#,
        )
        .unwrap();
        let err = finalize_install_job(scratch.path()).unwrap_err();
        assert!(
            matches!(err, BuilderVmError::ExtractionFailed(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn finalize_install_job_rejects_malformed_result_json() {
        let scratch = TempDir::new().unwrap();
        std::fs::write(scratch.path().join(INSTALL_RESULT_FILENAME), b"{not valid").unwrap();
        let err = finalize_install_job(scratch.path()).unwrap_err();
        match err {
            BuilderVmError::ExtractionFailed(msg) => assert!(msg.contains("parsing"), "got {msg}"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn run_build_fails_validation_before_reaching_libkrun() {
        // Bad input → validation error from `validate_mounts` /
        // `validate_job`, before run_build reaches the libkrun
        // availability check or the image cache.
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
    fn run_build_surfaces_environment_gaps_on_clean_input() {
        // Good input + a sandbox host (CI runner, dev macOS without
        // the cache populated) hits one of these in order:
        //   - libkrun shared library missing → LibkrunUnavailable
        //   - builder VM image cache empty   → ExtractionFailed
        //   - mvm-libkrun-supervisor missing → LibkrunUnavailable
        // Any of those is a valid pre-Plan-72-W5 state. The cutover
        // (Plan 72 W5) wires the Stage 0 bootstrap that populates
        // the image cache; until then, this test pins the shape
        // of what `mvmctl dev up` reports to operators.
        let scratch = TempDir::new().unwrap();
        let mounts = ok_mounts(&scratch);
        let err = LibkrunBuilderVm::default()
            .run_build(&ok_job(), &mounts)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BuilderVmError::LibkrunUnavailable(_) | BuilderVmError::ExtractionFailed(_)
            ),
            "unexpected error variant: {err:?}"
        );
    }

    #[test]
    fn shell_single_quote_escape_handles_apostrophes() {
        // `cmd.sh` embeds flake_ref + attr_path inside `'…'`
        // quoted shell variables. The only character that can't
        // appear verbatim is `'`. Standard escape: close-quote,
        // escape-via-backslash, reopen-quote.
        assert_eq!(shell_single_quote_escape("plain"), "plain");
        assert_eq!(shell_single_quote_escape("it's"), r"it'\''s");
        assert_eq!(shell_single_quote_escape("a'b'c"), r"a'\''b'\''c");
    }

    #[test]
    fn stage_job_dir_writes_cmd_sh_with_escaped_inputs() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job-1");
        let job = BuilderJob::Flake {
            flake_ref: "path:/work/nix/images/foo".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        stage_job_dir(&job_dir, &job).unwrap();
        let cmd = std::fs::read_to_string(job_dir.join("cmd.sh")).unwrap();
        assert!(cmd.contains("FLAKE_REF='path:/work/nix/images/foo'"));
        assert!(cmd.contains("ATTR_PATH='packages.x86_64-linux.default'"));
        assert!(cmd.starts_with("#!/bin/sh"));
        assert!(cmd.contains("set -eu"));
        assert!(cmd.contains("cd /work"));
        assert!(cmd.contains("printf '%s\\n' \"$NIX_OUT\" > /job/store-path"));
    }

    #[test]
    fn host_arch_tag_is_one_of_two_known_values() {
        // Plan 72 W2's flake outputs aarch64-linux and
        // x86_64-linux only; the cache-key segment must match
        // one of those.
        let tag = host_arch_tag();
        assert!(
            tag == "aarch64" || tag == "x86_64",
            "unexpected arch tag: {tag}"
        );
    }

    #[test]
    fn read_job_result_parses_well_formed_json() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().to_path_buf();
        std::fs::write(
            job_dir.join("result"),
            r#"{"exit_code":0,"stderr_tail":"hello"}"#,
        )
        .unwrap();
        let r = read_job_result(&job_dir).unwrap();
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.stderr_tail, "hello");
    }

    #[test]
    fn read_job_result_defaults_stderr_tail_when_absent() {
        // `#[serde(default)]` on stderr_tail. A guest that
        // exited before writing stderr_tail (rare, but possible
        // under panic) still parses cleanly.
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().to_path_buf();
        std::fs::write(job_dir.join("result"), r#"{"exit_code":2}"#).unwrap();
        let r = read_job_result(&job_dir).unwrap();
        assert_eq!(r.exit_code, 2);
        assert_eq!(r.stderr_tail, "");
    }

    #[test]
    fn read_job_result_errors_when_missing() {
        let scratch = TempDir::new().unwrap();
        let err = read_job_result(scratch.path()).unwrap_err();
        assert!(matches!(err, BuilderVmError::NixBuildFailed(_)));
    }

    #[test]
    fn extract_nix_store_hash_parses_output_path() {
        assert_eq!(
            extract_nix_store_hash("/nix/store/abc123def4567890-tenant-rootfs"),
            Some("abc123def4567890")
        );
        assert_eq!(extract_nix_store_hash("/tmp/not-store"), None);
        assert_eq!(extract_nix_store_hash("/nix/store/-missing-hash"), None);
    }

    #[test]
    fn finalize_flake_job_uses_store_path_hash_when_present() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job");
        let artifact_out = scratch.path().join("out");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::create_dir_all(&artifact_out).unwrap();
        std::fs::write(
            job_dir.join("result"),
            r#"{"exit_code":0,"stderr_tail":""}"#,
        )
        .unwrap();
        std::fs::write(
            job_dir.join("store-path"),
            "/nix/store/deadbeefcafebabe-builder-vm\n",
        )
        .unwrap();
        std::fs::write(artifact_out.join("rootfs.ext4"), b"rootfs").unwrap();

        let artifacts = finalize_flake_job(&job_dir, &artifact_out, "fallback-job-id").unwrap();
        match artifacts {
            BuilderArtifacts::Image { revision_hash, .. } => {
                assert_eq!(revision_hash, "deadbeefcafebabe");
            }
            other => panic!("wrong artifact variant: {other:?}"),
        }
    }

    #[test]
    fn finalize_flake_job_falls_back_to_job_id_without_store_path() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job");
        let artifact_out = scratch.path().join("out");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::create_dir_all(&artifact_out).unwrap();
        std::fs::write(
            job_dir.join("result"),
            r#"{"exit_code":0,"stderr_tail":""}"#,
        )
        .unwrap();
        std::fs::write(artifact_out.join("rootfs.ext4"), b"rootfs").unwrap();

        let artifacts = finalize_flake_job(&job_dir, &artifact_out, "fallback-job-id").unwrap();
        match artifacts {
            BuilderArtifacts::Image { revision_hash, .. } => {
                assert_eq!(revision_hash, "fallback-job-id");
            }
            other => panic!("wrong artifact variant: {other:?}"),
        }
    }

    #[test]
    fn ensure_nix_store_image_creates_sparse_file_once() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Sparse file allocates the logical size but consumes
        // ~no disk blocks. `set_len` is what asks the FS to
        // record the size. Subsequent calls find the existing
        // file and return its path without retouching.
        let scratch = TempDir::new().unwrap();
        // Redirect the cache dir via XDG_CACHE_HOME to keep the
        // test hermetic — `mvm_core::config::mvm_cache_dir()`
        // honors the env var.
        let old = std::env::var("XDG_CACHE_HOME").ok();
        // SAFETY: tests run single-threaded for env mutation
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", scratch.path());
        }
        let path = ensure_nix_store_image("x86_64", 256).unwrap();
        assert!(path.is_file());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 256 * 1024 * 1024);
        // Second call is idempotent.
        let path2 = ensure_nix_store_image("x86_64", 256).unwrap();
        assert_eq!(path, path2);
        // Restore the previous env so we don't leak into the
        // rest of the test suite.
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
    }

    #[test]
    fn ensure_builder_vm_image_requires_cmdline_txt() {
        let _lock = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new().unwrap();
        let old = std::env::var("XDG_CACHE_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", scratch.path());
        }

        let arch_dir = scratch
            .path()
            .join("mvm")
            .join("builder-vm")
            .join(host_arch_tag());
        std::fs::create_dir_all(&arch_dir).unwrap();
        std::fs::write(arch_dir.join("vmlinux"), b"kernel").unwrap();
        std::fs::write(arch_dir.join("rootfs.ext4"), b"rootfs").unwrap();

        let err = ensure_builder_vm_image().unwrap_err();
        assert!(
            format!("{err}").contains("cmdline.txt missing or unreadable"),
            "got {err}"
        );

        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
    }

    #[test]
    fn builder_vm_timeout_defaults_and_rejects_zero() {
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("MVM_BUILDER_VM_TIMEOUT_SECS").ok();
        unsafe {
            std::env::remove_var("MVM_BUILDER_VM_TIMEOUT_SECS");
        }
        assert_eq!(builder_vm_timeout().unwrap(), Duration::from_secs(30 * 60));

        unsafe {
            std::env::set_var("MVM_BUILDER_VM_TIMEOUT_SECS", "0");
        }
        let err = builder_vm_timeout().unwrap_err();
        assert!(format!("{err}").contains("greater than zero"), "got {err}");

        unsafe {
            match old {
                Some(v) => std::env::set_var("MVM_BUILDER_VM_TIMEOUT_SECS", v),
                None => std::env::remove_var("MVM_BUILDER_VM_TIMEOUT_SECS"),
            }
        }
    }

    #[test]
    fn unique_job_id_includes_pid_and_timestamp() {
        let id = unique_job_id();
        let pid = std::process::id().to_string();
        assert!(id.ends_with(&pid), "id missing pid suffix: {id}");
        assert!(id.contains('-'), "id missing separator: {id}");
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

    // ---------------------------------------------------------------
    // Plan 77 W6 — kernel-panic detector.
    //
    // `find_panic_line_in` is a pure scanner — tested directly. The
    // `wait_with_panic_detector` integration tests spawn `sleep` as a
    // stand-in for the libkrun supervisor (writes nothing on its own
    // to the console log, exits cleanly when killed or after its
    // configured duration). Tests run on Unix only because `sleep`
    // and `Child::kill` semantics are POSIX-specific.
    // ---------------------------------------------------------------

    #[test]
    fn find_panic_line_in_returns_none_when_banner_absent() {
        let buf = b"some boot log\nanother line\nfinal line\n";
        assert_eq!(find_panic_line_in(buf), None);
    }

    #[test]
    fn find_panic_line_in_extracts_full_line_when_banner_present() {
        let buf = b"[ 0.05 ] EXT4-fs: mounted\n\
                    [ 0.08 ] Kernel panic - not syncing: Requested init /sbin/foo failed (error -2).\n\
                    [ 0.09 ] more output\n";
        let line = find_panic_line_in(buf).expect("must match");
        assert!(line.contains("Kernel panic - not syncing"));
        assert!(line.contains("Requested init /sbin/foo failed"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn find_panic_line_in_handles_buffer_with_no_trailing_newline() {
        // Watcher may read partial output where the panic line is at
        // the end with no `\n` yet. The scanner still returns the
        // whole bufferred line so the watcher can fire immediately
        // instead of waiting for the next newline.
        let buf = b"[ 0.05 ] booting\n[ 0.08 ] Kernel panic - not syncing: detail";
        let line = find_panic_line_in(buf).expect("must match");
        assert!(line.contains("Kernel panic - not syncing: detail"));
    }

    #[test]
    fn find_panic_line_in_trims_trailing_carriage_return() {
        // Some console drivers emit `\r\n`; the line should be the
        // banner without the trailing `\r`.
        let buf = b"[ 0.08 ] Kernel panic - not syncing: oops\r\nnext line\n";
        let line = find_panic_line_in(buf).expect("must match");
        assert!(!line.ends_with('\r'), "got {line:?}");
        assert!(line.ends_with(": oops"));
    }

    #[test]
    fn find_panic_line_in_returns_match_at_start_of_buffer() {
        let buf = b"Kernel panic - not syncing: first thing\nsubsequent\n";
        let line = find_panic_line_in(buf).expect("must match");
        assert_eq!(line, "Kernel panic - not syncing: first thing");
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_panic_detector_returns_clean_when_no_console_log() {
        // No console log → falls back to plain wait; clean exit code
        // is propagated as-is.
        let mut child = Command::new("sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let outcome =
            wait_with_panic_detector(&mut child, None, Duration::from_millis(10)).expect("ok");
        match outcome {
            WaitOutcome::Clean(7) => {}
            other => panic!("expected Clean(7), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_panic_detector_returns_clean_when_log_never_contains_banner() {
        let scratch = TempDir::new().unwrap();
        let console = scratch.path().join("console.log");
        // Write a few benign lines — the watcher sees them, finds no
        // banner, the child exits cleanly, outcome is Clean.
        std::fs::write(&console, b"[ 0.01 ] boot\n[ 0.02 ] hello\n").unwrap();
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let outcome =
            wait_with_panic_detector(&mut child, Some(&console), Duration::from_millis(10))
                .expect("ok");
        match outcome {
            WaitOutcome::Clean(0) => {}
            other => panic!("expected Clean(0), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_panic_detector_kills_child_when_banner_appears() {
        let scratch = TempDir::new().unwrap();
        let console = scratch.path().join("console.log");

        // Long-running child standing in for the wedged libkrun
        // supervisor. Without panic detection this would block the
        // wait for the full 30s; with detection we expect it to be
        // killed within ~poll_interval of the banner write.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");

        // Writer thread: after a short delay, drop the banner into
        // the console log. The watcher should pick it up on the next
        // poll and signal the main thread to kill the sleep.
        let console_writer = console.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(
                &console_writer,
                b"[ 0.01 ] booting\n[ 0.08 ] Kernel panic - not syncing: test banner\n",
            )
            .expect("write console log fixture");
        });

        let start = std::time::Instant::now();
        let outcome =
            wait_with_panic_detector(&mut child, Some(&console), Duration::from_millis(10))
                .expect("ok");
        let elapsed = start.elapsed();
        writer.join().expect("writer thread join");

        match outcome {
            WaitOutcome::KernelPanic {
                panic_line,
                console_log_path,
            } => {
                assert!(
                    panic_line.contains("Kernel panic - not syncing: test banner"),
                    "panic_line: {panic_line:?}"
                );
                assert_eq!(console_log_path, console.display().to_string());
            }
            other => panic!("expected KernelPanic, got {other:?}"),
        }

        // The full sleep is 30s; detection-and-kill must complete in
        // a small fraction of that. 5s is generous slack for the
        // slowest plausible CI runner. A regression that loses the
        // kill (i.e. falls back to the wait blocking for 30s) blows
        // this assertion well before it would hang the suite.
        assert!(
            elapsed < Duration::from_secs(5),
            "panic detector did not kill the child promptly: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_panic_detector_handles_late_console_log_creation() {
        let scratch = TempDir::new().unwrap();
        let console = scratch.path().join("console.log");
        // No console.log on disk yet — the watcher must poll for it.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");

        let console_writer = console.clone();
        let writer = std::thread::spawn(move || {
            // Sleep past several poll cycles so the watcher exercises
            // its "file doesn't exist yet, retry" branch before the
            // write lands.
            std::thread::sleep(Duration::from_millis(200));
            std::fs::write(
                &console_writer,
                b"Kernel panic - not syncing: delayed banner\n",
            )
            .expect("write console log fixture");
        });

        let outcome =
            wait_with_panic_detector(&mut child, Some(&console), Duration::from_millis(10))
                .expect("ok");
        writer.join().unwrap();

        match outcome {
            WaitOutcome::KernelPanic { panic_line, .. } => {
                assert!(panic_line.contains("delayed banner"));
            }
            other => panic!("expected KernelPanic, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn seed_kernel_panic_error_display_mentions_panic_line_and_log_path() {
        // Pin the error's Display output so callers (Stage 0 audit
        // emit, user-facing UI) get a stable, parseable message
        // shape.
        let err = BuilderVmError::SeedKernelPanic {
            panic_line: "Kernel panic - not syncing: example".to_string(),
            console_log_path: "/tmp/example/console.log".to_string(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("Stage 0 seed VM kernel-panicked"), "{msg}");
        assert!(msg.contains("Kernel panic - not syncing: example"), "{msg}");
        assert!(msg.contains("/tmp/example/console.log"), "{msg}");
    }
}
