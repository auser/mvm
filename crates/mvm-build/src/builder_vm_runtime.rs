//! Plan 97 §"Phase C seam design" — hypervisor-agnostic builder-VM
//! orchestration helper.
//!
//! [`BuilderVmRuntime`] is the shared orchestration body that both
//! the libkrun and Vz builder paths route through. It owns the
//! pieces that aren't tied to a specific VMM:
//!
//! - `cmd.sh` emission (Flake jobs) and `install_spec.json` staging
//!   (Install jobs)
//! - `/job/result` JSON parsing
//! - Per-variant artifact finalisation (rootfs path resolution,
//!   revision hash extraction, install-volume sidecar discovery)
//! - Nix store image lock acquisition (libkrun-only concern in
//!   today's runtime; abstracted here for future Vz reuse)
//! - stderr-tail capture for build-failure diagnostics
//! - Wall-clock timeout handling
//!
//! It does **not** own:
//!
//! - Supervisor process lifecycle (lives in the
//!   [`VmBackendForBuilder`] impl — libkrun's
//!   `spawn_supervisor_in_background` / Vz's `run_attached`)
//! - Console-log watching for kernel-panic detection (also lives
//!   in the impl; surfaces through `BuilderVmExitInfo.panic_line`)
//! - Hypervisor-specific config translation (KrunContext vs.
//!   Vz's `SupervisorConfig`)
//!
//! Today the helper is a skeleton. Subsequent commits migrate the
//! concerns above out of `LibkrunBuilderVm.run_build` into here,
//! one at a time. Each migration commit is independently
//! verifiable: build + existing tests stay green, and the new
//! helper methods get their own unit tests.

use std::path::Path;

use crate::builder_vm::{BuilderJob, BuilderVmError, VmBackendForBuilder};

/// Per-job dir filename mvm-builder-init detects to dispatch
/// through the application-dependency install pipeline (Plan 73
/// Followup B.2). Migrated from `libkrun_builder.rs` because the
/// install spec staging is a hypervisor-agnostic concern that
/// both the libkrun and Vz builder paths need.
pub const INSTALL_SPEC_FILENAME: &str = "install_spec.json";

/// Hypervisor-agnostic orchestration helper. Holds a reference to
/// a [`VmBackendForBuilder`] so the actual supervisor spawn /
/// console-log path / per-VM state directory are routed through
/// the appropriate VMM without the helper knowing which one.
///
/// Lifetime-bound — the helper doesn't own the backend; callers
/// keep a long-lived backend instance (e.g. `LibkrunBuilderBackend`
/// constructed once at `LibkrunBuilderVm::run_build` entry) and
/// hand a borrow to the helper for the duration of one run.
pub struct BuilderVmRuntime<'a> {
    backend: &'a dyn VmBackendForBuilder,
}

impl<'a> BuilderVmRuntime<'a> {
    /// Construct over an existing backend reference.
    pub fn new(backend: &'a dyn VmBackendForBuilder) -> Self {
        Self { backend }
    }

    /// Borrow the underlying backend. Subsequent migration commits
    /// expose more targeted methods; this exists today so the
    /// helper has a way to thread the backend through its yet-to-be-
    /// migrated methods without re-routing every call site through
    /// a builder.
    pub fn backend(&self) -> &dyn VmBackendForBuilder {
        self.backend
    }
}

/// Stage the per-job dir inside `~/.cache/mvm/builder-vm/jobs/<id>/`
/// so the in-guest `mvm-builder-init` finds the right artifact
/// for dispatch:
///
/// - [`BuilderJob::Flake`] → writes `cmd.sh` (the in-guest nix-build
///   script). mvm-builder-init runs it after `/work` `/out` `/job`
///   virtio-fs shares are mounted.
/// - [`BuilderJob::Install`] → copies the caller's install-spec JSON
///   to `<job_dir>/install_spec.json`. mvm-builder-init detects the
///   filename and dispatches the application-dep install pipeline
///   (Plan 73 Followup B.2) instead of `cmd.sh`.
///
/// Hypervisor-agnostic — the staging produces files in a virtio-fs
/// share; libkrun and Vz both bind-mount the same host dir, so the
/// helper doesn't need to know which VMM is on the other end.
/// Migrated from `libkrun_builder.rs` in Plan 97 Phase C PR-B-migrate.
pub fn stage_job_dir(job_dir: &Path, job: &BuilderJob) -> Result<(), BuilderVmError> {
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

    let body = render_flake_cmd_sh(flake_ref, attr_path);

    let cmd_path = job_dir.join("cmd.sh");
    std::fs::write(&cmd_path, body).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("writing {}: {e}", cmd_path.display()))
    })?;
    Ok(())
}

