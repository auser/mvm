//! Rust bindings for Red Hat libkrun (Linux KVM, macOS Hypervisor.framework).
//!
//! libkrun is a library-style VMM: linked directly into the calling binary
//! rather than spawned as a separate daemon. On Linux it uses KVM; on
//! macOS it uses Hypervisor.framework. It is the only VMM mvm carries
//! that runs on both macOS Apple Silicon **and** macOS Intel without
//! Lima.
//!
//! # Build modes
//!
//! - **default** (no feature) — no FFI, no link to libkrun. [`start`]
//!   and [`stop`] return [`Error::NotYetWired`]. The workspace compiles
//!   on hosts without libkrun installed.
//! - **`libkrun-sys`** — bindgen-generated FFI from `libkrun.h` plus
//!   `-lkrun` linking. [`start`] and [`stop`] dispatch through
//!   [`sys::Context`] into real libkrun calls.
//!
//! Plan 57 W1 wires the bindings; W2 adds the macOS codesigning
//! entitlement; W3 validates an end-to-end boot. This crate stays
//! narrowly focused on the FFI; backend dispatch and lifecycle live in
//! `mvm-backend` and `mvm-cli`.

use std::path::Path;

#[cfg(feature = "libkrun-sys")]
mod sys;

#[cfg(feature = "libkrun-sys")]
pub use sys::{KernelFormat, LogLevel};

/// Errors returned by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// libkrun is not installed on the host (no shared library found at
    /// any of the standard locations checked by [`is_available`]).
    NotInstalled {
        /// Suggested install command for the user.
        install_hint: &'static str,
    },
    /// Built without the `libkrun-sys` feature — the FFI bindings are
    /// compiled out so [`start`] / [`stop`] cannot dispatch. Rebuild
    /// with `--features libkrun-sys` on a host where libkrun is
    /// installed.
    NotYetWired {
        /// Tracking issue / plan reference.
        tracking: &'static str,
    },
    /// libkrun returned a negative errno from one of its C functions.
    /// The value is the raw return code (which libkrun documents as
    /// `-EINVAL`, `-ENOMEM`, etc. for most calls).
    Krun(i32),
    /// A path or string argument contained an interior NUL byte or
    /// was not representable as UTF-8 / a C string.
    InvalidCString,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled { install_hint } => {
                write!(f, "libkrun is not installed on this host. {install_hint}")
            }
            Self::NotYetWired { tracking } => write!(
                f,
                "libkrun FFI is not compiled into this build (tracking: {tracking}). \
                 Rebuild with `--features libkrun-sys` on a host with libkrun installed."
            ),
            Self::Krun(rc) => write!(f, "libkrun call failed with rc {rc}"),
            Self::InvalidCString => write!(
                f,
                "argument contained an interior NUL byte or non-UTF-8 path"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Detect whether libkrun is installed on the host by probing for the
/// shared library at the standard install locations.
///
/// **Not the same as "is functional"** — even if `is_available()`
/// returns `true`, a build without the `libkrun-sys` feature will still
/// return [`Error::NotYetWired`] from [`start`]. Treat this as a
/// precondition probe: if it returns `false`, point the user at
/// [`install_hint`].
pub fn is_available() -> bool {
    install_paths().iter().any(|p| Path::new(p).exists())
}

/// Human-readable install hint used in error messages and `mvmctl
/// doctor` output. Caller-platform-aware so users see the right
/// command for their OS.
pub const fn install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Install via Homebrew: `brew install libkrun`."
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "Install via your distro package manager (e.g. `apt install libkrun-dev` \
         on Debian/Ubuntu, `dnf install libkrun-devel` on Fedora) or build from \
         source: https://github.com/containers/libkrun"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "Install via your distro package manager or build from source: \
         https://github.com/containers/libkrun"
    }
    #[cfg(target_os = "windows")]
    {
        "libkrun is not supported on Windows. Use --hypervisor docker \
         or install WSL2 and run mvm inside a Linux distro."
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "libkrun is not supported on this platform."
    }
}

/// Standard filesystem locations checked by [`is_available`]. Order is
/// "most likely first" so the predicate short-circuits on the typical
/// developer install.
pub fn install_paths() -> Vec<&'static str> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/opt/homebrew/lib/libkrun.dylib", // Apple Silicon Homebrew
            "/usr/local/lib/libkrun.dylib",    // Intel Homebrew + manual installs
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            "/usr/lib/x86_64-linux-gnu/libkrun.so",
            "/usr/lib/aarch64-linux-gnu/libkrun.so",
            "/usr/lib64/libkrun.so",
            "/usr/local/lib/libkrun.so",
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Configuration for a libkrun guest VM.
///
/// Pure data — no I/O until [`start`] consumes it. Field shape is
/// stable across the W1 → W3 transition; the FFI calls that consume
/// each field live in [`sys`] under the `libkrun-sys` feature.
#[derive(Debug, Clone)]
pub struct KrunContext {
    pub name: String,
    pub kernel_path: String,
    pub rootfs_path: String,
    pub vcpus: u8,
    pub ram_mib: u32,
    pub kernel_cmdline: Option<String>,
    pub vsock_ports: Vec<u32>,
}

