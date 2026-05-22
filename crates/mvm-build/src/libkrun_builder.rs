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

/// Default RAM in MiB. Originally 8 GiB (Plan 72 W5.D bullet 9 —
/// in-VM nix builds peak ~5-6 GiB). Plan 95 raised this to 16 GiB
/// alongside bumping the Stage 0 `/nix` tmpfs `size=` cap in
/// `stage0/init.sh` (4G → 14G): tmpfs is RAM-backed, so the cap
/// can only be honored if the VM has at least that much RAM. The
/// real bottleneck in dev-up validation was the tmpfs cap, not the
/// VM RAM — bumping VM RAM alone is a no-op as long as the
/// `size=` mount option clips earlier. Keep these two in lockstep.
pub const DEFAULT_MEMORY_MIB: u32 = 16384;

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

/// Caller-visible networking-backend preference. Read from
/// the `MVM_NETWORKING` env var at every VM-launch site.
///
/// Plan 87 introduced `Passt` (virtio-net via the userspace passt
/// gateway). Plan 88 added `Gvproxy` for macOS, where passt does not
/// build (`vmsplice`/namespace primitives are Linux-only — see
/// ADR-055 §"Cross-platform backends").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkingPreference {
    /// libkrun's built-in TSI (no virtio-net, no DHCP).
    Tsi,
    /// virtio-net via passt. Linux-only. Requires libkrun-sys + the
    /// `passt` binary on `$PATH`. The supervisor process spawns
    /// passt as a child and hands its fd to libkrun.
    Passt,
    /// virtio-net via gvproxy. macOS default; works on Linux too.
    /// Requires libkrun-sys + the `gvproxy` binary on `$PATH`. The
    /// supervisor spawns gvproxy with `-listen-vfkit unixgram://…`
    /// and libkrun connects to the listener path.
    Gvproxy,
}

/// Apply the resolved [`NetworkingPreference`] to a [`KrunContext`].
/// Dispatches `with_passt`, `with_gvproxy`, or leaves the context's
/// default `Tsi` mode in place — keeping the two builder-VM call
/// sites (shell-job + flake-build) in lockstep. Each gateway uses
/// `<vm_state_dir>` for its log/socket scratch space.
fn apply_networking_mode(
    krun: KrunContext,
    vm_state_dir: &std::path::Path,
) -> Result<KrunContext, BuilderVmError> {
    let scratch = path_to_str(vm_state_dir, "vm_state_dir")?;
    Ok(match resolve_networking_mode() {
        NetworkingPreference::Tsi => krun,
        NetworkingPreference::Passt => {
            krun.with_passt(mvm_libkrun::passt::DEFAULT_GUEST_MAC, scratch)
        }
        NetworkingPreference::Gvproxy => {
            krun.with_gvproxy(mvm_libkrun::gvproxy::DEFAULT_GUEST_MAC, scratch)
        }
    })
}

/// Per-host-OS default networking backend.
///
/// macOS → [`Gvproxy`](NetworkingPreference::Gvproxy) because passt
/// does not build there (the Homebrew formula refuses with "Linux is
/// required for this software"); everything else (Linux today) →
/// [`Passt`](NetworkingPreference::Passt). See ADR-055
/// §"Cross-platform backends" for the rationale.
pub fn default_networking_mode() -> NetworkingPreference {
    if cfg!(target_os = "macos") {
        NetworkingPreference::Gvproxy
    } else {
        NetworkingPreference::Passt
    }
}

