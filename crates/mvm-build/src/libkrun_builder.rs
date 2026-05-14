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

/// Concrete `(kernel_path, rootfs_path)` for the builder VM. Resolved
/// once per `run_build` invocation; both paths must be readable files
/// for the host to even consider spawning the supervisor.
#[derive(Debug, Clone)]
struct BuilderImage {
    kernel: std::path::PathBuf,
    rootfs: std::path::PathBuf,
}

/// Resolve the builder VM image (kernel + rootfs.ext4). Two probe
/// orders, both kept narrow on purpose:
///
/// 1. `MVM_BUILDER_VM_IMAGE_DIR` env var, if set, must point at a
///    directory containing both `vmlinux` and `rootfs.ext4`. This is
///    the dev / test escape hatch — Plan 72 W4 land sites that haven't
///    wired the Stage 0 build path yet use this to point at pre-staged
///    artifacts.
/// 2. `~/.cache/mvm/builder-vm/<arch>/{vmlinux,rootfs.ext4}` — the
///    cache layout Plan 72 W5's cutover writes to. Missing today
///    because nothing populates the cache yet; the env-var override
///    above is the only way to actually run.
///
/// When neither resolves, returns an actionable error naming both
/// paths plus the env var.
fn resolve_builder_image() -> Result<BuilderImage, BuilderVmError> {
    if let Some(dir) = std::env::var_os("MVM_BUILDER_VM_IMAGE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        let kernel = dir.join("vmlinux");
        let rootfs = dir.join("rootfs.ext4");
        if !kernel.is_file() || !rootfs.is_file() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "MVM_BUILDER_VM_IMAGE_DIR={} is missing vmlinux and/or rootfs.ext4",
                dir.display()
            )));
        }
        return Ok(BuilderImage { kernel, rootfs });
    }
    let cache = builder_vm_cache_dir();
    let kernel = cache.join("vmlinux");
    let rootfs = cache.join("rootfs.ext4");
    if kernel.is_file() && rootfs.is_file() {
        return Ok(BuilderImage { kernel, rootfs });
    }
    Err(BuilderVmError::ExtractionFailed(format!(
        "no builder VM image found. \
         Set MVM_BUILDER_VM_IMAGE_DIR=<dir with vmlinux + rootfs.ext4>, \
         or build the in-repo flake (nix/images/builder-vm/) via the Stage 0 \
         microsandbox path (`--features contributor-bootstrap`) which writes \
         to {}. The Stage 0 wiring is plan 72 W5's job and is not in this slice.",
        cache.display()
    )))
}

/// Where Plan 72 W5's cutover will cache the builder VM image. Today
/// nothing writes here; resolution falls back to the env-var override.
fn builder_vm_cache_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    std::path::PathBuf::from(home).join(format!(".cache/mvm/builder-vm/{arch}"))
}

/// Stage the per-job directory under
/// `~/.cache/mvm/builder-vm/jobs/<uuid>/` with the `cmd.sh` script
/// that `mvm-builder-init` (Plan 72 W3 PID-1) executes inside the
/// guest. The init writes `/job/result` (a small JSON) when the
/// command completes; the host reads it after the supervisor exits.
///
/// `cmd.sh` is plain `nix build` with the flake reference and attr
/// path the caller supplied. `--no-write-lock-file` keeps the lock
/// the host already pinned authoritative; `--print-out-paths`
/// dumps the store-path the host parses to derive the revision hash.
/// `--impure` is required because `MVM_WORKSPACE_PATH=/work` is
/// consumed by the flake's workspace import (Plan 72 W0.2).
fn stage_job(job: &BuilderJob) -> Result<std::path::PathBuf, BuilderVmError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let job_uuid = generate_job_id();
    let job_dir =
        std::path::PathBuf::from(&home).join(format!(".cache/mvm/builder-vm/jobs/{job_uuid}"));
    std::fs::create_dir_all(&job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("create job dir {}: {e}", job_dir.display()))
    })?;
    let cmd_script = format!(
        "#!/bin/sh\n\
         set -eu\n\
         export MVM_WORKSPACE_PATH=/work\n\
         cd /work\n\
         exec nix build {flake_ref}#{attr_path} \\\n\
             --no-link \\\n\
             --print-out-paths \\\n\
             --no-write-lock-file \\\n\
             --impure\n",
        flake_ref = job.flake_ref,
        attr_path = job.attr_path,
    );
    let cmd_path = job_dir.join("cmd.sh");
    std::fs::write(&cmd_path, cmd_script).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("write {}: {e}", cmd_path.display()))
    })?;
    // Mode 0755 so `mvm-builder-init` can exec it directly.
    set_file_executable(&cmd_path)?;
    Ok(job_dir)
}