impl KrunContext {
    /// Construct a context for a guest. No I/O — this is pure
    /// configuration. The actual VM creation happens in [`start`].
    pub fn new(
        name: impl Into<String>,
        kernel_path: impl Into<String>,
        rootfs_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kernel_path: kernel_path.into(),
            rootfs_path: rootfs_path.into(),
            vcpus: 1,
            ram_mib: 256,
            kernel_cmdline: None,
            vsock_ports: Vec::new(),
        }
    }

    /// Set CPU and memory shape.
    pub fn with_resources(mut self, vcpus: u8, ram_mib: u32) -> Self {
        self.vcpus = vcpus;
        self.ram_mib = ram_mib;
        self
    }

    /// Append a vsock port the guest agent will listen on.
    pub fn add_vsock_port(mut self, port: u32) -> Self {
        self.vsock_ports.push(port);
        self
    }
}

/// Start a libkrun guest from `ctx`.
///
/// **Plan 57 W1 scope.** With the `libkrun-sys` feature enabled, this
/// allocates a libkrun configuration context, applies CPU/memory,
/// kernel, rootfs, and vsock-port configuration through the FFI, then
/// frees the context and returns `Ok(())`. It does **not** call
/// `krun_start_enter` (which blocks until the guest exits) — the
/// blocking-thread lifecycle and state-tracking work is W3 + W4 of
/// plan 57. Today the call exists so consumers can exercise the
/// wrapper end-to-end on a host with libkrun installed; the W3 PR
/// upgrades it to actually boot.
///
/// Without the feature, returns [`Error::NotYetWired`].
pub fn start(ctx: &KrunContext) -> Result<(), Error> {
    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }
    #[cfg(not(feature = "libkrun-sys"))]
    {
        let _ = ctx;
        Err(Error::NotYetWired {
            tracking: "specs/plans/57-libkrun-spike.md W3+W4",
        })
    }
    #[cfg(feature = "libkrun-sys")]
    {
        start_via_ffi(ctx)
    }
}

#[cfg(feature = "libkrun-sys")]
fn start_via_ffi(ctx: &KrunContext) -> Result<(), Error> {
    let krun = sys::Context::new()?;
    krun.set_vm_config(ctx.vcpus, ctx.ram_mib)?;
    krun.set_kernel(
        Path::new(&ctx.kernel_path),
        sys::KernelFormat::Elf,
        None,
        ctx.kernel_cmdline.as_deref(),
    )?;
    krun.add_disk("root", Path::new(&ctx.rootfs_path), false)?;
    for &port in &ctx.vsock_ports {
        // libkrun's `krun_add_vsock_port` requires a host-side Unix
        // socket path. The full backend wiring (which generates that
        // path under ~/.mvm/vms/<name>/) lands in W3 alongside the
        // boot validation. For W1 the FFI exercise stops short of
        // booting, so a stub path is sufficient to confirm the wrapper
        // accepts the call.
        let socket = format!("/tmp/mvm-libkrun-{}-vsock-{port}.sock", ctx.name);
        krun.add_vsock_port(port, Path::new(&socket))?;
    }
    // `krun_start_enter` deliberately not invoked — that's W3 + W4.
    // Dropping the context here frees it cleanly through `Context::Drop`.
    Ok(())
}

/// Stop a running libkrun guest by name.
///
/// **Plan 57 W1 scope.** The blocking-thread + registry lifecycle that
/// would let us find and signal a running VM is W4 of plan 57. Today
/// this returns [`Error::NotYetWired`] regardless of feature — there
/// is no running state to stop yet.
pub fn stop(name: &str) -> Result<(), Error> {
    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }
    let _ = name;
    Err(Error::NotYetWired {
        tracking: "specs/plans/57-libkrun-spike.md W4",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_paths_are_platform_specific() {
        let paths = install_paths();
        #[cfg(target_os = "macos")]
        assert!(paths.iter().any(|p| p.ends_with(".dylib")));
        #[cfg(target_os = "linux")]
        assert!(paths.iter().any(|p| p.ends_with(".so")));
        #[cfg(target_os = "windows")]
        assert!(paths.is_empty());
    }

    #[test]
    fn install_hint_is_non_empty() {
        // All platforms produce *some* hint, even Windows ("not supported").
        assert!(!install_hint().is_empty());
    }

    #[test]
    fn krun_context_builds_without_io() {
        let ctx = KrunContext::new("vm-1", "/path/vmlinux", "/path/rootfs.ext4")
            .with_resources(2, 512)
            .add_vsock_port(5252);
        assert_eq!(ctx.name, "vm-1");
        assert_eq!(ctx.vcpus, 2);
        assert_eq!(ctx.ram_mib, 512);
        assert_eq!(ctx.vsock_ports, vec![5252]);
    }

    #[test]
    fn error_display_messages_are_actionable() {
        let not_installed = Error::NotInstalled {
            install_hint: "brew install libkrun",
        };
        let not_wired = Error::NotYetWired {
            tracking: "plan 57",
        };
        let krun_err = Error::Krun(-22);
        let invalid = Error::InvalidCString;
        // Each variant produces a non-empty, distinct message that
        // names what to do next.
        assert!(format!("{not_installed}").contains("brew install"));
        assert!(format!("{not_wired}").contains("plan 57"));
        assert!(format!("{krun_err}").contains("-22"));
        assert!(format!("{invalid}").contains("NUL"));
    }

    /// When libkrun isn't installed on the host, `start` short-circuits
    /// before touching the FFI — works the same way with or without
    /// the `libkrun-sys` feature.
    #[test]
    fn start_errors_when_not_installed() {
        if is_available() {
            // Host has libkrun; this test exercises the fast-fail path,
            // not the FFI surface, so skip.
            return;
        }
        let ctx = KrunContext::new("vm", "/k", "/r");
        let err = start(&ctx).expect_err("scaffolding errors without libkrun");
        assert!(matches!(err, Error::NotInstalled { .. }));
    }
}