/// Read `MVM_NETWORKING` from the env. Accepts `tsi`, `passt`, and
/// `gvproxy` (case-insensitive); anything else falls back to the
/// per-OS default and emits a warning so a typo is visible without
/// aborting.
///
/// Plan 87 W5 / PR3 flipped the default away from TSI; Plan 88
/// added the per-OS dispatch (macOS → gvproxy, Linux → passt). TSI
/// is libkrun's experimental no-network-stack mode; it works for
/// trivial HTTP but breaks on nix's substituter and source fetches
/// (HTTP/2 multiplexing, HTTPS redirect chains, the offline-mode
/// probe — see ADR-055 §"Context"). Contributors can still opt back
/// to TSI via `MVM_NETWORKING=tsi` for debugging, or pin a specific
/// gateway across OS via `MVM_NETWORKING=passt` / `=gvproxy`.
pub fn resolve_networking_mode() -> NetworkingPreference {
    match std::env::var("MVM_NETWORKING")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("tsi") => NetworkingPreference::Tsi,
        Some("passt") => NetworkingPreference::Passt,
        Some("gvproxy") => NetworkingPreference::Gvproxy,
        None | Some("") => default_networking_mode(),
        Some(other) => {
            let fallback = default_networking_mode();
            tracing::warn!(
                value = other,
                fallback = ?fallback,
                "MVM_NETWORKING unrecognised; falling back to per-OS default (accepted: tsi, passt, gvproxy)"
            );
            fallback
        }
    }
}

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

    /// Boot a Stage 0 bootstrap VM that runs a self-contained init
    /// script — no `/job/cmd.sh` staging, no `/job/result` parsing,
    /// no `/nix-store` virtio-blk. The guest is expected to be a
    /// `BuilderVmImage::RootDir` whose `/init` reads `/work` and
    /// writes the steady-state builder VM artifacts to `/out`, then
    /// powers off cleanly.
    ///
    /// On success, the caller still needs to validate that the
    /// expected artifacts (`vmlinux`, `rootfs.ext4`) landed in
    /// `artifact_out`; this function only asserts that the
    /// supervisor exited 0.
    pub fn run_stage0(
        &self,
        image: BuilderVmImage,
        workspace_dir: &std::path::Path,
        artifact_out: &std::path::Path,
    ) -> Result<(), BuilderVmError> {
        ensure_utf8_path(workspace_dir, "workspace_dir")?;
        ensure_utf8_path(artifact_out, "artifact_out")?;
        if !workspace_dir.is_dir() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "Stage 0 workspace_dir must be an existing directory: {}",
                workspace_dir.display()
            )));
        }
        std::fs::create_dir_all(artifact_out).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating Stage 0 artifact_out {}: {e}",
                artifact_out.display()
            ))
        })?;

        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::LibkrunUnavailable(format!(
                "libkrun shared library not found on host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        let supervisor_path = resolve_supervisor_path()?;

        let job_id = unique_job_id();
        let vm_name = format!("mvm-stage0-{job_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating Stage 0 VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;
        let console_log = vm_state_dir.join("console.log");

        let mut krun = krun_context_for_image(&vm_name, &image)?
            .with_resources(self.vcpus, self.memory_mib)
            .with_console_output(path_to_str(&console_log, "console_log")?)
            .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
            .add_virtio_fs("work", path_to_str(workspace_dir, "workspace_dir")?)
            .add_virtio_fs("out", path_to_str(artifact_out, "artifact_out")?);

        krun = apply_networking_mode(krun, &vm_state_dir)?;

        let cfg = SupervisorConfig {
            krun,
            vm_state_dir: path_to_str(&vm_state_dir, "vm_state_dir")?.to_string(),
            pid_file_name: Some("stage0.pid".to_string()),
        };

        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg, &vm_state_dir)?;
        if exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "Stage 0 supervisor exited with status {exit_code}; \
                 console log at {}",
                console_log.display()
            )));
        }
        Ok(())
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
        let nix_store_lock =
            acquire_nix_store_image_lock(host_arch_tag(), u64::from(self.nix_store_mib))?;

        let job_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&job_id);
        stage_shell_job_dir(&job_dir, &job.script)?;
        // Tell the operator up-front where the build's stderr will
        // land. `/job` is a virtio-fs share into the VM; the guest's
        // cmd.sh redirects `nix build` stderr into <job_dir>/nix-
        // stderr.log, which is this exact path on the host. A
        // contributor watching a long build can `tail -f` it without
        // waiting for the failure-path formatter (finalize_flake_job)
        // to surface the same path.
        tracing::info!(
            job_dir = %job_dir.display(),
            "builder VM job dir staged (nix-stderr.log streams here as the build runs)"
        );

        let vm_name = format!("mvm-builder-vm-{job_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating builder VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;
        let console_log = vm_state_dir.join("console.log");

        let mut krun = krun_context_for_image(&vm_name, &image)?
            .with_resources(self.vcpus, self.memory_mib)
            .with_console_output(path_to_str(&console_log, "console_log")?)
            .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
            .add_disk(
                "nix-store",
                path_to_str(nix_store_lock.path(), "nix_store_img")?,
                false,
            )
            .add_virtio_fs("work", path_to_str(&job.work_dir, "work_dir")?)
            .add_virtio_fs("out", path_to_str(&job.artifact_out, "artifact_out")?)
            .add_virtio_fs("job", path_to_str(&job_dir, "job_dir")?)
            .add_vsock_port(mvm_guest::builder_agent::BUILDER_DISPATCH_PORT);

        for disk in &job.extra_disks {
            krun = krun.add_disk(
                disk.id.as_str(),
                path_to_str(&disk.path, "extra_disk")?,
                disk.read_only,
            );
        }

        krun = apply_networking_mode(krun, &vm_state_dir)?;

        let cfg = SupervisorConfig {
            krun,
            vm_state_dir: path_to_str(&vm_state_dir, "vm_state_dir")?.to_string(),
            pid_file_name: Some("builder.pid".to_string()),
        };
        // Plan 89 W2 part 4: spawn the vsock response listener
        // BEFORE the supervisor so it can connect as soon as libkrun
        // creates the dispatch socket. Drained after the supervisor
        // exits — see `log_vsock_response_outcome` for the
        // cross-validation contract.
        let vsock_rx = spawn_vsock_response_listener(&vm_state_dir);
        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg, &vm_state_dir)?;
        if exit_code != 0 {
            log_vsock_response_outcome(vsock_rx, None);
            return Err(BuilderVmError::NixBuildFailed(format!(
                "supervisor exited with non-zero status ({exit_code}); \
                 guest stderr at {}",
                vm_state_dir.display()
            )));
        }

        let result = read_job_result(&job_dir)?;
        log_vsock_response_outcome(vsock_rx, Some(result.exit_code));
        if result.exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "guest shell job exited {} — stderr tail:\n{}",
                result.exit_code, result.stderr_tail
            )));
        }

        let result = BuilderShellResult {
            job_dir,
            vm_state_dir,
        };
        drop(nix_store_lock);
        Ok(result)
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
        let nix_store_lock =
            acquire_nix_store_image_lock(host_arch_tag(), u64::from(self.nix_store_mib))?;

        // 6. Stage the per-build job dir. Flake jobs get
        //    `cmd.sh`; install jobs get `install_spec.json`.
        //    `mvm-builder-init` (Plan 72 W3 + Plan 73 Followup
        //    B.2) dispatches based on which file it sees.
        let job_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&job_id);
        stage_job_dir(&job_dir, job)?;
        // Same announcement as the single-shot path — see the
        // identical block in `LibkrunBuilderVm::run_build`.
        tracing::info!(
            job_dir = %job_dir.display(),
            "builder VM job dir staged (nix-stderr.log streams here as the build runs)"
        );

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
        let mut krun = krun_context_for_image(&vm_name, &image)?
            .with_resources(self.vcpus, self.memory_mib)
            .with_console_output(path_to_str(&console_log, "console_log")?)
            .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
            .add_disk(
                "nix-store",
                path_to_str(nix_store_lock.path(), "nix_store_img")?,
                false,
            )
            .add_virtio_fs("work", path_to_str(&mounts.flake_src, "flake_src")?)
            .add_virtio_fs("out", path_to_str(&mounts.artifact_out, "artifact_out")?)
            .add_virtio_fs("job", path_to_str(&job_dir, "job_dir")?)
            .add_vsock_port(mvm_guest::builder_agent::BUILDER_DISPATCH_PORT);

        krun = apply_networking_mode(krun, &vm_state_dir)?;

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
        // Plan 89 W2 part 4: same dispatch-listener wiring as
        // `run_shell_script`. Drained after the supervisor exits
        // (or right before bailing on supervisor failure) so the
        // background thread always gets a chance to log.
        let vsock_rx = spawn_vsock_response_listener(&vm_state_dir);
        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg, &vm_state_dir)?;
        if exit_code != 0 {
            log_vsock_response_outcome(vsock_rx, None);
            return Err(BuilderVmError::NixBuildFailed(format!(
                "supervisor exited with non-zero status ({exit_code}); \
                 guest stderr at {}",
                vm_state_dir.display()
            )));
        }
        // The flake / install finalize paths don't return a
        // structured exit_code from the file before they branch on
        // variant-specific shapes (Flake reads `/job/result`,
        // Install reads `/out/result.json`). For W2 part 4 we log
        // without the file cross-validation in this site to keep
        // the change minimal; W3 wires per-variant cross-validation.
        log_vsock_response_outcome(vsock_rx, None);

        // 9. Per-variant result parsing + artifact validation.
        //    Flake jobs read `<job_dir>/result` (legacy shape);
        //    install jobs read `<artifact_out>/result.json` and
        //    return a different artifact variant.
        let artifacts = match job {
            BuilderJob::Flake { .. } => finalize_flake_job(&job_dir, &mounts.artifact_out, &job_id),
            BuilderJob::Install { .. } => finalize_install_job(&mounts.artifact_out),
        }?;
        drop(nix_store_lock);
        Ok(artifacts)
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