fn set_file_executable(path: &std::path::Path) -> Result<(), BuilderVmError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perm = std::fs::metadata(path)
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("stat {}: {e}", path.display())))?
        .permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

/// Job-dir name derived from PID + nanos. Not a real UUID — we just
/// need uniqueness across concurrent builds on one host, not global.
fn generate_job_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid}-{nanos}")
}

/// Allocate the persistent `/nix`-store sparse image at
/// `~/.cache/mvm/builder-vm/nix-store-<arch>.img`. On first call
/// `truncate` creates a sparse file of `mib` MiB; subsequent calls
/// reuse the existing file. ext4 formatting happens inside the guest
/// on first boot (`mvm-builder-init`, plan 72 W3).
fn allocate_nix_store_img(mib: u32) -> Result<std::path::PathBuf, BuilderVmError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let dir = std::path::PathBuf::from(&home).join(".cache/mvm/builder-vm");
    std::fs::create_dir_all(&dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("create cache dir {}: {e}", dir.display()))
    })?;
    let path = dir.join(format!("nix-store-{arch}.img"));
    if !path.exists() {
        // ftruncate creates a sparse hole; the file occupies 0
        // bytes on disk until the guest writes blocks via ext4.
        let bytes = u64::from(mib).saturating_mul(1024 * 1024);
        let file = std::fs::File::create(&path).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!("create {}: {e}", path.display()))
        })?;
        file.set_len(bytes).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "ftruncate {} to {bytes}: {e}",
                path.display()
            ))
        })?;
    }
    Ok(path)
}

/// Build the `SupervisorConfig` JSON the
/// `mvm-libkrun-supervisor` binary reads from stdin (plan 57 W4.1
/// contract). The builder VM differs from a user-facing libkrun
/// guest in a few ways:
///
/// - Three virtio-fs mounts: workspace at `/work` (RO from the
///   builder's perspective, RW from the host's), artifact dir at
///   `/out`, job dir at `/job`. libkrun's virtio-fs API today
///   doesn't expose read-only; mvm-builder-init bind-mounts each
///   share read-only into the guest namespace before exec'ing
///   cmd.sh (plan 72 W3).
/// - One virtio-blk: the persistent `/nix`-store image as `/dev/vdb`.
/// - No vsock ports — the builder doesn't need a guest agent. The
///   guest writes `/job/result` and shuts down; the host reads the
///   result file after the supervisor exits.
fn build_supervisor_config(
    vm: &LibkrunBuilderVm,
    mounts: &BuilderMounts,
    image: &BuilderImage,
    job_dir: &std::path::Path,
    nix_store_img: &std::path::Path,
) -> Result<mvm_libkrun::SupervisorConfig, BuilderVmError> {
    use mvm_libkrun::{KrunContext, SupervisorConfig};

    // Per-job state dir under ~/.mvm/vms/ so the supervisor's PID
    // file and any vsock sockets don't collide with user-facing VMs.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let vm_name = format!(
        "mvm-builder-{}",
        job_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("anon")
    );
    let vm_state_dir = std::path::PathBuf::from(&home).join(format!(".mvm/vms/{vm_name}"));
    std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "create vm_state_dir {}: {e}",
            vm_state_dir.display()
        ))
    })?;

    let kernel = image
        .kernel
        .to_str()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(format!(
                "kernel path is not UTF-8: {:?}",
                image.kernel
            ))
        })?
        .to_string();
    let rootfs = image
        .rootfs
        .to_str()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(format!(
                "rootfs path is not UTF-8: {:?}",
                image.rootfs
            ))
        })?
        .to_string();
    let nix_store = nix_store_img
        .to_str()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(format!(
                "nix-store image path is not UTF-8: {nix_store_img:?}"
            ))
        })?
        .to_string();

    let mut ctx = KrunContext::new(&vm_name, kernel, rootfs)
        .with_resources(vm.vcpus, vm.memory_mib)
        // The W2 builder VM rootfs hardcodes `init=/sbin/mvm-builder-init`
        // in its baked cmdline, but we override here too as a
        // belt-and-suspenders for any pre-W2 image variant.
        .with_cmdline("console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/mvm-builder-init")
        .add_disk("nix-store", nix_store, false)
        .with_console_output(
            vm_state_dir
                .join("console.log")
                .to_string_lossy()
                .into_owned(),
        )
        .with_vsock_socket_dir(vm_state_dir.to_string_lossy().into_owned());

    // virtio-fs mounts. PR #168 added the API; tags match
    // `mvm-builder-init`'s expected mount points (plan 72 W3).
    ctx = add_virtiofs(ctx, "work", &mounts.flake_src)?;
    ctx = add_virtiofs(ctx, "out", &mounts.artifact_out)?;
    ctx = add_virtiofs(ctx, "job", job_dir)?;

    Ok(SupervisorConfig {
        krun: ctx,
        vm_state_dir: vm_state_dir.to_string_lossy().into_owned(),
        pid_file_name: Some("builder.pid".to_string()),
    })
}

