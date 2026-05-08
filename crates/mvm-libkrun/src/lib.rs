//! libkrun backend bindings for mvm.
//!
//! libkrun is Red Hat's library-style VMM (Apache-2.0). Unlike Firecracker
//! (separate binary, HTTP API) or Apple Virtualization.framework (Swift /
//! objc2 FFI), libkrun is a C library you link directly into your binary.
//! On Linux it uses KVM; on macOS it uses Hypervisor.framework — making it
//! the only VMM in mvm's consideration set that runs on **both** macOS
//! Apple Silicon and macOS Intel without Lima. On Linux it gives us a
//! single-binary alternative to the firecracker-on-PATH dependency.
//!
//! See plan 53 §"Plan E" for design context and the fork test that
//! qualified libkrun as the one new backend we add in Sprint 48.
//!
//! # Status (Sprint 48 lane-laying)
//!
//! This crate ships **scaffolding** with the final public API shape:
//! [`KrunContext`] for construction, [`start`] / [`stop`] for lifecycle,
//! [`is_available`] for runtime detection. The implementation is gated
//! on a real libkrun install — see `specs/plans/57-libkrun-spike.md`
//! for the work that lands the bindings, codesigning, and end-to-end
//! boot validation.
//!
//! Until then, [`start`] / [`stop`] return [`Error::NotYetWired`] with a
//! pointer to plan 57, and [`is_available`] checks the host for a
//! libkrun shared library at standard install locations (Homebrew on
//! macOS, distro packages on Linux).
//!
//! # Platform support
//!
//! - **macOS Apple Silicon**: libkrun ships in Homebrew as `libkrun`.
//! - **macOS Intel**: libkrun via Hypervisor.framework also works.
//! - **Linux x86_64 / aarch64**: libkrun is in most distro repos, or
//!   build from source via Nix.
//! - **Windows**: not supported. libkrun has no Windows port.

use std::path::Path;

/// Errors returned by this crate. `NotYetWired` is the placeholder
/// until the Plan E spike lands; once the bindings are real, this
/// variant goes away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// libkrun is not installed on the host (no shared library found at
    /// any of the standard locations checked by [`is_available`]).
    NotInstalled {
        /// Suggested install command for the user.
        install_hint: &'static str,
    },
    /// libkrun is installed but this crate's bindings haven't landed
    /// yet (Plan E spike pending). Returned by [`start`] / [`stop`]
    /// in Sprint 48 scaffolding.
    NotYetWired {
        /// Tracking issue / plan reference.
        tracking: &'static str,
    },
    /// Generic libkrun call failure — populated once bindings are real.
    Krun(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled { install_hint } => {
                write!(f, "libkrun is not installed on this host. {install_hint}")
            }
            Self::NotYetWired { tracking } => write!(
                f,
                "libkrun bindings are not yet wired up (tracking: {tracking}). \
                 The Plan E spike phase has to land before this backend can \
                 boot guests."
            ),
            Self::Krun(msg) => write!(f, "libkrun call failed: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Detect whether libkrun is installed on the host by probing for the
/// shared library at the standard install locations.
///
/// **Not the same as "is functional"** — even if `is_available()`
/// returns `true`, the [`start`] call may still fail with
/// [`Error::NotYetWired`] until the libkrun spike (specs/plans/
/// 57-libkrun-spike.md) lands the real bindings. Treat this as a
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
/// Final API shape — the spike phase will populate the inner
/// representation but callers won't have to change.
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

/// Start a libkrun guest from `ctx`. Currently a stub — see
/// [`Error::NotYetWired`].
pub fn start(ctx: &KrunContext) -> Result<(), Error> {
    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }
    let _ = ctx; // silence unused-arg until bindings land
    Err(Error::NotYetWired {
        tracking: "specs/plans/57-libkrun-spike.md",
    })
}

/// Stop a running libkrun guest by name. Stub for the Sprint 48
/// scaffolding — see [`Error::NotYetWired`].
pub fn stop(name: &str) -> Result<(), Error> {
    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }
    let _ = name;
    Err(Error::NotYetWired {
        tracking: "specs/plans/57-libkrun-spike.md",
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
    fn start_errors_when_not_installed_or_not_yet_wired() {
        let ctx = KrunContext::new("vm", "/k", "/r");
        let err = start(&ctx).expect_err("scaffolding always errors");
        // Either "not installed" (most common in CI) or "not yet wired"
        // (when libkrun *is* installed on the dev's host) — both are
        // acceptable surfaces for the scaffolding phase.
        assert!(matches!(
            err,
            Error::NotInstalled { .. } | Error::NotYetWired { .. }
        ));
    }

    #[test]
    fn error_display_messages_are_actionable() {
        let not_installed = Error::NotInstalled {
            install_hint: "brew install libkrun",
        };
        let not_wired = Error::NotYetWired {
            tracking: "plan 53",
        };
        let krun_err = Error::Krun("kvm_create failed".to_string());
        // Each variant produces a non-empty, distinct message that
        // names what to do next.
        assert!(format!("{not_installed}").contains("brew install"));
        assert!(format!("{not_wired}").contains("plan 53"));
        assert!(format!("{krun_err}").contains("kvm_create"));
    }
}
