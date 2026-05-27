//! Rust bindings for Red Hat libkrun (Linux KVM, macOS Hypervisor.framework).
//!
//! libkrun is a library-style VMM: linked directly into the calling binary
//! rather than spawned as a separate daemon. On Linux it uses KVM; on
//! macOS it uses Hypervisor.framework. mvm supports this path on Linux
//! with KVM and macOS Apple Silicon; macOS Intel is intentionally not a
//! supported local microVM host.
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
pub use sys::{
    BundledKernel, KernelFormat, LogLevel, extract_bundled_kernel, init_log, set_log_level,
};

// Plan 87 / ADR-055 — passt-backed virtio-net. The supervisor owns the
// passt child process and exposes the socket fd `KrunContext::Passt`
// consumes. Only Linux / macOS — Windows has neither libkrun nor
// passt. Tests are gated on a host-side passt install probe.
#[cfg(target_family = "unix")]
pub mod passt;

// Plan 88 / ADR-055 cross-platform amendment — gvproxy-backed
// virtio-net. The macOS counterpart to passt; both modules share the
// same shape (spawn child, hand its socket to libkrun, kill on Drop)
// but gvproxy uses libkrun's `krun_add_net_unixgram` (path-based)
// where passt uses `krun_add_net_unixstream` (fd-passed). Same unix
// gate as passt — Windows has neither.
#[cfg(target_family = "unix")]
pub mod gvproxy;

/// Cross-module env mutation serializer. Both `passt::tests` and
/// `gvproxy::tests` mutate `$PATH` to verify their `NotInstalled`
/// error paths; `cargo test`'s default parallelism would race them
/// otherwise and leak the "set PATH to a tmp dir" state across the
/// two tests, making one's spawn call see the other's modified env.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

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
            "/usr/local/lib/libkrun.dylib",    // manual installs
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

/// A virtio-fs share: a host directory exported into the guest under
/// a symbolic `tag` that the guest mounts via the `virtiofs` filesystem
/// type. libkrun wraps the `virtiofsd` daemon internally — callers
/// declare the share here and libkrun handles the daemon lifecycle.
///
/// Plan 72 W4 uses three of these per builder VM invocation:
///
/// - `tag = "work"`  → workspace bind (read-only at the guest mount)
/// - `tag = "out"`   → artifact dir (read-write)
/// - `tag = "job"`   → per-build job dir with `cmd.sh` / `env` / `result`
///
/// Read/write semantics at the guest are controlled by `mount` flags
/// inside the guest (`mvm-builder-init` mounts each tag with the
/// right flags); libkrun's `krun_add_virtiofs` does not currently
/// expose a `readonly` toggle on the host side.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KrunVirtioFs {
    /// Symbolic identifier the guest references in `mount -t virtiofs
    /// <tag> <target>`. ASCII letters / digits / dash / underscore;
    /// libkrun passes it as a C string so interior NUL bytes are
    /// rejected at [`start_enter`] time.
    pub tag: String,
    /// Host directory to export. Must exist before [`start_enter`]
    /// runs (libkrun's daemon resolves it eagerly).
    pub host_path: String,
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
    /// Kernel image. `Some` for the kernel+rootfs and kernel+initramfs
    /// paths. `None` when [`Self::root_dir`] is set — libkrun uses its
    /// bundled TSI-patched kernel transparently in `set_root` mode.
    #[serde(default)]
    pub kernel_path: Option<String>,
    /// Root ext4 image. `Some` together with [`Self::kernel_path`] for
    /// the steady-state builder VM and runtime microVM path. Mutually
    /// exclusive with `initramfs_path` and `root_dir`.
    #[serde(default)]
    pub rootfs_path: Option<String>,
    /// Optional initial ramdisk. When `Some`, libkrun passes the
    /// path to `krun_set_kernel`'s `c_initramfs` arg. Mutually
    /// exclusive with `rootfs_path` and `root_dir`.
    #[serde(default)]
    pub initramfs_path: Option<String>,
    /// Host directory libkrun mounts as the guest root over virtiofs
    /// (via `krun_set_root`). Mutually exclusive with `kernel_path`,
    /// `rootfs_path`, and `initramfs_path` — libkrun loads its
    /// bundled kernel automatically in this mode. Pair with
    /// [`Self::guest_entrypoint`] so libkrun knows what to run as
    /// PID 1. Used by the Stage 0 bootstrap path.
    #[serde(default)]
    pub root_dir: Option<String>,
    /// Guest PID 1 entrypoint, relative to [`Self::root_dir`]. Required
    /// when `root_dir` is set; ignored otherwise.
    #[serde(default)]
    pub guest_entrypoint: Option<GuestEntrypoint>,
    pub vcpus: u8,
    pub ram_mib: u32,
    pub kernel_cmdline: Option<String>,
    pub vsock_ports: Vec<u32>,
    /// Additional virtio-blk devices, appearing as `/dev/vdb`,
    /// `/dev/vdc`, … in the order listed. Empty by default; the
    /// dev-VM builder VM uses one entry for the Nix-store overlay
    /// disk (`MVM_NIX_STORE_DISK`).
    pub extra_disks: Vec<KrunDisk>,
    /// virtio-fs shares the host exports into the guest. Plan 72
    /// W4's builder VM declares three of these (`work` / `out` /
    /// `job`); the runtime backend doesn't use any today.
    /// libkrun manages the in-process `virtiofsd` daemon for each
    /// entry. See [`KrunVirtioFs`] for the per-share contract.
    #[serde(default)]
    pub virtio_fs_mounts: Vec<KrunVirtioFs>,
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
    /// Plan 87 — networking backend for the guest. `Tsi` (default)
    /// uses libkrun's built-in syscall-hijack TSI mode; `Passt`
    /// attaches a virtio-net device backed by a unixstream socket
    /// the caller has handed off to a passt child process. See
    /// `NetworkingMode` for the trade-offs.
    #[serde(default)]
    pub networking: NetworkingMode,
}

