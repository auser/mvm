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
pub use sys::{KernelFormat, LogLevel, set_log_level};

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
    /// Filesystem I/O failure while setting up the supervisor's per-VM
    /// state directory or PID file. Carries a free-form context
    /// string rather than the raw `io::Error` so the `PartialEq`/`Eq`
    /// derives on `Error` keep working.
    Io {
        /// Operation + path + underlying message, formatted by the
        /// caller. E.g. `create_dir_all /Users/x/.mvm/vms/foo: permission denied`.
        context: String,
    },
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
            Self::Io { context } => write!(f, "supervisor I/O error: {context}"),
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

/// Extra block device to attach to the guest alongside the rootfs.
///
/// libkrun mounts the rootfs at `/dev/vda` (by convention from the
/// order disks are added); each `KrunDisk` becomes `/dev/vdb`,
/// `/dev/vdc`, … in the order they appear in [`KrunContext::extra_disks`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KrunDisk {
    /// Symbolic identifier passed to `krun_add_disk` (`block_id`).
    /// Not user-visible inside the guest; libkrun uses it for
    /// bookkeeping.
    pub id: String,
    /// Host path to the backing image (raw format).
    pub path: String,
    /// `true` opens the device read-only at the libkrun layer. Useful
    /// for dm-verity sidecars and signed root images.
    pub read_only: bool,
}

/// Configuration for a libkrun guest VM.
///
/// Pure data — no I/O until [`start`] / [`start_enter`] consume it.
/// Field shape is stable across the W1 → W3 transition; the FFI calls
/// that consume each field live in [`sys`] under the `libkrun-sys`
/// feature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KrunContext {
    pub name: String,
    pub kernel_path: String,
    pub rootfs_path: String,
    pub vcpus: u8,
    pub ram_mib: u32,
    pub kernel_cmdline: Option<String>,
    pub vsock_ports: Vec<u32>,
    /// Additional virtio-blk devices, appearing as `/dev/vdb`,
    /// `/dev/vdc`, … in the order listed. Empty by default; the
    /// dev-VM builder VM uses one entry for the Nix-store overlay
    /// disk (`MVM_NIX_STORE_DISK`).
    pub extra_disks: Vec<KrunDisk>,
    /// When `Some`, libkrun routes the guest's hvc0 console to this
    /// host path (a regular file, FIFO, or device). When `None`, the
    /// console writes inherit the calling process's stdout (the
    /// default for an interactive smoke test). Plan 57 W3 uses this
    /// to capture early-boot kernel output for diagnosis.
    pub console_output_path: Option<String>,
    /// Directory on the host where the per-vsock-port Unix socket
    /// files live. libkrun proxies between each guest-side
    /// `AF_VSOCK` port (TSI / virtio-vsock) and the corresponding
    /// `vsock-<port>.sock` inside this directory; a sibling host
    /// process (e.g. `mvmctl console <vm>` or the guest-agent
    /// vsock client in mvm-supervisor) speaks to the guest by
    /// opening the unix socket. When `None`, [`vsock_socket_path`]
    /// falls back to `/tmp/mvm-libkrun-<name>-vsock-<port>.sock`
    /// — fine for the spike smoke binary, but real consumers
    /// (Plan 57 W4 supervisor, Plan 72 builder-VM launcher) should
    /// always set a stable per-VM dir under `~/.mvm/vms/<name>/`
    /// so cross-process clients can find it.
    pub vsock_socket_dir: Option<String>,
}