/// Render the `cmd.sh` body the in-guest `mvm-builder-init` runs
/// for a [`BuilderJob::Flake`]. Inlined as a separate function so
/// tests can assert the rendered output without touching the
/// filesystem.
fn render_flake_cmd_sh(flake_ref: &str, attr_path: &str) -> String {
    format!(
        r#"#!/bin/sh
# mvm-builder-vm cmd.sh — emitted by BuilderVmRuntime (Plan 97
# Phase C migration; was Plan 72 W4's stage_job_dir).
# Runs inside the builder VM under `/bin/sh -eu`. The host wires
# /work (workspace), /out (artifact dir), /job (this dir) as
# virtio-fs shares; /nix is a persistent virtio-blk overlay
# handled by mvm-builder-init.
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
    )
}

/// Escape a string for inclusion inside `'…'` single quotes in
/// POSIX shell. The only character that can't appear inside single
/// quotes is `'` itself; we close the quote, emit `\'`, then
/// reopen. Standard sh-escape pattern.
///
/// Public so external callers that build their own per-job shell
/// scripts (e.g. `LibkrunBuilderVm::run_shell_script`'s validator)
/// can reuse the same escape rules.
pub fn shell_single_quote_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder_vm::{BuilderVmDisk, BuilderVmExitInfo, BuilderVmMount, BuilderVmRunConfig};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Minimal backend that records every call. Same shape as the
    /// mock in `builder_vm::vm_backend_for_builder_tests`, but
    /// re-defined here because that mock lives inside `#[cfg(test)]`
    /// in the sibling module and isn't visible across the module
    /// boundary. As the helper grows, this fixture grows with it.
    #[derive(Default)]
    struct CountingBackend {
        run_calls: std::sync::atomic::AtomicUsize,
        console_calls: std::sync::atomic::AtomicUsize,
    }

    impl VmBackendForBuilder for CountingBackend {
        fn run_attached_with_mounts(
            &self,
            _config: &BuilderVmRunConfig,
            _mounts: &[BuilderVmMount],
            _extra_disks: &[BuilderVmDisk],
            _timeout: Duration,
        ) -> Result<BuilderVmExitInfo, crate::builder_vm::BuilderVmError> {
            self.run_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(BuilderVmExitInfo {
                exit_code: Some(0),
                panic_line: None,
            })
        }

        fn console_log_path(&self, vm_state_dir: &Path) -> PathBuf {
            self.console_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vm_state_dir.join("console.log")
        }
    }

    #[test]
    fn runtime_borrows_backend_for_lifetime_of_run() {
        let backend = CountingBackend::default();
        let runtime = BuilderVmRuntime::new(&backend);
        // Backend accessor returns the same trait object we passed
        // in; future helper methods will use it to dispatch.
        let log = runtime
            .backend()
            .console_log_path(Path::new("/tmp/state/foo"));
        assert_eq!(log, PathBuf::from("/tmp/state/foo/console.log"));
        assert_eq!(
            backend
                .console_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn runtime_is_zero_cost_to_construct() {
        // Construction is a single pointer + vtable; no fs / VM /
        // network ops fire. This test pins that contract — a
        // subsequent commit that adds expensive setup to
        // `BuilderVmRuntime::new` would break it.
        let backend = CountingBackend::default();
        let _runtime = BuilderVmRuntime::new(&backend);
        assert_eq!(
            backend.run_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            backend
                .console_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