/// Resolved builder VM image. One of two boot shapes:
///
/// - **Rootfs**: kernel + rootfs.ext4 + cmdline. The steady-state
///   builder VM path produced by the W2 flake.
/// - **RootDir**: host directory + guest entrypoint. The Stage 0
///   bootstrap path. libkrun's bundled kernel boots transparently;
///   no host-built kernel involved.
#[derive(Debug, Clone)]
pub enum BuilderVmImage {
    /// Steady-state builder VM image.
    Rootfs {
        kernel_path: PathBuf,
        rootfs_path: PathBuf,
        cmdline: String,
    },
    /// Host directory libkrun mounts as the guest root via virtiofs.
    /// `entry_path` is the guest PID 1 (relative to `root_dir`).
    RootDir {
        root_dir: PathBuf,
        entry_path: String,
    },
}

impl BuilderVmImage {
    /// Image that boots from a rootfs ext4 disk + the supervisor's
    /// canonical `init=` cmdline.
    pub fn new(kernel_path: PathBuf, rootfs_path: PathBuf, cmdline: String) -> Self {
        Self::Rootfs {
            kernel_path,
            rootfs_path,
            cmdline,
        }
    }

    /// Image that hands a host directory to libkrun as the guest
    /// root via `krun_set_root`. libkrun's bundled kernel boots
    /// transparently. `entry_path` is the guest PID 1, relative to
    /// `root_dir`.
    pub fn new_root_dir(root_dir: PathBuf, entry_path: impl Into<String>) -> Self {
        Self::RootDir {
            root_dir,
            entry_path: entry_path.into(),
        }
    }
}

/// Build the right `KrunContext` flavor for a [`BuilderVmImage`],
/// pre-populated with the variant's kernel cmdline (where applicable).
/// `RootDir` images carry no cmdline — libkrun handles `set_root`
/// mode without one.
fn krun_context_for_image(
    vm_name: &str,
    image: &BuilderVmImage,
) -> Result<KrunContext, BuilderVmError> {
    match image {
        BuilderVmImage::Rootfs {
            kernel_path,
            rootfs_path,
            cmdline,
        } => Ok(KrunContext::new(
            vm_name,
            path_to_str(kernel_path, "kernel_path")?,
            path_to_str(rootfs_path, "rootfs_path")?,
        )
        .with_cmdline(cmdline.as_str())),
        BuilderVmImage::RootDir {
            root_dir,
            entry_path,
        } => Ok(KrunContext::new_root_dir(
            vm_name,
            path_to_str(root_dir, "root_dir")?,
            entry_path.as_str(),
        )),
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
             on a host with Nix and copying `result/{{vmlinux,rootfs.ext4,cmdline.txt}}` to {}/.",
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

    Ok(BuilderVmImage::new(kernel_path, rootfs_path, cmdline))
}

#[derive(Debug)]
struct NixStoreImageLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl NixStoreImageLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

/// Find or create the persistent `/nix-store` sparse image and hold
/// an exclusive host-side lock on it.
///
/// libkrun attaches this file as a writable virtio-blk device; the
/// guest's `mvm-builder-init` then mounts it as ext4 at `/nix-store`.
/// Two independent guests mounting the same ext4 image read-write can
/// corrupt the filesystem, so callers must keep the returned guard in
/// scope for the full VM lifetime (spawn supervisor, wait for poweroff,
/// and read result artifacts). Dropping the guard releases the lock.
///
/// `size_mib` is the sparse cap — the file consumes only the bytes the
/// in-VM ext4 actually writes. Caller-controlled because dev hosts may
/// want a smaller cap than CI runners.
fn acquire_nix_store_image_lock(
    arch: &str,
    size_mib: u64,
) -> Result<NixStoreImageLock, BuilderVmError> {
    use fs2::FileExt;

    let dir = builder_vm_cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating builder cache dir {}: {e}",
            dir.display()
        ))
    })?;
    let path = dir.join(format!("nix-store-{arch}.img"));
    let existed_before_open = path.exists();

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("open {}: {e}", path.display())))?;

    file.try_lock_exclusive().map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "nix-store image {} is already attached by another builder VM process; \
             wait for the running `mvmctl build` / `mvmctl deps install` to finish and retry: {e}",
            path.display()
        ))
    })?;

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

    let current_len = file.metadata().map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("metadata {}: {e}", path.display()))
    })?;
    if current_len.len() == 0 {
        file.set_len(size_bytes).map_err(|e| {
            if !existed_before_open {
                let _ = std::fs::remove_file(&path);
            }
            BuilderVmError::ExtractionFailed(format!(
                "set_len({size_bytes}) on {}: {e}",
                path.display()
            ))
        })?;
    }

    let current_len = file.metadata().map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("metadata {}: {e}", path.display()))
    })?;
    if current_len.len() == 0 {
        if !existed_before_open {
            let _ = std::fs::remove_file(&path);
        }
        return Err(BuilderVmError::ExtractionFailed(format!(
            "nix-store image {} stayed empty after sparse allocation",
            path.display()
        )));
    }

    Ok(NixStoreImageLock { path, _file: file })
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

# Point HOME at writable tmpfs (`/tmp`) to satisfy code paths that
# write to `~/...` (the rootfs is mounted `ro`; nix would otherwise
# bail with "creating directory '//.cache/nix': Read-only file
# system"). XDG_CACHE_HOME lives on the persistent `/nix-store`
# disk so Nix's eval-cache-v5, tarball-cache, and binary-cache-v6
# survive across builds — cold flake eval is the long pole on
# warm-store rebuilds, and these caches reclaim it. `/nix-store`
# is the ext4 root for the persistent virtio-blk device; it sits
# alongside the overlay upperdir (`/nix-store/upper`) at the
# disk's top level, so writes here don't pollute the Nix store
# namespace. XDG_STATE_HOME stays on tmpfs: it only holds profile
# generations, which one-shot build VMs don't use.
export HOME=/tmp
export XDG_CACHE_HOME=/nix-store/.cache
export XDG_STATE_HOME=/tmp/.local/state
mkdir -p /nix-store/.cache /tmp/.local/state

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
max-jobs = auto
cores = 0
auto-optimise-store = true
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