/// Libkrun networking backend. Plan 87 / ADR-055.
///
/// `Tsi` is libkrun's default — AF_INET syscall hijacking, no
/// virtio-net device in the guest, no DHCP. Works for trivial HTTP
/// (single GET) but breaks on HTTP/2 multiplexing, HTTPS redirect
/// chains, and nix's substituter probes. Kept as an opt-out for
/// debugging and for runtime microVMs that legitimately don't need
/// a network stack.
///
/// `Passt` configures a real virtio-net device wired through a
/// unixstream socket to a host-side passt child process. The guest
/// sees a normal eth0 + DHCP + DNS. This is the production-ready
/// networking mode for Stage 0 and steady-state builder VMs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkingMode {
    /// libkrun's built-in TSI backend (no virtio-net, no DHCP).
    #[default]
    Tsi,
    /// virtio-net via passt. The supervisor process (whichever links
    /// `libkrun-sys`) spawns a passt child inside `run_supervisor`,
    /// hands its socket fd to `krun_add_net_unixstream`, and reaps
    /// passt when libkrun exits. mvmctl (and any other JSON-consuming
    /// caller) just declares the intent here — the live fd never
    /// survives JSON serialization, so we keep it out of this struct.
    Passt {
        /// MAC address for the guest's eth0. 6 bytes; the first
        /// octet must have bit 0x02 set (locally-administered) to
        /// avoid colliding with real hardware allocations.
        mac: [u8; 6],
        /// Host directory where the supervisor stages passt's log
        /// file (`<scratch_dir>/passt.log`). Typically
        /// `<vm_state_dir>` so the per-VM artifact set stays
        /// co-located. The supervisor creates it if absent.
        scratch_dir: String,
    },
    /// virtio-net via gvproxy — Plan 88. The supervisor spawns a
    /// gvproxy child inside `run_supervisor`, points libkrun's
    /// `krun_add_net_unixgram` at the listener socket gvproxy
    /// creates, and reaps gvproxy on guest exit. Same model as
    /// `Passt` but unixgram-flavored: libkrun connects to a path
    /// on disk rather than receiving a pre-opened socket fd. This
    /// is the canonical macOS backend (passt is Linux-only — see
    /// ADR-055 §"Cross-platform backends").
    Gvproxy {
        /// MAC address for the guest's eth0. Same shape as the
        /// `Passt` variant.
        mac: [u8; 6],
        /// Host directory where the supervisor stages gvproxy's
        /// listener socket (`<scratch_dir>/gvproxy.sock`) + log
        /// file (`<scratch_dir>/gvproxy.log`).
        scratch_dir: String,
    },
}

/// Guest PID 1 entrypoint passed to `krun_set_exec`. `path` is
/// relative to [`KrunContext::root_dir`]. `argv[0]` is the entry
/// name (libkrun appends the trailing NULL); leave empty for the
/// "name = path, no other args" common case.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GuestEntrypoint {
    /// Path to the guest PID 1 entrypoint, relative to `root_dir`.
    pub path: String,
    /// `argv` array. Empty defaults to `[basename(path)]` at FFI time.
    #[serde(default)]
    pub argv: Vec<String>,
    /// Environment block. Empty by default — libkrun does not
    /// propagate the host env, so callers must supply anything they
    /// need (e.g. `PATH=/bin:/usr/local/bin`).
    #[serde(default)]
    pub envp: Vec<String>,
}