impl KrunContext {
    /// Construct a context for a guest. No I/O — this is pure
    /// configuration. The actual VM creation happens in [`start`] or
    /// [`start_enter`].
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
            extra_disks: Vec::new(),
            console_output_path: None,
            vsock_socket_dir: None,
        }
    }

    /// Resolve the host-side unix socket path libkrun should pair
    /// with `port`. If [`Self::vsock_socket_dir`] is set, returns
    /// `<dir>/vsock-<port>.sock`; otherwise falls back to a per-VM
    /// `/tmp` path scoped by [`Self::name`]. The fallback is fine
    /// for the spike smoke binary but real consumers should always
    /// supply an explicit dir (see field docs).
    pub fn vsock_socket_path(&self, port: u32) -> std::path::PathBuf {
        match &self.vsock_socket_dir {
            Some(dir) => std::path::PathBuf::from(dir).join(format!("vsock-{port}.sock")),
            None => std::path::PathBuf::from(format!(
                "/tmp/mvm-libkrun-{}-vsock-{port}.sock",
                self.name
            )),
        }
    }

    /// Set CPU and memory shape.
    pub fn with_resources(mut self, vcpus: u8, ram_mib: u32) -> Self {
        self.vcpus = vcpus;
        self.ram_mib = ram_mib;
        self
    }

    /// Set the kernel command line (replaces the default).
    pub fn with_cmdline(mut self, cmdline: impl Into<String>) -> Self {
        self.kernel_cmdline = Some(cmdline.into());
        self
    }

    /// Append a vsock port the guest agent will listen on.
    pub fn add_vsock_port(mut self, port: u32) -> Self {
        self.vsock_ports.push(port);
        self
    }

    /// Attach an additional virtio-blk device. The first call appears
    /// as `/dev/vdb` in the guest, the second `/dev/vdc`, etc.
    pub fn add_disk(
        mut self,
        id: impl Into<String>,
        path: impl Into<String>,
        read_only: bool,
    ) -> Self {
        self.extra_disks.push(KrunDisk {
            id: id.into(),
            path: path.into(),
            read_only,
        });
        self
    }

    /// Route the guest's hvc0 console output to `path`. Pass `None`
    /// (or omit the call) to leave libkrun's default behavior in
    /// place (writes to the calling process's stdout).
    pub fn with_console_output(mut self, path: impl Into<String>) -> Self {
        self.console_output_path = Some(path.into());
        self
    }

    /// Set the per-VM directory libkrun should place vsock unix
    /// socket files under. See [`Self::vsock_socket_dir`] for the
    /// rationale; this builder method ensures consumers can chain it
    /// alongside the other resource setters.
    pub fn with_vsock_socket_dir(mut self, dir: impl Into<String>) -> Self {
        self.vsock_socket_dir = Some(dir.into());
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

/// Apply every `KrunContext` field to a freshly-allocated libkrun
/// configuration context. Shared between [`start`] (W1: configure +
/// drop) and [`start_enter`] (W3: configure + boot).
#[cfg(feature = "libkrun-sys")]
fn configure(ctx: &KrunContext) -> Result<sys::Context, Error> {
    let krun = sys::Context::new()?;
    krun.set_vm_config(ctx.vcpus, ctx.ram_mib)?;
    // ARM64 Linux kernels build as the "Image" format (a flat binary
    // header + payload, not ELF). libkrun's `RAW` kernel format consumes
    // them directly. x86_64 bzImage is also `RAW`. ELF-format kernels
    // (rare outside test fixtures) would need `KernelFormat::Elf`.
    krun.set_kernel(
        Path::new(&ctx.kernel_path),
        sys::KernelFormat::Raw,
        None,
        ctx.kernel_cmdline.as_deref(),
    )?;
    krun.add_disk("root", Path::new(&ctx.rootfs_path), false)?;
    for disk in &ctx.extra_disks {
        krun.add_disk(&disk.id, Path::new(&disk.path), disk.read_only)?;
    }
    // libkrun's vsock model — refined by plan 57 W4 after a closer
    // read of `libkrun.h`:
    //
    // - TSI (Transparent Socket Impersonation) is auto-enabled when
    //   no virtio-net device is added. The libkrun README is
    //   explicit: "TSI for AF_INET and AF_INET6 is automatically
    //   enabled when no network interface is added to the VM." We
    //   never call `krun_add_net_*`, so TSI is always on, and the
    //   virtio-vsock device exists implicitly.
    //
    // - `krun_add_vsock` is for *adding a second independent*
    //   virtio-vsock device, and requires `krun_disable_implicit_vsock`
    //   first. Because TSI already provides one, naively calling it
    //   returns `-EEXIST` (verified on libkrun 1.17.4). Do not use
    //   from mvm's path.
    //
    // - **Direction matters.** `krun_add_vsock_port` (no `_2`)
    //   documents "port that the guest will connect to for IPC" —
    //   guest is the client, host is the server, host binds the
    //   listener. `krun_add_vsock_port2(..., listen=true)` flips
    //   it: "guest expects connections to be initiated from host
    //   side" — guest is the server, libkrun creates the unix
    //   socket file as a listener, host processes connect as
    //   clients. mvm's guest agent always *listens* on
    //   `GUEST_AGENT_PORT`, so `listen=true` is the right mode for
    //   every vsock port we register here. The W3.3 PR used
    //   `add_vsock_port` (no `_2`); that was the wrong direction
    //   but happened to boot because nothing in the guest actually
    //   tried to use vsock. W4 corrects it.
    for &port in &ctx.vsock_ports {
        let socket = ctx.vsock_socket_path(port);
        krun.add_vsock_port2(port, &socket, /* listen = */ true)?;
    }
    if let Some(console_path) = &ctx.console_output_path {
        krun.set_console_output(Path::new(console_path))?;
    }
    Ok(krun)
}

#[cfg(feature = "libkrun-sys")]
fn start_via_ffi(ctx: &KrunContext) -> Result<(), Error> {
    let _krun = configure(ctx)?;
    // `krun_start_enter` deliberately not invoked here — that's
    // [`start_enter`]. Dropping the context frees it cleanly through
    // `Context::Drop`.
    Ok(())
}

/// Boot a libkrun guest from `ctx` and block until it exits.
///
/// **Plan 57 W3 spike entry point.** Configures libkrun the same way
/// [`start`] does, then calls `krun_start_enter`. libkrun's
/// `start_enter` calls `exit()` on the calling process with the
/// guest's exit code when the guest powers off cleanly, so this
/// function does not return on success — its return type is
/// [`std::convert::Infallible`] in the `Ok` arm.
///
/// Use cases:
/// - the W3 smoke binary (`crates/mvm-libkrun/examples/libkrun-smoke.rs`)
///   that validates a real Nix-built kernel + ext4 rootfs boots on
///   macOS Apple Silicon;
/// - one-shot guest invocations where the caller wants the process
///   to exit alongside the guest.
///
/// **Not yet suitable** for `LibkrunBackend::start()` — that consumer
/// needs the surrounding mvmctl process to keep running after the VM
/// boots. The blocking-thread + per-VM registry lifecycle is W4 of
/// plan 57.
///
/// Without the `libkrun-sys` feature, returns [`Error::NotYetWired`].
pub fn start_enter(ctx: &KrunContext) -> Result<std::convert::Infallible, Error> {
    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }
    #[cfg(not(feature = "libkrun-sys"))]
    {
        let _ = ctx;
        Err(Error::NotYetWired {
            tracking: "specs/plans/57-libkrun-spike.md W3",
        })
    }
    #[cfg(feature = "libkrun-sys")]
    {
        let krun = configure(ctx)?;
        krun.start_enter()
    }
}