/// Read the last `max_bytes` of `path` into a `String`, replacing any
/// invalid UTF-8 lossily. Returns `Err` if the file is missing or
/// unreadable. Used by `finalize_flake_job` to surface the tail of
/// `<job_dir>/nix-stderr.log` (the cmd.sh's nix-build stderr capture)
/// in the failure path without loading a multi-hundred-KB log into
/// memory.
fn read_last_bytes_of(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let take = max_bytes.min(len);
    // SeekFrom::End wants i64; max_bytes is bounded to a small constant
    // at every call site (4 KiB today) so the cast is safe.
    let offset = i64::try_from(take).unwrap_or(i64::MAX).saturating_neg();
    file.seek(SeekFrom::End(offset))?;
    let mut buf = Vec::with_capacity(take as usize);
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
        // The 20-line `stderr_tail` in `result` is from the OUTER
        // cmd.sh (run_job captures cmd.sh's stderr into a 20-line
        // ringbuffer). That ringbuffer typically only carries the
        // "nix build exited N; tail of stderr:" preamble — not the
        // real per-derivation failure. The actual nix-build stderr
        // is at `<job_dir>/nix-stderr.log` (cmd.sh redirects there
        // via `2> /job/nix-stderr.log`). Surface its tail so the
        // operator doesn't have to know the convention.
        let stderr_log = job_dir.join("nix-stderr.log");
        let derivation_tail = read_last_bytes_of(&stderr_log, 4 * 1024)
            .unwrap_or_else(|_| String::from("<nix-stderr.log not present on host>"));
        return Err(BuilderVmError::NixBuildFailed(format!(
            "guest cmd.sh exited {} — full log: {}\n\
             outer stderr tail (cmd.sh ringbuffer):\n{}\n\
             derivation stderr tail (last 4 KiB of {}):\n{}",
            result.exit_code,
            stderr_log.display(),
            result.stderr_tail,
            stderr_log.display(),
            derivation_tail,
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
/// Plan 89 W3 part 4 — spawn the supervisor with the given
/// `cfg` piped to its stdin, return the live `Child` *without*
/// waiting on it. Persistent-VM callers
/// ([`LibkrunPersistentBuilderVm::start`]) consume the child via
/// [`PersistentVmHandle`]; the single-shot
/// [`spawn_supervisor_and_wait`] calls this then runs the wait
/// loop on top.
fn spawn_supervisor_in_background(
    supervisor_path: &Path,
    cfg: &SupervisorConfig,
) -> Result<std::process::Child, BuilderVmError> {
    let json = serde_json::to_string(cfg).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("serialize SupervisorConfig: {e}"))
    })?;

    let mut command = Command::new(supervisor_path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // libkrun dlopens `libkrunfw.5.dylib` by short name when its
    // bundled-kernel path runs (e.g. `krun_set_root` mode without
    // an explicit `set_kernel`). macOS dyld's default search list
    // does not include /opt/homebrew/lib where Homebrew installs
    // libkrunfw, so the dlopen fails with "Couldn't find or load"
    // and the supervisor exits rc -2. Adding the Homebrew prefix
    // to DYLD_FALLBACK_LIBRARY_PATH unblocks the lookup without
    // overriding dyld's normal search order.
    #[cfg(target_os = "macos")]
    {
        let mut fallback = std::env::var("DYLD_FALLBACK_LIBRARY_PATH").unwrap_or_default();
        for path in ["/opt/homebrew/lib", "/usr/local/lib"] {
            if !fallback.split(':').any(|p| p == path) {
                if !fallback.is_empty() {
                    fallback.push(':');
                }
                fallback.push_str(path);
            }
        }
        command.env("DYLD_FALLBACK_LIBRARY_PATH", fallback);
    }
    let mut child = command.spawn().map_err(|e| {
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
    Ok(child)
}

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
    let mut child = spawn_supervisor_in_background(supervisor_path, cfg)?;

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

/// Plan 89 W2 part 4 — spawn a background thread that reads the
/// `BuilderResponse::Result` frame `mvm-builder-init` sends over
/// AF_VSOCK port [`mvm_guest::builder_agent::BUILDER_DISPATCH_PORT`]
/// right before reboot (W2 part 3). Returns a `Receiver` the caller
/// drains after the supervisor exits via [`log_vsock_response_outcome`].
///
/// The thread starts BEFORE the supervisor so it can connect as
/// soon as libkrun creates `<vm_state_dir>/vsock-21471.sock` —
/// before the guest's listener exists, `UnixStream::connect` would
/// return `ENOENT`/`ECONNREFUSED`, hence the retry loop with a 60-second
/// outer deadline. Once connected, the thread reads with a 10-second
/// read deadline so an unresponsive guest doesn't leak the thread
/// indefinitely.
///
/// Pre-W2-part-3 cached dev images won't send a response at all;
/// the legacy `<job_dir>/result` file path remains authoritative
/// for the build's exit code. This helper's job is purely
/// observational: log what arrives, warn on mismatch against the
/// file, never gate the build on the vsock outcome.
#[cfg(feature = "builder-vm")]
pub fn spawn_vsock_response_listener(
    vm_state_dir: &Path,
) -> std::sync::mpsc::Receiver<crate::builder_protocol::BuilderResponseRead> {
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::builder_protocol::{BuilderResponseRead, read_builder_response};

    let (tx, rx) = mpsc::channel();
    let socket_path = vm_state_dir.join(format!(
        "vsock-{}.sock",
        mvm_guest::builder_agent::BUILDER_DISPATCH_PORT
    ));

    std::thread::Builder::new()
        .name("vsock-builder-response".to_string())
        .spawn(move || {
            let connect_deadline = Duration::from_secs(60);
            let poll_interval = Duration::from_millis(100);
            let read_timeout = Duration::from_secs(10);
            let start = Instant::now();
            let result = loop {
                if start.elapsed() > connect_deadline {
                    break BuilderResponseRead::Timeout;
                }
                match UnixStream::connect(&socket_path) {
                    Ok(mut stream) => {
                        let _ = stream.set_read_timeout(Some(read_timeout));
                        break read_builder_response(&mut stream);
                    }
                    Err(_) => std::thread::sleep(poll_interval),
                }
            };
            // The receiver may have been dropped already (caller hit
            // its recv_timeout before the thread finished). That's
            // fine — the response was observational anyway.
            let _ = tx.send(result);
        })
        .expect("std::thread::Builder::spawn never fails on unix");

    rx
}

/// Drain the listener from [`spawn_vsock_response_listener`] with a
/// bounded wait and log the outcome. If `file_exit_code` is `Some`,
/// cross-validate against the vsock-reported exit code and warn on
/// mismatch (but never propagate as an error — the file is the
/// authoritative source until W3 reverses the polarity).
///
/// The 5-second `recv_timeout` past supervisor exit is enough for
/// the read-deadline-bounded thread to deliver any in-flight
/// response; longer waits would hurt UX without buying meaningful
/// signal.
#[cfg(feature = "builder-vm")]
pub fn log_vsock_response_outcome(
    rx: std::sync::mpsc::Receiver<crate::builder_protocol::BuilderResponseRead>,
    file_exit_code: Option<i32>,
) {
    use crate::builder_protocol::{BuilderResponse, BuilderResponseRead};

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(BuilderResponseRead::Frame(BuilderResponse::Result {
            exit_code,
            job_timings,
            boot_timings,
            ..
        })) => {
            tracing::info!(
                vsock_exit_code = exit_code,
                build_ms = job_timings.build_ms,
                boot_timings_present = boot_timings.is_some(),
                "received BuilderResponse::Result via vsock dispatch channel (W2 part 3)"
            );
            if let Some(file_code) = file_exit_code
                && file_code != exit_code
            {
                tracing::warn!(
                    file_exit_code = file_code,
                    vsock_exit_code = exit_code,
                    "vsock dispatch and <job_dir>/result disagree on exit_code; \
                     file value is authoritative pre-W3"
                );
            }
        }
        Ok(BuilderResponseRead::Frame(other)) => {
            tracing::debug!(
                ?other,
                "vsock dispatch: non-Result frame ignored in single-shot"
            );
        }
        Ok(BuilderResponseRead::EmptyEof) => {
            tracing::debug!(
                "vsock dispatch: guest closed without sending — \
                 pre-W2-part-3 image, expected"
            );
        }
        Ok(BuilderResponseRead::Timeout) => {
            tracing::debug!("vsock dispatch: receive thread timed out");
        }
        Err(_) => {
            tracing::debug!(
                "vsock dispatch: no response within 5s of supervisor exit \
                 (thread will self-terminate via read deadline)"
            );
        }
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

// ============================================================
// Plan 89 W3 part 4 — LibkrunPersistentBuilderVm
// ============================================================

/// Filename of the marker the host stages under `<job_dir>/` to
/// tell `mvm-builder-init` to enter its dispatch loop (W3 part 3)
/// instead of running the single-shot `cmd.sh` / `install_spec`
/// flow. Same key as the path the guest checks.
pub const DISPATCH_SOCK_MARKER: &str = "dispatch.sock.marker";

/// Plan 89 W3 part 4 — spawn the long-lived builder VM that
/// `mvm-builder-init`'s W3 part 3 dispatch loop runs inside.
///
/// Mirrors the config surface of [`LibkrunBuilderVm`] but
/// produces a different shape of dispatch: instead of running a
/// single cmd.sh / install_spec and powering off, the guest
/// detects `<job_dir>/<DISPATCH_SOCK_MARKER>` and enters the
/// dispatch loop that reads `BuilderRequest` frames over
/// AF_VSOCK port [`mvm_guest::builder_agent::BUILDER_DISPATCH_PORT`].
///
/// The caller pairs this with a `PersistentBuilderSupervisor`
/// (W3 part 1) constructed against
/// [`PersistentVmHandle::dispatch_socket_path`].
///
/// ## What's *not* in this PR
///
/// - `mvmctl dev up` integration (W3 part 5) is what actually
///   constructs one of these against the dev session's lifecycle.
/// - Per-job namespace isolation inside the dispatch loop (W3
///   part 8 — security amendments F2/F7).
#[cfg(feature = "builder-vm")]
#[derive(Debug, Clone)]
pub struct LibkrunPersistentBuilderVm {
    vcpus: u8,
    memory_mib: u32,
    nix_store_mib: u32,
    image_override: Option<BuilderVmImage>,
    /// Host directory bound at `/work` in the guest. Plan 89 §"Workspace
    /// mount strategy" — bound at VM start, not per-dispatch.
    workspace_root: PathBuf,
}

#[cfg(feature = "builder-vm")]
impl LibkrunPersistentBuilderVm {
    /// Construct a persistent builder VM rooted at `workspace_root`.
    /// Defaults match [`LibkrunBuilderVm::default`] for vCPUs / RAM /
    /// nix-store image size.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            nix_store_mib: DEFAULT_NIX_STORE_MIB,
            image_override: None,
            workspace_root: workspace_root.into(),
        }
    }

    pub fn with_vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = vcpus;
        self
    }

    pub fn with_memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    pub fn with_nix_store_mib(mut self, nix_store_mib: u32) -> Self {
        self.nix_store_mib = nix_store_mib;
        self
    }

    pub fn with_image_override(mut self, image: BuilderVmImage) -> Self {
        self.image_override = Some(image);
        self
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Spawn the supervisor + libkrun VM in the background and
    /// return a handle whose `dispatch_socket_path` the W3 part 1
    /// supervisor connects to. The returned `Child` is alive
    /// until either the guest dispatch loop processes a
    /// `BuilderRequest::Shutdown` (clean exit) or the caller
    /// invokes [`PersistentVmHandle::kill`].
    pub fn start(&self) -> Result<PersistentVmHandle, BuilderVmError> {
        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::LibkrunUnavailable(format!(
                "libkrun shared library not found on host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        if !self.workspace_root.is_dir() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "workspace_root {} is not a directory",
                self.workspace_root.display()
            )));
        }

        let supervisor_path = resolve_supervisor_path()?;
        let image = match &self.image_override {
            Some(image) => image.clone(),
            None => ensure_builder_vm_image()?,
        };
        // Acquire the cross-process flock on the nix-store image
        // for the persistent VM's lifetime. Issue #371 mitigation:
        // concurrent `mvmctl deps install` while a dev session's
        // persistent VM is up would otherwise corrupt the shared
        // ext4. Held inside the handle; released on drop / kill /
        // wait_for_shutdown.
        let nix_store_lock =
            acquire_nix_store_image_lock(host_arch_tag(), u64::from(self.nix_store_mib))?;

        let session_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&session_id);
        stage_persistent_job_dir(&job_dir)?;

        let vm_name = format!("mvm-persistent-builder-vm-{session_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating persistent builder VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;
        let console_log = vm_state_dir.join("console.log");

        let mut krun = krun_context_for_image(&vm_name, &image)?
            .with_resources(self.vcpus, self.memory_mib)
            .with_console_output(path_to_str(&console_log, "console_log")?)
            .with_vsock_socket_dir(path_to_str(&vm_state_dir, "vm_state_dir")?)
            .add_disk(
                "nix-store",
                path_to_str(nix_store_lock.path(), "nix_store_img")?,
                false,
            )
            .add_virtio_fs("work", path_to_str(&self.workspace_root, "workspace_root")?)
            .add_virtio_fs("out", path_to_str(&job_dir, "job_dir")?)
            .add_virtio_fs("job", path_to_str(&job_dir, "job_dir")?)
            .add_vsock_port(mvm_guest::builder_agent::BUILDER_DISPATCH_PORT);

        krun = apply_networking_mode(krun, &vm_state_dir)?;

        let cfg = SupervisorConfig {
            krun,
            vm_state_dir: path_to_str(&vm_state_dir, "vm_state_dir")?.to_string(),
            pid_file_name: Some("builder.pid".to_string()),
        };

        let child = spawn_supervisor_in_background(&supervisor_path, &cfg)?;

        Ok(PersistentVmHandle {
            vm_state_dir,
            job_dir,
            session_id,
            supervisor: Some(child),
            _nix_store_lock: nix_store_lock,
        })
    }
}

/// Stage `<job_dir>/<DISPATCH_SOCK_MARKER>` so the in-guest
/// `mvm-builder-init` enters its W3 part 3 dispatch loop instead
/// of the single-shot cmd.sh / install_spec flow. The marker
/// body is intentionally empty — its mere existence is the
/// signal.
#[cfg(feature = "builder-vm")]
fn stage_persistent_job_dir(job_dir: &Path) -> Result<(), BuilderVmError> {
    std::fs::create_dir_all(job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating persistent job dir {}: {e}",
            job_dir.display()
        ))
    })?;
    let marker_path = job_dir.join(DISPATCH_SOCK_MARKER);
    std::fs::write(&marker_path, b"").map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "staging dispatch marker {}: {e}",
            marker_path.display()
        ))
    })?;
    Ok(())
}