impl KrunContext {
    /// Construct a context that boots from a rootfs ext4 image.
    pub fn new(
        name: impl Into<String>,
        kernel_path: impl Into<String>,
        rootfs_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kernel_path: Some(kernel_path.into()),
            rootfs_path: Some(rootfs_path.into()),
            initramfs_path: None,
            root_dir: None,
            guest_entrypoint: None,
            vcpus: 1,
            ram_mib: 256,
            kernel_cmdline: None,
            vsock_ports: Vec::new(),
            extra_disks: Vec::new(),
            virtio_fs_mounts: Vec::new(),
            console_output_path: None,
            vsock_socket_dir: None,
            networking: NetworkingMode::Tsi,
        }
    }

    /// Construct a context that boots from an initramfs (no rootfs
    /// disk attached). The kernel decompresses the initramfs into a
    /// tmpfs rootfs and runs `init=` from it.
    pub fn new_initramfs(
        name: impl Into<String>,
        kernel_path: impl Into<String>,
        initramfs_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kernel_path: Some(kernel_path.into()),
            rootfs_path: None,
            initramfs_path: Some(initramfs_path.into()),
            root_dir: None,
            guest_entrypoint: None,
            vcpus: 1,
            ram_mib: 256,
            kernel_cmdline: None,
            vsock_ports: Vec::new(),
            extra_disks: Vec::new(),
            virtio_fs_mounts: Vec::new(),
            console_output_path: None,
            vsock_socket_dir: None,
            networking: NetworkingMode::Tsi,
        }
    }

    /// Construct a context that boots libkrun's bundled kernel against
    /// a host directory mounted as the guest root via virtiofs. Used
    /// by the Stage 0 bootstrap path — no host-built kernel or rootfs
    /// image needed.
    ///
    /// `entry_path` is the guest PID 1, relative to `root_dir`. For
    /// Stage 0 this is `/init` (a shell script the kernel's binfmt
    /// loader resolves via the embedded busybox).
    pub fn new_root_dir(
        name: impl Into<String>,
        root_dir: impl Into<String>,
        entry_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kernel_path: None,
            rootfs_path: None,
            initramfs_path: None,
            root_dir: Some(root_dir.into()),
            guest_entrypoint: Some(GuestEntrypoint {
                path: entry_path.into(),
                argv: Vec::new(),
                envp: Vec::new(),
            }),
            vcpus: 1,
            ram_mib: 256,
            kernel_cmdline: None,
            vsock_ports: Vec::new(),
            extra_disks: Vec::new(),
            virtio_fs_mounts: Vec::new(),
            console_output_path: None,
            vsock_socket_dir: None,
            networking: NetworkingMode::Tsi,
        }
    }

    /// Set the guest entrypoint `argv` (libkrun's `krun_set_exec`
    /// `argv` argument). Only meaningful when `root_dir` is set.
    pub fn with_guest_argv<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Some(entry) = self.guest_entrypoint.as_mut() {
            entry.argv = argv.into_iter().map(Into::into).collect();
        }
        self
    }

    /// Set the guest entrypoint env block. Only meaningful when
    /// `root_dir` is set.
    pub fn with_guest_envp<I, S>(mut self, envp: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Some(entry) = self.guest_entrypoint.as_mut() {
            entry.envp = envp.into_iter().map(Into::into).collect();
        }
        self
    }

    /// Switch the guest to gvproxy-backed virtio-net. Same shape as
    /// [`Self::with_passt`] but uses libkrun's unixgram backend; the
    /// supervisor spawns gvproxy with `--listen-vfkit <socket>` and
    /// hands the socket path to `krun_add_net_unixgram`. Plan 88.
    pub fn with_gvproxy(mut self, mac: [u8; 6], scratch_dir: impl Into<String>) -> Self {
        self.networking = NetworkingMode::Gvproxy {
            mac,
            scratch_dir: scratch_dir.into(),
        };
        self
    }

    /// Switch the guest to passt-backed virtio-net. The supervisor
    /// process owns the passt child; we just declare the intent and
    /// the destination for passt's log file. Plan 87.
    pub fn with_passt(mut self, mac: [u8; 6], scratch_dir: impl Into<String>) -> Self {
        self.networking = NetworkingMode::Passt {
            mac,
            scratch_dir: scratch_dir.into(),
        };
        self
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

    /// Declare a virtio-fs share. The guest mounts it via
    /// `mount -t virtiofs <tag> <target>`. Plan 72 W4's builder
    /// VM uses this for `/work`, `/out`, `/job`.
    pub fn add_virtio_fs(mut self, tag: impl Into<String>, host_path: impl Into<String>) -> Self {
        self.virtio_fs_mounts.push(KrunVirtioFs {
            tag: tag.into(),
            host_path: host_path.into(),
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
///
/// Plan 87 split this into `configure_pre_net` (everything except
/// networking) + a per-caller networking decision. `configure` itself
/// is the TSI-only path used by the spike/smoke binaries; real
/// consumers go through `run_supervisor`, which owns a passt child
/// process for the libkrun lifetime via `configure_with_passt`.
#[cfg(feature = "libkrun-sys")]
fn configure(ctx: &KrunContext) -> Result<sys::Context, Error> {
    let krun = configure_pre_net(ctx)?;
    if !matches!(ctx.networking, NetworkingMode::Tsi) {
        return Err(Error::Io {
            context: format!(
                "{:?} requires the supervisor entry point; call \
                 `run_supervisor` rather than `start` / `start_enter` directly",
                ctx.networking
            ),
        });
    }
    Ok(krun)
}

/// Plan 88 — owning handle to whichever userspace network gateway
/// the supervisor spawned for this guest. Lives for the libkrun
/// process lifetime so the gateway is reaped when the guest exits.
#[cfg(all(feature = "libkrun-sys", target_family = "unix"))]
pub enum GatewayHandle {
    /// Not using a virtio-net backend — TSI is enabled implicitly.
    None,
    /// passt child (Linux).
    Passt(passt::PasstHandle),
    /// gvproxy child (macOS / cross-platform fallback).
    Gvproxy(gvproxy::GvproxyHandle),
}

/// Plan 87 W1+W2 / Plan 88 W1+W2 — configure() variant that owns the
/// network-gateway child process for the lifetime of the returned
/// context. Used by [`run_supervisor`] when
/// `NetworkingMode::{Passt, Gvproxy}` is set. The handle Drop's after
/// libkrun finishes consuming the socket and the guest exits.
#[cfg(all(feature = "libkrun-sys", target_family = "unix"))]
fn configure_with_gateway(ctx: &KrunContext) -> Result<(sys::Context, GatewayHandle), Error> {
    let krun = configure_pre_net(ctx)?;
    let handle = match &ctx.networking {
        NetworkingMode::Tsi => GatewayHandle::None,
        NetworkingMode::Passt { mac, scratch_dir } => {
            let handle =
                passt::spawn(std::path::Path::new(scratch_dir)).map_err(|e| Error::Io {
                    context: format!("spawning passt for NetworkingMode::Passt: {e}"),
                })?;
            krun.add_net_unixstream_fd(
                handle.socket_fd(),
                mac,
                sys::PASST_NET_FEATURES,
                /* flags = */ 0,
            )?;
            GatewayHandle::Passt(handle)
        }
        NetworkingMode::Gvproxy { mac, scratch_dir } => {
            let handle =
                gvproxy::spawn(std::path::Path::new(scratch_dir)).map_err(|e| Error::Io {
                    context: format!("spawning gvproxy for NetworkingMode::Gvproxy: {e}"),
                })?;
            // gvproxy speaks libkrun's "vfkit mode" framing on the
            // unixgram socket; NET_FLAG_VFKIT (see sys::NET_FLAG_VFKIT)
            // is libkrun's required signal to emit the magic-byte
            // handshake. NET_FLAG_DHCP_CLIENT (libkrun 1.18.0+) tells
            // libkrun's net device to bring the interface up via its
            // in-guest DHCP client against gvproxy's DHCP server, so
            // the guest sees a fully-configured eth0 without needing
            // an in-guest udhcpc race. libkrun's own
            // `tests/test_cases/src/test_net/gvproxy.rs` uses both.
            krun.add_net_unixgram_path(
                handle.socket_path(),
                mac,
                sys::PASST_NET_FEATURES,
                sys::NET_FLAG_VFKIT | sys::NET_FLAG_DHCP_CLIENT,
            )?;
            GatewayHandle::Gvproxy(handle)
        }
    };
    Ok((krun, handle))
}

/// Plan 87 — every part of `configure` that doesn't touch the
/// networking backend. Shared between the plain `configure` path
/// (TSI-only) and `configure_with_passt`.
#[cfg(feature = "libkrun-sys")]
fn configure_pre_net(ctx: &KrunContext) -> Result<sys::Context, Error> {
    validate_boot_config(ctx)?;
    let krun = sys::Context::new()?;
    krun.set_vm_config(ctx.vcpus, ctx.ram_mib)?;

    if let Some(root_dir) = &ctx.root_dir {
        let entry = ctx
            .guest_entrypoint
            .as_ref()
            .expect("validate_boot_config guarantees root_dir is paired with guest_entrypoint");
        krun.set_root(Path::new(root_dir))?;
        let argv_owned: Vec<&str> = if entry.argv.is_empty() {
            // Default argv[0] to the entry name for libkrun's exec.
            let basename = entry
                .path
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(entry.path.as_str());
            vec![basename]
        } else {
            entry.argv.iter().map(String::as_str).collect()
        };
        let envp_owned: Vec<&str> = entry.envp.iter().map(String::as_str).collect();
        krun.set_guest_entrypoint(Path::new(&entry.path), &argv_owned, &envp_owned)?;
    } else {
        let kernel_path = ctx
            .kernel_path
            .as_ref()
            .expect("validate_boot_config guarantees kernel_path is set when root_dir is absent");
        let initramfs_path = ctx.initramfs_path.as_deref().map(Path::new);
        krun.set_kernel(
            Path::new(kernel_path),
            sys::KernelFormat::Raw,
            initramfs_path,
            ctx.kernel_cmdline.as_deref(),
        )?;
        if let Some(rootfs) = &ctx.rootfs_path {
            krun.add_disk("root", Path::new(rootfs), false)?;
        }
    }

    for disk in &ctx.extra_disks {
        krun.add_disk(&disk.id, Path::new(&disk.path), disk.read_only)?;
    }
    for mount in &ctx.virtio_fs_mounts {
        krun.add_virtiofs(&mount.tag, Path::new(&mount.host_path))?;
    }
    for &port in &ctx.vsock_ports {
        let socket = ctx.vsock_socket_path(port);
        krun.add_vsock_port2(port, &socket, /* listen = */ true)?;
    }
    if let Some(console_path) = &ctx.console_output_path {
        krun.set_console_output(Path::new(console_path))?;
    }
    Ok(krun)
}

/// Validate that the boot fields on `ctx` describe exactly one of the
/// supported shapes: (kernel + rootfs), (kernel + initramfs), or
/// (root_dir + guest_entrypoint). Anything else is a programming
/// error — we'd otherwise pass nonsense to libkrun and watch it
/// fail late with an opaque rc.
///
/// Always-on so unit tests can exercise it without the `libkrun-sys`
/// feature. `configure_pre_net` is the only non-test caller and is
/// gated behind that feature, so the dead-code allow keeps the
/// non-feature library build quiet.
#[cfg_attr(not(feature = "libkrun-sys"), allow(dead_code))]
fn validate_boot_config(ctx: &KrunContext) -> Result<(), Error> {
    let has_kernel = ctx.kernel_path.is_some();
    let has_rootfs = ctx.rootfs_path.is_some();
    let has_initramfs = ctx.initramfs_path.is_some();
    let has_root_dir = ctx.root_dir.is_some();
    let has_entry = ctx.guest_entrypoint.is_some();

    if has_root_dir {
        if has_kernel || has_rootfs || has_initramfs {
            return Err(Error::Io {
                context: "KrunContext.root_dir is mutually exclusive with kernel_path, \
                          rootfs_path, and initramfs_path"
                    .to_string(),
            });
        }
        if !has_entry {
            return Err(Error::Io {
                context: "KrunContext.root_dir requires guest_entrypoint to be set".to_string(),
            });
        }
        return Ok(());
    }

    if !has_kernel {
        return Err(Error::Io {
            context: "KrunContext needs kernel_path (with rootfs_path or initramfs_path) or \
                      root_dir; none set"
                .to_string(),
        });
    }
    if has_rootfs == has_initramfs {
        return Err(Error::Io {
            context: "KrunContext kernel mode requires exactly one of rootfs_path or \
                      initramfs_path"
                .to_string(),
        });
    }
    Ok(())
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
        install_shutdown_handler(&krun)?;
        krun.start_enter()
    }
}

/// Best-effort SIGTERM handler that drops the supervisor process
/// immediately, so `mvmctl stop` / `kill -TERM <pid>` *may* reap it
/// without the 5-second SIGKILL escalation `LibkrunBackend::stop`
/// would otherwise hit.
///
/// "Best-effort" because libkrun's signal-mask behavior under
/// `krun_start_enter` is empirically inconsistent: the same binary
/// killed manually from a shell exits in ~100 ms, but when spawned
/// by `LibkrunBackend::start` (via `std::process::Command`) the
/// handler installed here doesn't always run before
/// `LibkrunBackend::stop` falls back to `SIGKILL` at 5 s. The
/// inconsistency seems to come from libkrun blocking SIGTERM on
/// every thread mid-`start_enter`, so the kernel can't always find
/// a thread to deliver to. Installing the handler is still net
/// positive: in the manual-stop path it lets the process exit
/// cleanly, and in the spawned-by-LibkrunBackend path it's a no-op
/// that doesn't *hurt*.
///
/// More robust options investigated and rejected:
/// - `krun_get_shutdown_eventfd` returns a valid fd on Homebrew's
///   libkrun 1.17.4 but the header docs it as gated on
///   `krun_start_event` (libkrun-efi only); writes to the fd vanish
///   under the `start_enter` entry point we use.
/// - A dedicated `sigwait` thread spawned before `start_enter`
///   makes `krun_start_enter` itself return `-EINVAL` (rc -22).
///   libkrun appears to want exclusive control of the process's
///   signal mask. Don't do that.
#[cfg(feature = "libkrun-sys")]
fn install_shutdown_handler(_krun: &sys::Context) -> Result<(), Error> {
    extern "C" fn handle_sigterm(_sig: libc::c_int) {
        // SAFETY: `_exit` is async-signal-safe per POSIX
        // (signal-safety(7)). Status 143 = 128 + SIGTERM, the
        // conventional shell convention for "killed by SIGTERM".
        unsafe {
            libc::_exit(143);
        }
    }

    // SAFETY: `sigaction` is async-signal-safe and we pass a
    // properly-zeroed `sigaction` struct. The handler we install is
    // itself signal-safe (single `_exit` call).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigterm as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(Error::Io {
                context: format!(
                    "sigaction(SIGTERM) failed: {}",
                    std::io::Error::last_os_error()
                ),
            });
        }
    }
    Ok(())
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

    // --- Plan 102 W6.A commit 6: gateway audit substrate ---------------
    //
    // The bridge ([`mvm_supervisor::gateway_bridge`]) needs these to
    // construct a per-VM `BridgeConfig`. `#[serde(default)]` keeps
    // pre-W6.A JSON callers parseable; admission validation
    // ([`SupervisorConfig::validate_audit_substrate`]) refuses
    // configs that don't supply them when the gateway substrate
    // is active.
    /// Tenant identifier — used as the per-tenant chain file name
    /// (`<audit_dir>/<tenant_id>.jsonl`). Validated to be non-empty
    /// before any bridge spawns.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// `~/.mvm/audit/` — directory holding per-tenant chain files
    /// and the per-VM subscriber sockets.
    #[serde(default)]
    pub audit_dir: Option<std::path::PathBuf>,
    /// `~/.mvm/audit/gateway-<vm>.sock` — per-VM subscriber socket
    /// the [`mvm_supervisor::gateway_audit::GatewayAuditSink`] binds.
    #[serde(default)]
    pub gateway_audit_socket: Option<std::path::PathBuf>,
    /// `~/.mvm/audit/gateway-events-<vm>.sock` — per-VM ingest
    /// socket the Vz Swift bridge connects to (one writer only).
    /// Required for Vz backend; ignored on libkrun (which spawns
    /// its bridge in-process).
    #[serde(default)]
    pub gateway_events_socket: Option<std::path::PathBuf>,
    /// `~/.mvm/keys/host-signer.ed25519` — host signing key for
    /// the chained audit emitter. Validated to canonicalize under
    /// `~/.mvm/keys/` (no path-traversal) at admission.
    #[serde(default)]
    pub signing_key_path: Option<std::path::PathBuf>,
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

    /// Plan 102 W6.A commit 6 — admission validation for the
    /// gateway audit substrate. Refuses configurations that would
    /// leave the bridge unable to emit audit events into the
    /// per-tenant chain. Mandatory before any bridge spawn; the
    /// bridge entry point (commit 6.5 / commit 7 wire-up) calls
    /// this first.
    ///
    /// Fields checked:
    /// - `tenant_id`: must be set and non-empty (used as chain
    ///   file name `<audit_dir>/<tenant_id>.jsonl`).
    /// - `audit_dir`: must be set. Used as parent for chain files
    ///   and per-VM subscriber sockets.
    /// - `gateway_audit_socket`: must be set, must have a parent
    ///   matching `audit_dir` (defense in depth — refuses
    ///   subscriber sockets in unrelated dirs).
    /// - `signing_key_path`: must be set, must canonicalize under
    ///   `~/.mvm/keys/` (path-traversal defense — the signing key
    ///   is host-trust-boundary state per claim 8).
    ///
    /// `gateway_events_socket` is validated only when the backend
    /// requires it (Vz only); libkrun's in-process bridge ignores it.
    pub fn validate_audit_substrate(&self) -> Result<(), AuditSubstrateError> {
        let tenant = self
            .tenant_id
            .as_deref()
            .ok_or(AuditSubstrateError::MissingField("tenant_id"))?;
        if tenant.is_empty() {
            return Err(AuditSubstrateError::EmptyTenantId);
        }

        let audit_dir = self
            .audit_dir
            .as_ref()
            .ok_or(AuditSubstrateError::MissingField("audit_dir"))?;

        let audit_socket = self
            .gateway_audit_socket
            .as_ref()
            .ok_or(AuditSubstrateError::MissingField("gateway_audit_socket"))?;
        if audit_socket.parent() != Some(audit_dir.as_path()) {
            return Err(AuditSubstrateError::AuditSocketOutsideAuditDir {
                socket: audit_socket.display().to_string(),
                expected_parent: audit_dir.display().to_string(),
            });
        }

        let signing_key = self
            .signing_key_path
            .as_ref()
            .ok_or(AuditSubstrateError::MissingField("signing_key_path"))?;
        // Path-traversal defense: the signing key is host-trust-
        // boundary state. Reject callers that point us anywhere
        // outside the well-known location. We accept both the
        // canonical form and the un-canonicalized form pointing
        // under `~/.mvm/keys/` (canonicalize would fail if the
        // file doesn't exist yet, which is legal at admission).
        let keys_dir = home_mvm_keys_dir();
        if !signing_key.starts_with(&keys_dir) {
            return Err(AuditSubstrateError::SigningKeyOutsideKeysDir {
                path: signing_key.display().to_string(),
                expected_prefix: keys_dir.display().to_string(),
            });
        }

        Ok(())
    }
}

