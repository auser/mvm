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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mvm_libkrun::{KrunContext, SupervisorConfig};
use serde::Deserialize;

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
        // 1. Validate caller-supplied inputs early; clearer
        //    errors than failing inside the libkrun FFI.
        self.validate_mounts(mounts)?;
        self.validate_job(job)?;

        // 2. Refuse to proceed on a host without libkrun. The
        //    `backends-builder-vm-libkrun` feature being compiled
        //    in doesn't imply the runtime library is installed.
        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::MicrosandboxUnavailable(format!(
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
        //    produces.
        let image = ensure_builder_vm_image()?;

        // 5. Allocate / locate the persistent `/nix-store`
        //    virtio-blk image. First build on a host pays the
        //    sparse-allocate cost; subsequent builds reuse the
        //    warm Nix store.
        let nix_store_img = ensure_nix_store_image(host_arch_tag(), u64::from(self.nix_store_mib))?;

        // 6. Stage the per-build job dir (`cmd.sh` + `env` +
        //    placeholder `result`). The job ID derives from a
        //    monotonic timestamp so concurrent invocations on
        //    one host don't clobber.
        let job_id = unique_job_id();
        let job_dir = builder_vm_cache_dir().join("jobs").join(&job_id);
        stage_job_dir(&job_dir, job)?;

        // 7. Build the `KrunContext` libkrun consumes. Three
        //    virtio-fs shares (work / out / job), one virtio-blk
        //    (Nix store), and the canonical cmdline pinned at the
        //    flake output.
        let vm_name = format!("mvm-builder-vm-{job_id}");
        let vm_state_dir = builder_vm_cache_dir().join("vms").join(&vm_name);
        std::fs::create_dir_all(&vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating builder VM state dir {}: {e}",
                vm_state_dir.display()
            ))
        })?;

        let krun = KrunContext::new(
            &vm_name,
            path_to_str(&image.kernel_path, "kernel_path")?,
            path_to_str(&image.rootfs_path, "rootfs_path")?,
        )
        .with_resources(self.vcpus, self.memory_mib)
        .with_cmdline(&image.cmdline)
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
        let exit_code = spawn_supervisor_and_wait(&supervisor_path, &cfg)?;
        if exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "supervisor exited with non-zero status ({exit_code}); \
                 guest stderr at {}",
                vm_state_dir.display()
            )));
        }

        // 9. Read the guest's structured exit status.
        let result = read_job_result(&job_dir)?;
        if result.exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "guest cmd.sh exited {} — stderr tail:\n{}",
                result.exit_code, result.stderr_tail
            )));
        }

        // 10. Validate the artifacts the cmd.sh script was
        //     supposed to drop into `/out`. The guest wrote them
        //     via virtio-fs — they show up on the host at
        //     `mounts.artifact_out`.
        let rootfs_path = mounts.artifact_out.join("rootfs.ext4");
        if !rootfs_path.is_file() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "builder VM exited cleanly but {} was not written",
                rootfs_path.display()
            )));
        }
        let kernel_path_out = mounts.artifact_out.join("vmlinux");
        let kernel_path = if kernel_path_out.is_file() {
            Some(kernel_path_out)
        } else {
            None
        };

        Ok(BuilderArtifacts {
            rootfs_path,
            kernel_path,
            // The Nix store path hash isn't trivially recoverable
            // from inside the host here — `nix build` printed it
            // to stdout inside the guest and the cmd.sh discards
            // it after copy. Plan 72 W5 can plumb it through via
            // a `/job/store-path` sidecar if cache-keying needs
            // it; for now the artifact dir's own digest is the
            // cache key the host carries.
            revision_hash: job_id,
            lock_hash: None,
            accessible: None,
        })
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

/// Resolved builder VM image — the W2 flake output the libkrun
/// launcher boots into.
struct BuilderVmImage {
    kernel_path: PathBuf,
    rootfs_path: PathBuf,
    cmdline: String,
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

    let cmdline = if cmdline_path.is_file() {
        std::fs::read_to_string(&cmdline_path)
            .map_err(|e| {
                BuilderVmError::ExtractionFailed(format!("reading {}: {e}", cmdline_path.display()))
            })?
            .trim()
            .to_string()
    } else {
        // Fallback — the cmdline the flake emits, verbatim from
        // Plan 72 §W2. Missing cmdline.txt means an older image
        // (pre-Plan 72 W2 finalisation); use the canonical
        // default rather than refuse to boot.
        "console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/mvm-builder-init".to_string()
    };

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

/// Stage `<job_dir>/cmd.sh` with the shell script the guest's
/// PID 1 (`mvm-builder-init`) executes. `cmd.sh` runs
/// `/bin/sh -eu` and is responsible for invoking `nix build`
/// against the user's flake and dropping `vmlinux` +
/// `rootfs.ext4` into `/out`.
fn stage_job_dir(job_dir: &Path, job: &BuilderJob) -> Result<(), BuilderVmError> {
    std::fs::create_dir_all(job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("creating job dir {}: {e}", job_dir.display()))
    })?;

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

cd /work
export NIX_CONFIG="experimental-features = nix-command flakes"
# Plan 72 W0's flake convention: workspace-path env var so
# flakes that reference the workspace root don't depend on
# relative-path resolution against the store-copied flake dir.
export MVM_WORKSPACE_PATH=/work

# `--impure` is what unblocks builds inside the VM when the
# flake has path inputs; `--no-write-lock-file` keeps the
# read-only `/work` mount from tripping EROFS.
NIX_OUT=$(nix build "${{FLAKE_REF}}#${{ATTR_PATH}}" \
    --no-link --print-out-paths --no-write-lock-file --impure)

if [ -z "$NIX_OUT" ]; then
    echo "nix build emitted no /nix/store output path" >&2
    exit 1
fi

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
        flake_ref = shell_single_quote_escape(&job.flake_ref),
        attr_path = shell_single_quote_escape(&job.attr_path),
    );

    let cmd_path = job_dir.join("cmd.sh");
    std::fs::write(&cmd_path, body).map_err(|e| {
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
        return Err(BuilderVmError::MicrosandboxUnavailable(format!(
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
    Err(BuilderVmError::MicrosandboxUnavailable(
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
            BuilderVmError::MicrosandboxUnavailable(format!(
                "spawn {}: {e}",
                supervisor_path.display()
            ))
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

    let status = child
        .wait()
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("wait on supervisor child: {e}")))?;
    Ok(status.code().unwrap_or(-1))
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
        //   - libkrun shared library missing → MicrosandboxUnavailable
        //   - builder VM image cache empty   → ExtractionFailed
        //   - mvm-libkrun-supervisor missing → MicrosandboxUnavailable
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
                BuilderVmError::MicrosandboxUnavailable(_) | BuilderVmError::ExtractionFailed(_)
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
        let job = BuilderJob {
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
    fn ensure_nix_store_image_creates_sparse_file_once() {
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
}