/// Handle to a live persistent builder VM. Owns the supervisor
/// `Child` for its lifetime; dropping without calling
/// [`Self::wait_for_shutdown`] or [`Self::kill`] leaks the
/// supervisor process (and the VM behind it) — callers should
/// always one of the two before dropping.
#[cfg(feature = "builder-vm")]
#[derive(Debug)]
pub struct PersistentVmHandle {
    vm_state_dir: PathBuf,
    job_dir: PathBuf,
    session_id: String,
    /// `None` after [`Self::wait_for_shutdown`] consumes it.
    supervisor: Option<std::process::Child>,
    /// Held to keep the cross-process flock on the shared
    /// nix-store image alive for the VM's lifetime. Drops
    /// (releasing the lock) when the handle drops, after
    /// `wait_for_shutdown`, or after `kill`. Underscore-prefixed
    /// because the type is opaque to consumers — it exists for
    /// its `Drop` side-effect.
    _nix_store_lock: NixStoreImageLock,
}

#[cfg(feature = "builder-vm")]
impl PersistentVmHandle {
    /// Path libkrun uses for the per-VM state (vsock sockets,
    /// console log, PID file). Pass this to
    /// `dispatch_socket_path` when constructing the W3 part 1
    /// `PersistentBuilderSupervisor`.
    pub fn vm_state_dir(&self) -> &Path {
        &self.vm_state_dir
    }