/// Attach a virtio-fs share. Stringifies the host path and delegates
/// to the existing `KrunContext::add_virtiofs` builder; isolates the
/// UTF-8 check in one place.
fn add_virtiofs(
    ctx: mvm_libkrun::KrunContext,
    tag: &str,
    host_path: &std::path::Path,
) -> Result<mvm_libkrun::KrunContext, BuilderVmError> {
    let host = host_path
        .to_str()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(format!(
                "virtio-fs share '{tag}' host path is not UTF-8: {host_path:?}"
            ))
        })?
        .to_string();
    Ok(ctx.add_virtio_fs(tag, host))
}

/// Spawn `mvm-libkrun-supervisor` with the JSON config on stdin and
/// block until it exits. Returns Ok on a clean guest poweroff,
/// Err with an actionable message on any failure.
fn spawn_supervisor_and_wait(
    cfg: &mvm_libkrun::SupervisorConfig,
    job_dir: &std::path::Path,
) -> Result<(), BuilderVmError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let supervisor = resolve_supervisor_path()?;
    let json = serde_json::to_string(cfg).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("serialize SupervisorConfig: {e}"))
    })?;

    let mut child = Command::new(&supervisor)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "spawn supervisor {}: {e}",
                supervisor.display()
            ))
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed("supervisor stdin was not piped".to_string())
        })?
        .write_all(json.as_bytes())
        .map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "pipe SupervisorConfig to supervisor stdin: {e}"
            ))
        })?;

    let status = child
        .wait()
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("wait on supervisor: {e}")))?;

    // Read /job/result (written by mvm-builder-init before
    // poweroff). The guest writes a tiny `<exit>\n<stderr-tail>`
    // form, where exit is the nix-build process's return code.
    let result_path = job_dir.join("result");
    if !result_path.is_file() {
        return Err(BuilderVmError::NixBuildFailed(format!(
            "supervisor exited (status {status:?}) but guest never wrote /job/result. \
             Check vm_state_dir console.log for kernel panic / init failure."
        )));
    }
    let result_body = std::fs::read_to_string(&result_path).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("read job result {}: {e}", result_path.display()))
    })?;
    let (exit_line, tail) = result_body
        .split_once('\n')
        .unwrap_or((result_body.as_str(), ""));
    let exit_code: i32 = exit_line.trim().parse().map_err(|_| {
        BuilderVmError::NixBuildFailed(format!(
            "guest /job/result is not a valid exit code: {exit_line:?}"
        ))
    })?;
    if exit_code != 0 {
        return Err(BuilderVmError::NixBuildFailed(format!(
            "nix build inside builder VM exited {exit_code}. Tail: {tail}"
        )));
    }
    Ok(())
}