/// Configuration consumed by [`run_supervisor`] — the JSON shape that
/// the `mvm-libkrun-supervisor` binary reads from stdin and that
/// `LibkrunBackend::start()` produces. Holds the [`KrunContext`] plus
/// supervisor-process bookkeeping fields that don't belong on the
/// pure-FFI config type.
///
/// Always available (not feature-gated) because `mvm-backend`'s
/// `LibkrunBackend::start()` constructs it and serializes to JSON
/// without ever turning on `libkrun-sys` itself — the supervisor
/// process on the other end of the pipe is the one that links libkrun.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SupervisorConfig {
    /// The guest configuration to hand to libkrun.
    pub krun: KrunContext,
    /// Directory under which the supervisor writes its PID file and
    /// (via `KrunContext::vsock_socket_dir`) the per-port vsock
    /// listener sockets. Typically `~/.mvm/vms/<name>/`. Created if
    /// absent.
    pub vm_state_dir: String,
    /// File name inside `vm_state_dir` to receive the supervisor's
    /// PID. Defaults to `"libkrun.pid"` when `None`. Plan 72's
    /// builder VM uses a different name (`builder.pid`) so the user
    /// dev VM and the builder can coexist in the same directory tree.
    pub pid_file_name: Option<String>,
}

impl SupervisorConfig {
    /// Resolve the absolute path to the PID file
    /// (`<vm_state_dir>/<pid_file_name>`). Used by the supervisor to
    /// write its PID and by `LibkrunBackend` to read it for
    /// `stop`/`status`/`list`.
    pub fn pid_file(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.vm_state_dir)
            .join(self.pid_file_name.as_deref().unwrap_or("libkrun.pid"))
    }
}