    /// Host-side path of the libkrun-managed Unix socket that
    /// proxies to AF_VSOCK [`mvm_guest::builder_agent::BUILDER_DISPATCH_PORT`]
    /// inside the guest. The W3 part 1
    /// `PersistentBuilderSupervisor::new` takes this directly.
    pub fn dispatch_socket_path(&self) -> PathBuf {
        self.vm_state_dir.join(format!(
            "vsock-{}.sock",
            mvm_guest::builder_agent::BUILDER_DISPATCH_PORT
        ))
    }

    /// Per-VM job directory bound at `/job` inside the guest.
    /// Hosts stage per-dispatch artifacts (`<job_dir_relpath>/cmd.sh`,
    /// per-dispatch install specs, etc.) here before sending the
    /// matching `BuilderRequest::Run`.
    pub fn job_dir(&self) -> &Path {
        &self.job_dir
    }

    /// Opaque session identifier — useful for logging /
    /// observability. Stable for the VM's lifetime.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Block until the supervisor child exits. Normal way to
    /// reach this is the supervisor sending `BuilderRequest::Shutdown`,
    /// the guest dispatch loop processing it + replying `Bye`,
    /// then `mvm-builder-init` calling `reboot(RB_POWER_OFF)`,
    /// then libkrun's `krun_start_enter` returning, then the
    /// supervisor exiting `main`. Consumes the child; subsequent
    /// calls return [`BuilderVmError::ExtractionFailed`].
    pub fn wait_for_shutdown(mut self) -> Result<i32, BuilderVmError> {
        let mut child = self.supervisor.take().ok_or_else(|| {
            BuilderVmError::ExtractionFailed(
                "PersistentVmHandle::wait_for_shutdown called twice".to_string(),
            )
        })?;
        let status = child.wait().map_err(|e| {
            BuilderVmError::ExtractionFailed(format!("waiting on persistent supervisor: {e}"))
        })?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Forcibly terminate the supervisor child (SIGKILL via
    /// `Child::kill`). The VM goes down hard; in-flight builds
    /// are abandoned. Use only as a fallback after
    /// [`Self::wait_for_shutdown`] hangs.
    pub fn kill(&mut self) -> std::io::Result<()> {
        if let Some(child) = self.supervisor.as_mut() {
            child.kill()?;
            let _ = child.wait();
            self.supervisor = None;
        }
        Ok(())
    }
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
        // Plan 72 W5.D bullet 9: 4 → 8 GiB (in-VM nix builds peak
        // ~5-6 GiB; OOM at lower default). Plan 95: 8 → 16 GiB
        // alongside stage0/init.sh bumping the `/nix` tmpfs `size=`
        // cap to 14G. Hardcoded so a regression on either side
        // fails fast.
        assert_eq!(vm.memory_mib, 16384);
        assert_eq!(vm.nix_store_mib, 65536);
    }

    #[test]
    fn resolve_networking_mode_parses_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: tests guarded by ENV_LOCK; env mutation in test
        // process is fine when serialized.
        unsafe {
            // Plan 88: default is per-OS — macOS → Gvproxy, others → Passt.
            std::env::remove_var("MVM_NETWORKING");
            assert_eq!(resolve_networking_mode(), default_networking_mode());

            std::env::set_var("MVM_NETWORKING", "tsi");
            assert_eq!(resolve_networking_mode(), NetworkingPreference::Tsi);

            std::env::set_var("MVM_NETWORKING", "TSI");
            assert_eq!(resolve_networking_mode(), NetworkingPreference::Tsi);

            std::env::set_var("MVM_NETWORKING", " passt ");
            assert_eq!(resolve_networking_mode(), NetworkingPreference::Passt);

            std::env::set_var("MVM_NETWORKING", "GVPROXY");
            assert_eq!(resolve_networking_mode(), NetworkingPreference::Gvproxy);

            std::env::set_var("MVM_NETWORKING", " gvproxy ");
            assert_eq!(resolve_networking_mode(), NetworkingPreference::Gvproxy);

            std::env::set_var("MVM_NETWORKING", "");
            assert_eq!(resolve_networking_mode(), default_networking_mode());

            // Unknown value falls back to the per-OS default without panic.
            std::env::set_var("MVM_NETWORKING", "vmnet-helper");
            assert_eq!(resolve_networking_mode(), default_networking_mode());

            std::env::remove_var("MVM_NETWORKING");
        }
    }