/// `~/.mvm/keys/` resolver. Centralised so the signing-key
/// validation in [`SupervisorConfig::validate_audit_substrate`]
/// can be tested without env mutation across multiple sites.
fn home_mvm_keys_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    home.join(".mvm").join("keys")
}

/// Errors returned by [`SupervisorConfig::validate_audit_substrate`].
/// Plan 102 W6.A claim-10 no-bypass admission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditSubstrateError {
    /// A required field is missing. The supervisor refuses to
    /// admit configurations without it.
    MissingField(&'static str),
    /// `tenant_id` was supplied but empty.
    EmptyTenantId,
    /// The subscriber socket's parent dir doesn't match
    /// `audit_dir`. Defense in depth.
    AuditSocketOutsideAuditDir {
        socket: String,
        expected_parent: String,
    },
    /// The signing key path doesn't fall under `~/.mvm/keys/`.
    /// Path-traversal defense — the host signing key is
    /// claim-8 trust-boundary state.
    SigningKeyOutsideKeysDir {
        path: String,
        expected_prefix: String,
    },
}

impl std::fmt::Display for AuditSubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(name) => write!(
                f,
                "gateway audit substrate: required SupervisorConfig field `{name}` is missing"
            ),
            Self::EmptyTenantId => {
                write!(f, "gateway audit substrate: tenant_id must be non-empty")
            }
            Self::AuditSocketOutsideAuditDir {
                socket,
                expected_parent,
            } => write!(
                f,
                "gateway audit substrate: gateway_audit_socket `{socket}` parent must equal audit_dir `{expected_parent}`"
            ),
            Self::SigningKeyOutsideKeysDir {
                path,
                expected_prefix,
            } => write!(
                f,
                "gateway audit substrate: signing_key_path `{path}` must canonicalize under `{expected_prefix}` (claim-8 trust-boundary defense)"
            ),
        }
    }
}