/// Run a libkrun guest under a long-lived supervisor process.
///
/// **Plan 57 W4 entry point.** Each call owns exactly one libkrun
/// guest for the lifetime of the calling process. Steps:
///
/// 1. Create `vm_state_dir` if absent.
/// 2. Write the calling process's PID to `<vm_state_dir>/<pid_file_name>`.
/// 3. Call [`start_enter`], which configures libkrun (including the
///    `add_vsock_port2(listen=true)` host-listener registration so
///    libkrun creates the unix socket file as a server) and then
///    blocks the calling thread in `krun_start_enter`.
///
/// `krun_start_enter` calls `exit()` on the supervisor when the
/// guest powers off cleanly. That's the point of running one process
/// per VM — only this supervisor terminates; the parent `mvmctl` and
/// any sibling supervisors are unaffected.
///
/// Without the `libkrun-sys` feature, returns [`Error::NotYetWired`].
#[cfg(feature = "libkrun-sys")]
pub fn run_supervisor(cfg: &SupervisorConfig) -> Result<std::convert::Infallible, Error> {
    std::fs::create_dir_all(&cfg.vm_state_dir).map_err(|e| Error::Io {
        context: format!("create_dir_all {}: {e}", cfg.vm_state_dir),
    })?;
    let pid_path = cfg.pid_file();
    let pid = std::process::id().to_string();
    std::fs::write(&pid_path, &pid).map_err(|e| Error::Io {
        context: format!("write pid file {}: {e}", pid_path.display()),
    })?;
    start_enter(&cfg.krun)
}

/// Non-FFI-feature stub so callers can reference the function name
/// regardless of build configuration. Returns [`Error::NotYetWired`].
#[cfg(not(feature = "libkrun-sys"))]
pub fn run_supervisor_unavailable() -> Error {
    Error::NotYetWired {
        tracking: "specs/plans/57-libkrun-spike.md W4 — rebuild with --features libkrun-sys",
    }
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

    /// Plan 57 W3.3: the per-VM `vsock_socket_dir` overrides the
    /// `/tmp` fallback. Real consumers (W4 supervisor, Plan 72
    /// builder-VM launcher) always supply a dir under
    /// `~/.mvm/vms/<name>/` so cross-process clients can find it
    /// without scanning `/tmp`.
    #[test]
    fn vsock_socket_path_falls_back_to_tmp_when_no_dir_set() {
        let ctx = KrunContext::new("vm-1", "/k", "/r");
        let path = ctx.vsock_socket_path(5252);
        assert_eq!(
            path,
            std::path::PathBuf::from("/tmp/mvm-libkrun-vm-1-vsock-5252.sock")
        );
    }

    #[test]
    fn vsock_socket_path_uses_configured_dir_when_set() {
        let ctx =
            KrunContext::new("vm-1", "/k", "/r").with_vsock_socket_dir("/home/user/.mvm/vms/vm-1");
        let path = ctx.vsock_socket_path(5252);
        assert_eq!(
            path,
            std::path::PathBuf::from("/home/user/.mvm/vms/vm-1/vsock-5252.sock")
        );
    }

    /// Multiple ports share one dir; libkrun proxies each
    /// independently. The name → port pairing keeps cross-VM
    /// concurrency on a single host from clashing.
    #[test]
    fn vsock_socket_path_distinguishes_ports() {
        let ctx = KrunContext::new("vm-1", "/k", "/r")
            .with_vsock_socket_dir("/d")
            .add_vsock_port(5252)
            .add_vsock_port(5253);
        let a = ctx.vsock_socket_path(5252);
        let b = ctx.vsock_socket_path(5253);
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().contains("5252"));
        assert!(b.file_name().unwrap().to_string_lossy().contains("5253"));
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