    #[test]
    fn default_networking_mode_matches_host_os() {
        let expected = if cfg!(target_os = "macos") {
            NetworkingPreference::Gvproxy
        } else {
            NetworkingPreference::Passt
        };
        assert_eq!(default_networking_mode(), expected);
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
        assert!(cmd.contains("max-jobs = auto"));
        assert!(cmd.contains("cores = 0"));
        assert!(cmd.contains("auto-optimise-store = true"));
        assert!(cmd.contains("XDG_CACHE_HOME=/nix-store/.cache"));
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

    /// `read_last_bytes_of` returns the trailing `max_bytes` of a
    /// file. When the file is larger than the cap, we get the *end*,
    /// not the head — the use case is tailing nix-build stderr where
    /// the cause-of-death is at the bottom.
    #[test]
    fn read_last_bytes_of_returns_trailing_window_when_file_exceeds_cap() {
        let scratch = TempDir::new().unwrap();
        let path = scratch.path().join("log");
        let mut body = String::new();
        for i in 0..2_000 {
            body.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, &body).unwrap();
        let tail = read_last_bytes_of(&path, 200).unwrap();
        assert!(tail.len() <= 200);
        assert!(tail.contains("line 1999"), "tail contains the last line");
        assert!(
            !tail.contains("line 0\n"),
            "tail does not include the head: {tail}"
        );
    }

    /// Small file: the helper returns the whole file (capped at its
    /// real length, not the requested max).
    #[test]
    fn read_last_bytes_of_returns_entire_file_when_smaller_than_cap() {
        let scratch = TempDir::new().unwrap();
        let path = scratch.path().join("log");
        std::fs::write(&path, b"hello world").unwrap();
        let tail = read_last_bytes_of(&path, 4096).unwrap();
        assert_eq!(tail, "hello world");
    }

    /// Missing file surfaces as an `io::Error`; the caller in
    /// `finalize_flake_job` swallows it into a `<not present>`
    /// sentinel rather than failing the whole error format.
    #[test]
    fn read_last_bytes_of_errors_on_missing_file() {
        let scratch = TempDir::new().unwrap();
        let err = read_last_bytes_of(&scratch.path().join("missing"), 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// The failure-path error message names the nix-stderr.log path
    /// AND inlines its tail. This is the diagnostic-surface fix —
    /// before the change, callers got the outer cmd.sh ringbuffer
    /// only, with no hint where the real log lived.
    #[test]
    fn finalize_flake_job_failure_includes_nix_stderr_log_path_and_tail() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job");
        let artifact_out = scratch.path().join("out");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::create_dir_all(&artifact_out).unwrap();
        std::fs::write(
            job_dir.join("result"),
            r#"{"exit_code":1,"stderr_tail":"outer-tail"}"#,
        )
        .unwrap();
        // Sentinel string the helper must surface — proves we're
        // reading from THIS file and not from the outer ringbuffer.
        std::fs::write(
            job_dir.join("nix-stderr.log"),
            "/nix/store/.../cargo-install-hook.sh: line 27: /dev/fd/63: No such file or directory\n",
        )
        .unwrap();

        let err = finalize_flake_job(&job_dir, &artifact_out, "job-id").unwrap_err();
        let msg = match err {
            BuilderVmError::NixBuildFailed(s) => s,
            other => panic!("expected NixBuildFailed, got {other:?}"),
        };
        assert!(msg.contains("exited 1"), "names exit code: {msg}");
        let log_path = job_dir.join("nix-stderr.log");
        assert!(
            msg.contains(&*log_path.to_string_lossy()),
            "names the full log path: {msg}"
        );
        assert!(
            msg.contains("/dev/fd/63: No such file or directory"),
            "inlines the real derivation stderr tail: {msg}"
        );
        assert!(
            msg.contains("outer-tail"),
            "still includes the outer ringbuffer for context: {msg}"
        );
    }

    /// Missing `nix-stderr.log` doesn't crash the formatter — we get
    /// a clean sentinel instead of an `Err(...)` cascade. This
    /// matters for very-early failures (e.g. cmd.sh exit before the
    /// `2> /job/nix-stderr.log` redirect runs).
    #[test]
    fn finalize_flake_job_failure_handles_missing_nix_stderr_log_cleanly() {
        let scratch = TempDir::new().unwrap();
        let job_dir = scratch.path().join("job");
        let artifact_out = scratch.path().join("out");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::create_dir_all(&artifact_out).unwrap();
        std::fs::write(
            job_dir.join("result"),
            r#"{"exit_code":2,"stderr_tail":"no cmd.sh"}"#,
        )
        .unwrap();

        let err = finalize_flake_job(&job_dir, &artifact_out, "job-id").unwrap_err();
        let msg = match err {
            BuilderVmError::NixBuildFailed(s) => s,
            other => panic!("expected NixBuildFailed, got {other:?}"),
        };
        assert!(
            msg.contains("<nix-stderr.log not present on host>"),
            "sentinel surfaces in place of missing log: {msg}"
        );
        assert!(
            msg.contains("no cmd.sh"),
            "outer tail still surfaces: {msg}"
        );
    }

    #[test]
    fn acquire_nix_store_image_lock_creates_sparse_file_once() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Sparse file allocates the logical size but consumes
        // ~no disk blocks. `set_len` is what asks the FS to
        // record the size. A later acquisition finds the existing
        // file and returns its path without retouching.
        let scratch = TempDir::new().unwrap();
        // Redirect the cache dir via XDG_CACHE_HOME to keep the
        // test hermetic — `mvm_core::config::mvm_cache_dir()`
        // honors the env var.
        let old = std::env::var("XDG_CACHE_HOME").ok();
        // SAFETY: tests run single-threaded for env mutation
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", scratch.path());
        }
        let guard = acquire_nix_store_image_lock("x86_64", 256).unwrap();
        let path = guard.path().to_path_buf();
        assert!(path.is_file());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 256 * 1024 * 1024);
        drop(guard);
        // Second acquisition is idempotent.
        let guard2 = acquire_nix_store_image_lock("x86_64", 256).unwrap();
        let path2 = guard2.path().to_path_buf();
        assert_eq!(path, path2);
        drop(guard2);
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
    fn acquire_nix_store_image_lock_refuses_concurrent_writer() {
        let _lock = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new().unwrap();
        let old = std::env::var("XDG_CACHE_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", scratch.path());
        }