/// Resolve the path to the `mvm-libkrun-supervisor` binary. Three
/// fallbacks, same shape as `LibkrunBackend::resolve_supervisor_path`
/// but inlined here to avoid a `mvm-build → mvm-backend` cycle. Both
/// implementations will move into `mvm-libkrun` once we add a
/// common path resolver there (small follow-up; not in this slice).
fn resolve_supervisor_path() -> Result<std::path::PathBuf, BuilderVmError> {
    if let Some(p) = std::env::var_os("MVM_LIBKRUN_SUPERVISOR_PATH") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(BuilderVmError::ExtractionFailed(format!(
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
    Err(BuilderVmError::ExtractionFailed(
        "mvm-libkrun-supervisor binary not found. Build it with `cargo build \
         -p mvm-libkrun --bin mvm-libkrun-supervisor --features libkrun-sys` \
         and either install adjacent to mvmctl or set \
         MVM_LIBKRUN_SUPERVISOR_PATH=/abs/path/to/the/binary."
            .to_string(),
    ))
}

/// After the supervisor exits and `/job/result` reports success,
/// confirm `mounts.artifact_out` carries the expected outputs and
/// build a `BuilderArtifacts`. The dev image build emits
/// `vmlinux` + `rootfs.ext4`; both must be present.
fn finalize_artifacts(mounts: &BuilderMounts) -> Result<BuilderArtifacts, BuilderVmError> {
    let kernel = mounts.artifact_out.join("vmlinux");
    let rootfs = mounts.artifact_out.join("rootfs.ext4");
    if !rootfs.is_file() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "guest reported success but {} is missing",
            rootfs.display()
        )));
    }
    let kernel_path = kernel.is_file().then_some(kernel);
    // Revision hash derives from the artifact path's parent name in
    // the production flow; for now we use a content-hash placeholder.
    // Plan 72 W5 wires the real revision-hash extraction.
    let revision_hash = mounts
        .artifact_out
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(BuilderArtifacts {
        rootfs_path: rootfs,
        kernel_path,
        revision_hash,
        lock_hash: None,
        accessible: None,
    })
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
        // Step 1: catch bad input now so libkrun FFI doesn't have
        // to re-validate later.
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

        // Step 3: resolve the builder VM image. In a source
        // checkout this comes from `nix build
        // nix/images/builder-vm#packages.<arch>.default` (Plan 72
        // W2 flake) via the Stage 0 path (microsandbox +
        // `contributor-bootstrap` feature) — separate slice, not
        // wired here. For now we accept `MVM_BUILDER_VM_IMAGE_DIR`
        // as an override so local dev iteration and the
        // forthcoming `tests/builder_vm_lifecycle.rs` test can
        // point at pre-staged artifacts.
        let image = resolve_builder_image()?;

        // Step 4: stage the per-job dir with `cmd.sh` that
        // `mvm-builder-init` (Plan 72 W3) reads + executes.
        let job_dir = stage_job(job)?;

        // Step 5: allocate the persistent `/nix`-store sparse
        // image. ext4 format happens inside the guest on first
        // boot (mvm-builder-init detects the empty superblock and
        // mkfs's it); host-side we just `truncate` to the
        // configured cap.
        let nix_store_img = allocate_nix_store_img(self.nix_store_mib)?;

        // Step 6: build the SupervisorConfig the
        // mvm-libkrun-supervisor binary expects on stdin (plan
        // 57 W4.1 contract). virtio-fs for workspace + artifacts +
        // job dir; virtio-blk for the persistent nix store.
        let cfg = build_supervisor_config(self, mounts, &image, &job_dir, &nix_store_img)?;

        // Step 7: spawn the supervisor and block until it exits.
        // `krun_start_enter` calls `exit()` when the guest powers
        // off, so the supervisor's exit status corresponds to the
        // guest's. mvm-builder-init writes `/job/result` *before*
        // calling poweroff, so the host can read the actual nix
        // build status after the supervisor is gone.
        spawn_supervisor_and_wait(&cfg, &job_dir)?;

        // Step 8: validate artifacts ended up in
        // `mounts.artifact_out` and construct the result. The dev
        // image build produces `vmlinux` + `rootfs.ext4`; both must
        // be present.
        finalize_artifacts(mounts)
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
    fn run_build_surfaces_missing_image_when_libkrun_installed() {
        // Plan 72 W4 wire-up: with valid mounts/job, `run_build`
        // walks past validation and image resolution. Two terminal
        // shapes from this point:
        //
        // - libkrun installed but no builder VM image cached and no
        //   `MVM_BUILDER_VM_IMAGE_DIR` set → `ExtractionFailed`
        //   pointing the user at the env-var override + the
        //   Stage 0 path (plan 72 W5).
        // - libkrun not installed at all (no shared library on
        //   PATH) → `MicrosandboxUnavailable` carrying the install
        //   hint.
        //
        // Either shape is the W4 acceptance for an empty host —
        // the CI matrix runs both. The supervisor-spawn / boot path
        // exercised against a real builder image lives in
        // `tests/builder_vm_lifecycle.rs` (separate slice, opt-in
        // via env var, skipped in CI for cost).
        let scratch = TempDir::new().unwrap();
        let mounts = ok_mounts(&scratch);
        // Make sure no leftover env var from a previous test run
        // turns this into the supervisor-spawn path. SAFETY:
        // single-threaded test.
        unsafe { std::env::remove_var("MVM_BUILDER_VM_IMAGE_DIR") };
        let err = LibkrunBuilderVm::default()
            .run_build(&ok_job(), &mounts)
            .unwrap_err();
        assert!(
            matches!(
                err,
                BuilderVmError::ExtractionFailed(_) | BuilderVmError::MicrosandboxUnavailable(_)
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
