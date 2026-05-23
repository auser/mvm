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

use crate::builder_vm::VmBackendForBuilder;

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