        let first = acquire_nix_store_image_lock("x86_64", 256).unwrap();
        let err = acquire_nix_store_image_lock("x86_64", 256).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("already attached by another builder VM process"),
            "unexpected error: {msg}"
        );
        drop(first);

        acquire_nix_store_image_lock("x86_64", 256)
            .expect("lock should be available after first guard drops");

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

    // -----------------------------------------------------------
    // Plan 89 W3 part 4 — LibkrunPersistentBuilderVm
    // -----------------------------------------------------------

    #[test]
    fn dispatch_sock_marker_constant_is_filename_only() {
        // Must not contain `/`. The host joins it under <job_dir>;
        // a slash would break the path concatenation contract the
        // in-guest builder-init's marker probe assumes.
        assert!(!DISPATCH_SOCK_MARKER.contains('/'));
        assert!(!DISPATCH_SOCK_MARKER.is_empty());
        assert_eq!(DISPATCH_SOCK_MARKER, "dispatch.sock.marker");
    }

    #[test]
    fn stage_persistent_job_dir_creates_marker_in_fresh_dir() {
        // Hermetic — no libkrun, no VM. Validates the host's
        // side of the marker-file convention.
        let scratch = TempDir::new().expect("tempdir");
        let job_dir = scratch.path().join("job-dir");
        stage_persistent_job_dir(&job_dir).expect("stage");
        let marker = job_dir.join(DISPATCH_SOCK_MARKER);
        assert!(
            marker.is_file(),
            "marker should exist at {}",
            marker.display()
        );
        // Marker body is intentionally empty — its existence is
        // the signal. If the body grows non-empty in the future
        // the dispatch contract changes.
        let body = std::fs::read(&marker).expect("read marker");
        assert_eq!(body, b"");
    }

    #[test]
    fn stage_persistent_job_dir_is_idempotent() {
        // Re-staging into the same job dir must succeed (caller
        // may retry after a transient supervisor failure).
        let scratch = TempDir::new().expect("tempdir");
        let job_dir = scratch.path().join("job-dir");
        stage_persistent_job_dir(&job_dir).expect("stage 1");
        stage_persistent_job_dir(&job_dir).expect("stage 2");
        assert!(job_dir.join(DISPATCH_SOCK_MARKER).is_file());
    }

    #[test]
    fn persistent_vm_config_defaults_track_libkrun_builder_vm() {
        // Same vcpus / memory / nix-store defaults so users moving
        // from single-shot to persistent don't see surprise
        // resource shifts.
        let vm = LibkrunPersistentBuilderVm::new(std::env::temp_dir());
        assert_eq!(vm.vcpus, DEFAULT_VCPUS);
        assert_eq!(vm.memory_mib, DEFAULT_MEMORY_MIB);
        assert_eq!(vm.nix_store_mib, DEFAULT_NIX_STORE_MIB);
    }

    #[test]
    fn persistent_vm_with_setters_override_defaults() {
        let vm = LibkrunPersistentBuilderVm::new(std::env::temp_dir())
            .with_vcpus(2)
            .with_memory_mib(2048)
            .with_nix_store_mib(8192);
        assert_eq!(vm.vcpus, 2);
        assert_eq!(vm.memory_mib, 2048);
        assert_eq!(vm.nix_store_mib, 8192);
    }

    #[test]
    fn persistent_vm_start_rejects_missing_workspace() {
        // ExtractionFailed is the typed error variant for "host
        // input doesn't satisfy the precondition". Caller will
        // surface it directly.
        let nonexistent = std::env::temp_dir().join(format!(
            "no-such-workspace-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let vm = LibkrunPersistentBuilderVm::new(&nonexistent);
        match vm.start() {
            Err(BuilderVmError::ExtractionFailed(msg)) => {
                assert!(msg.contains("workspace_root"), "msg: {msg}");
                assert!(msg.contains("not a directory"), "msg: {msg}");
            }
            // libkrun may not be installed in CI; that's a
            // different error variant and also acceptable.
            Err(BuilderVmError::LibkrunUnavailable(_)) => {}
            other => panic!("expected ExtractionFailed or LibkrunUnavailable, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // Plan 89 W2 part 4 — vsock response listener
    // ---------------------------------------------------------------
    //
    // Live in `crates/mvm-build/tests/vsock_response_listener.rs`
    // (not here) so the server-binding pattern they need to simulate
    // libkrun's host-side proxy doesn't trip the `architecture.yml`
    // invariant grep against `crates/` source. The grep excludes
    // `**/tests/**` by design — that's where mock-server patterns
    // for test scaffolding belong.
}