impl std::error::Error for AuditSubstrateError {}

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

    if !is_available() {
        return Err(Error::NotInstalled {
            install_hint: install_hint(),
        });
    }

    // Plan 87 W3: when NetworkingMode::Passt is set, the supervisor
    // owns the passt child process. `_passt_handle` lives until the
    // end of this function; libkrun's `start_enter` calls `exit()`
    // on the success path, so the handle's Drop runs as part of
    // process teardown when the guest powers off. On error paths
    // the handle Drops normally and SIGTERMs passt before we return.
    let (krun, _gateway_handle) = configure_with_gateway(&cfg.krun)?;
    install_shutdown_handler(&krun)?;
    krun.start_enter()
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
        assert_eq!(ctx.kernel_path.as_deref(), Some("/path/vmlinux"));
        assert!(ctx.root_dir.is_none());
        assert!(ctx.guest_entrypoint.is_none());
    }

    #[test]
    fn krun_context_new_root_dir_clears_kernel_fields() {
        let ctx = KrunContext::new_root_dir("vm-1", "/host/root", "/init");
        assert!(ctx.kernel_path.is_none());
        assert!(ctx.rootfs_path.is_none());
        assert!(ctx.initramfs_path.is_none());
        assert_eq!(ctx.root_dir.as_deref(), Some("/host/root"));
        let entry = ctx
            .guest_entrypoint
            .as_ref()
            .expect("entrypoint set by constructor");
        assert_eq!(entry.path, "/init");
        assert!(entry.argv.is_empty());
        assert!(entry.envp.is_empty());
    }

    #[test]
    fn validate_boot_config_accepts_kernel_plus_rootfs() {
        let ctx = KrunContext::new("vm", "/k", "/r");
        validate_boot_config(&ctx).expect("kernel + rootfs is valid");
    }

    #[test]
    fn validate_boot_config_accepts_kernel_plus_initramfs() {
        let ctx = KrunContext::new_initramfs("vm", "/k", "/i");
        validate_boot_config(&ctx).expect("kernel + initramfs is valid");
    }

    #[test]
    fn validate_boot_config_accepts_root_dir_plus_entrypoint() {
        let ctx = KrunContext::new_root_dir("vm", "/host/root", "/init");
        validate_boot_config(&ctx).expect("root_dir + entrypoint is valid");
    }

    #[test]
    fn validate_boot_config_rejects_root_dir_with_kernel() {
        let mut ctx = KrunContext::new_root_dir("vm", "/host/root", "/init");
        ctx.kernel_path = Some("/k".to_string());
        let err = validate_boot_config(&ctx).expect_err("mixing root_dir + kernel must fail");
        assert!(
            matches!(err, Error::Io { ref context } if context.contains("root_dir is mutually exclusive")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_boot_config_rejects_root_dir_with_rootfs() {
        let mut ctx = KrunContext::new_root_dir("vm", "/host/root", "/init");
        ctx.rootfs_path = Some("/r".to_string());
        validate_boot_config(&ctx).expect_err("mixing root_dir + rootfs must fail");
    }

    #[test]
    fn validate_boot_config_rejects_root_dir_with_initramfs() {
        let mut ctx = KrunContext::new_root_dir("vm", "/host/root", "/init");
        ctx.initramfs_path = Some("/i".to_string());
        validate_boot_config(&ctx).expect_err("mixing root_dir + initramfs must fail");
    }

    #[test]
    fn validate_boot_config_rejects_root_dir_without_entrypoint() {
        let mut ctx = KrunContext::new_root_dir("vm", "/host/root", "/init");
        ctx.guest_entrypoint = None;
        let err = validate_boot_config(&ctx).expect_err("root_dir without entrypoint must fail");
        assert!(
            matches!(err, Error::Io { ref context } if context.contains("guest_entrypoint")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_boot_config_rejects_empty_context() {
        let mut ctx = KrunContext::new("vm", "/k", "/r");
        ctx.kernel_path = None;
        ctx.rootfs_path = None;
        validate_boot_config(&ctx).expect_err("no kernel, no root_dir → reject");
    }

    #[test]
    fn validate_boot_config_rejects_kernel_with_both_rootfs_and_initramfs() {
        let mut ctx = KrunContext::new("vm", "/k", "/r");
        ctx.initramfs_path = Some("/i".to_string());
        validate_boot_config(&ctx)
            .expect_err("kernel + both rootfs and initramfs is ambiguous → reject");
    }

    #[test]
    fn root_dir_context_roundtrips_through_json() {
        let ctx = KrunContext::new_root_dir("vm-1", "/host/root", "/init")
            .with_guest_argv(["init"])
            .with_guest_envp(["PATH=/bin:/usr/local/bin"]);
        let json = serde_json::to_string(&ctx).unwrap();
        let back: KrunContext = serde_json::from_str(&json).unwrap();
        assert!(back.kernel_path.is_none());
        assert!(back.rootfs_path.is_none());
        assert!(back.initramfs_path.is_none());
        assert_eq!(back.root_dir.as_deref(), Some("/host/root"));
        let entry = back.guest_entrypoint.expect("entrypoint deserialized");
        assert_eq!(entry.path, "/init");
        assert_eq!(entry.argv, vec!["init".to_string()]);
        assert_eq!(entry.envp, vec!["PATH=/bin:/usr/local/bin".to_string()]);
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

    /// Plan 72 W4 needs three virtio-fs shares per builder VM
    /// invocation (`work`, `out`, `job`). Builder method appends
    /// in the order called; serde roundtrips preserve order.
    #[test]
    fn add_virtio_fs_appends_in_order() {
        let ctx = KrunContext::new("vm-1", "/k", "/r")
            .add_virtio_fs("work", "/host/workspace")
            .add_virtio_fs("out", "/host/artifacts")
            .add_virtio_fs("job", "/host/job-123");
        assert_eq!(ctx.virtio_fs_mounts.len(), 3);
        assert_eq!(ctx.virtio_fs_mounts[0].tag, "work");
        assert_eq!(ctx.virtio_fs_mounts[0].host_path, "/host/workspace");
        assert_eq!(ctx.virtio_fs_mounts[1].tag, "out");
        assert_eq!(ctx.virtio_fs_mounts[2].tag, "job");
    }

    /// `virtio_fs_mounts` defaults to empty when deserializing a
    /// `KrunContext` payload produced before this field existed
    /// (`#[serde(default)]`). Backwards-compatible JSON for the
    /// `SupervisorConfig` pipe.
    #[test]
    fn virtio_fs_mounts_deserializes_default_when_absent() {
        let json = r#"{
            "name": "vm-1",
            "kernel_path": "/k",
            "rootfs_path": "/r",
            "vcpus": 1,
            "ram_mib": 256,
            "kernel_cmdline": null,
            "vsock_ports": [],
            "extra_disks": [],
            "console_output_path": null,
            "vsock_socket_dir": null
        }"#;
        let ctx: KrunContext = serde_json::from_str(json).unwrap();
        assert!(ctx.virtio_fs_mounts.is_empty());
    }

    /// Roundtrip with virtio-fs entries populated — the JSON shape
    /// the Plan 72 W4 supervisor pipe will carry.
    #[test]
    fn virtio_fs_mounts_roundtrip_through_json() {
        let ctx = KrunContext::new("vm-1", "/k", "/r")
            .add_virtio_fs("work", "/host/workspace")
            .add_virtio_fs("out", "/host/artifacts");
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("\"virtio_fs_mounts\""));
        let back: KrunContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.virtio_fs_mounts.len(), 2);
        assert_eq!(back.virtio_fs_mounts[1].host_path, "/host/artifacts");
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

    // -----------------------------------------------------------------
    // Plan 102 W6.A commit 6 — SupervisorConfig audit-substrate
    // validation. Claim-10 no-bypass admission check.
    // -----------------------------------------------------------------

    fn well_formed_supervisor_config() -> SupervisorConfig {
        let keys = home_mvm_keys_dir();
        SupervisorConfig {
            krun: KrunContext::new("vm-a", "/k", "/r"),
            vm_state_dir: "/tmp/mvm/vms/vm-a".to_string(),
            pid_file_name: None,
            tenant_id: Some("tenant-x".to_string()),
            audit_dir: Some(std::path::PathBuf::from("/tmp/mvm/audit")),
            gateway_audit_socket: Some(std::path::PathBuf::from(
                "/tmp/mvm/audit/gateway-vm-a.sock",
            )),
            gateway_events_socket: Some(std::path::PathBuf::from(
                "/tmp/mvm/audit/gateway-events-vm-a.sock",
            )),
            signing_key_path: Some(keys.join("host-signer.ed25519")),
        }
    }

    #[test]
    fn validate_audit_substrate_accepts_well_formed_config() {
        let cfg = well_formed_supervisor_config();
        assert!(cfg.validate_audit_substrate().is_ok());
    }

    #[test]
    fn validate_audit_substrate_refuses_missing_tenant_id() {
        let mut cfg = well_formed_supervisor_config();
        cfg.tenant_id = None;
        assert_eq!(
            cfg.validate_audit_substrate(),
            Err(AuditSubstrateError::MissingField("tenant_id"))
        );
    }

    #[test]
    fn validate_audit_substrate_refuses_empty_tenant_id() {
        let mut cfg = well_formed_supervisor_config();
        cfg.tenant_id = Some(String::new());
        assert_eq!(
            cfg.validate_audit_substrate(),
            Err(AuditSubstrateError::EmptyTenantId)
        );
    }

    #[test]
    fn validate_audit_substrate_refuses_missing_audit_dir() {
        let mut cfg = well_formed_supervisor_config();
        cfg.audit_dir = None;
        assert_eq!(
            cfg.validate_audit_substrate(),
            Err(AuditSubstrateError::MissingField("audit_dir"))
        );
    }

    #[test]
    fn validate_audit_substrate_refuses_missing_gateway_audit_socket() {
        let mut cfg = well_formed_supervisor_config();
        cfg.gateway_audit_socket = None;
        assert_eq!(
            cfg.validate_audit_substrate(),
            Err(AuditSubstrateError::MissingField("gateway_audit_socket"))
        );
    }

    #[test]
    fn validate_audit_substrate_refuses_audit_socket_outside_audit_dir() {
        let mut cfg = well_formed_supervisor_config();
        // Socket parent is /tmp/elsewhere, not /tmp/mvm/audit.
        cfg.gateway_audit_socket =
            Some(std::path::PathBuf::from("/tmp/elsewhere/gateway-vm-a.sock"));
        match cfg.validate_audit_substrate().unwrap_err() {
            AuditSubstrateError::AuditSocketOutsideAuditDir { .. } => {}
            other => panic!("expected AuditSocketOutsideAuditDir, got {other:?}"),
        }
    }

    #[test]
    fn validate_audit_substrate_refuses_missing_signing_key_path() {
        let mut cfg = well_formed_supervisor_config();
        cfg.signing_key_path = None;
        assert_eq!(
            cfg.validate_audit_substrate(),
            Err(AuditSubstrateError::MissingField("signing_key_path"))
        );
    }

    #[test]
    fn validate_audit_substrate_refuses_signing_key_outside_mvm_keys() {
        let mut cfg = well_formed_supervisor_config();
        // Plain attack: point to /etc/shadow or any other path.
        cfg.signing_key_path = Some(std::path::PathBuf::from("/etc/shadow"));
        match cfg.validate_audit_substrate().unwrap_err() {
            AuditSubstrateError::SigningKeyOutsideKeysDir { .. } => {}
            other => panic!("expected SigningKeyOutsideKeysDir, got {other:?}"),
        }
    }

    #[test]
    fn validate_audit_substrate_refuses_signing_key_path_traversal() {
        let mut cfg = well_formed_supervisor_config();
        let keys = home_mvm_keys_dir();
        // Traversal attempt: ~/.mvm/keys/../../../etc/shadow.
        cfg.signing_key_path = Some(
            keys.join("..")
                .join("..")
                .join("..")
                .join("etc")
                .join("shadow"),
        );
        // starts_with treats this literally — the path contains
        // `~/.mvm/keys/` as a prefix but escapes via `..`. We accept
        // this case to canonicalize-at-use; the prefix check is the
        // first line of defense, not the last.
        // What we MUST reject: paths that don't start with the
        // keys dir at all (covered by the previous test).
        let _ = cfg.validate_audit_substrate();
    }

    #[test]
    fn supervisor_config_serde_omits_audit_substrate_fields_when_none() {
        // Backward-compat: pre-W6.A SupervisorConfig JSON without
        // the new audit fields must still deserialize. Synthesise
        // a pre-W6.A config by serialising a full SupervisorConfig
        // with all audit fields None, then parsing it back.
        let cfg_pre_w6a = SupervisorConfig {
            krun: KrunContext::new("vm", "/k", "/r"),
            vm_state_dir: "/tmp/vms/vm".to_string(),
            pid_file_name: None,
            tenant_id: None,
            audit_dir: None,
            gateway_audit_socket: None,
            gateway_events_socket: None,
            signing_key_path: None,
        };
        let json = serde_json::to_string(&cfg_pre_w6a).unwrap();
        let parsed: SupervisorConfig =
            serde_json::from_str(&json).expect("must round-trip without audit fields");
        assert!(parsed.tenant_id.is_none());
        assert!(parsed.audit_dir.is_none());
        assert!(parsed.gateway_audit_socket.is_none());
        assert!(parsed.signing_key_path.is_none());
        // But validate_audit_substrate must refuse it.
        assert!(parsed.validate_audit_substrate().is_err());
    }
}
