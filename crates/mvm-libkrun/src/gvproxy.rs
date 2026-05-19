//! Gvproxy-backed virtio-net supervisor (Plan 88 W2 / ADR-055
//! cross-platform amendment).
//!
//! `gvproxy` is a userspace network gateway (containers/gvisor-tap-vsock
//! project, Apache-2.0, single statically-linked binary) that translates
//! between virtio-net frames the libkrun guest writes to a unix-domain
//! socket and AF_INET sockets on the host. The slp/krun Homebrew tap
//! ships it as the canonical macOS networking backend for libkrun,
//! filling the role passt fills on Linux (passt itself doesn't build on
//! macOS — see ADR-055 §"Cross-platform backends").
//!
//! The integration model differs from passt:
//!
//! - passt: parent creates a socketpair, hands one end to passt via
//!   `--fd=N`, keeps the other end; libkrun consumes the parent end.
//! - gvproxy: gvproxy itself creates a listening unix-domain socket
//!   when invoked with `--listen-vfkit <path>`; libkrun connects to
//!   that path via `krun_add_net_unixgram(ctx, c_path, fd=-1, …)`.
//!
//! Boot sequence:
//!
//! 1. Host calls [`spawn`]. It picks a socket path under the
//!    scratch dir, spawns `gvproxy --listen-vfkit <socket>
//!    --log-file <log>`, then polls for the socket file to appear
//!    (gvproxy creates it ~tens of ms after spawn).
//! 2. Host stuffs the socket path into [`crate::KrunContext`] via
//!    [`crate::KrunContext::with_gvproxy`].
//! 3. libkrun's `start_enter` opens the socket and consumes the
//!    virtio-net frames.
//! 4. When the host drops the [`GvproxyHandle`], the supervisor
//!    `SIGTERM`s gvproxy, waits up to [`SHUTDOWN_GRACE`], then
//!    `SIGKILL`s.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Grace period between SIGTERM and SIGKILL during shutdown. Matches
/// the [`crate::passt::SHUTDOWN_GRACE`] knob so both backends behave
/// the same way under cleanup pressure.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long [`spawn`] waits for gvproxy's listener socket to appear
/// on disk. gvproxy creates the file within ~tens of milliseconds on
/// macOS Apple Silicon (no `bind(2)` blocking). 500ms is generous
/// without measurably slowing `dev up`.
pub const SOCKET_READY_TIMEOUT: Duration = Duration::from_millis(500);

/// Default MAC for the guest's `eth0`. Locally-administered (bit
/// `0x02` set), unicast, stable across boots. Matches
/// [`crate::passt::DEFAULT_GUEST_MAC`] so the in-guest udev rules
/// don't reshuffle the interface name when a contributor switches
/// between backends.
pub const DEFAULT_GUEST_MAC: [u8; 6] = [0xAE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];

/// Errors the supervisor can return.
#[derive(Debug)]
pub enum GvproxyError {
    /// `gvproxy` binary not found on `$PATH`.
    NotInstalled { install_hint: &'static str },
    /// Spawning the gvproxy child process failed.
    Spawn(io::Error),
    /// gvproxy exited before the listener socket appeared on disk.
    EarlyExit { status: std::process::ExitStatus },
    /// `SOCKET_READY_TIMEOUT` elapsed without gvproxy creating its
    /// listener socket. Typically a permission issue on the scratch
    /// dir or a fatal error gvproxy logged before listening.
    SocketTimeout { socket_path: PathBuf },
    /// Filesystem I/O failure (scratch dir create, etc.).
    Io { context: String, source: io::Error },
}

impl std::fmt::Display for GvproxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GvproxyError::NotInstalled { install_hint } => {
                write!(f, "`gvproxy` binary not found on $PATH. {install_hint}")
            }
            GvproxyError::Spawn(e) => write!(f, "spawning gvproxy failed: {e}"),
            GvproxyError::EarlyExit { status } => write!(
                f,
                "gvproxy exited before its listener socket appeared (status: {status:?})"
            ),
            GvproxyError::SocketTimeout { socket_path } => write!(
                f,
                "gvproxy did not create its listener socket at {} within {} ms",
                socket_path.display(),
                SOCKET_READY_TIMEOUT.as_millis()
            ),
            GvproxyError::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for GvproxyError {}

/// Suggested install command for the current host platform. Surfaced
/// both in `GvproxyError::NotInstalled` and `mvmctl doctor`.
pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        // slp/krun is the same tap that ships libkrun + libkrunfw, so
        // pointing at it keeps the doc story consistent.
        "Install with: brew install slp/krun/gvproxy"
    } else if cfg!(target_os = "linux") {
        // Most distros don't package gvproxy. Building from source is
        // a single `go build` against
        // github.com/containers/gvisor-tap-vsock.
        "Install from source: https://github.com/containers/gvisor-tap-vsock"
    } else {
        "Install gvproxy: https://github.com/containers/gvisor-tap-vsock"
    }
}

/// Probe `$PATH` for `gvproxy`. Returns the absolute path on success.
pub fn locate_gvproxy() -> Option<PathBuf> {
    which::which("gvproxy").ok()
}

/// Owning handle to a running gvproxy child process. `Drop` cleans up
/// the child the same way [`crate::passt::PasstHandle::drop`] does:
/// SIGTERM → grace period → SIGKILL → reap.
#[derive(Debug)]
pub struct GvproxyHandle {
    child: Option<Child>,
    socket_path: PathBuf,
}

impl GvproxyHandle {
    /// Path to the unix-domain socket gvproxy listens on. Hand this
    /// to [`crate::KrunContext::with_gvproxy`] (or directly to
    /// [`crate::sys::Context::add_net_unixgram_path`] for advanced
    /// callers).
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Spawn a gvproxy child process and return a handle to its listener
/// socket path. The child logs to `scratch_dir/gvproxy.log`.
///
/// The returned [`GvproxyHandle`] must outlive any libkrun
/// configuration referencing `socket_path()`. On Drop the child is
/// killed gracefully and the socket file is deleted (best-effort —
/// libkrun may have already consumed it).
pub fn spawn(scratch_dir: &Path) -> Result<GvproxyHandle, GvproxyError> {
    let gvproxy_bin = locate_gvproxy().ok_or_else(|| GvproxyError::NotInstalled {
        install_hint: install_hint(),
    })?;

    std::fs::create_dir_all(scratch_dir).map_err(|e| GvproxyError::Io {
        context: format!("creating gvproxy scratch dir {}", scratch_dir.display()),
        source: e,
    })?;

    let socket_path = scratch_dir.join("gvproxy.sock");
    let log_path = scratch_dir.join("gvproxy.log");

    // Remove a stale socket from a previous run before spawning —
    // gvproxy refuses to bind if the file exists.
    let _ = std::fs::remove_file(&socket_path);

    // gvproxy args we care about:
    //   -listen-vfkit <path> — unix-domain socket libkrun connects to
    //                          via `krun_add_net_unixgram`. "vfkit"
    //                          mode is the libkrun-compatible one.
    //   -log-file <path>     — diagnostic log; absent → stderr (lost
    //                          when we redirect to /dev/null).
    //   -debug               — verbose logging. Not set by default;
    //                          if a future MVM_GVPROXY_DEBUG=1 env
    //                          var trips this we'd flip it here.
    // gvproxy expects the `-listen-vfkit` arg as a URL —
    // `unixgram://<path>`. A bare path errors out with
    // "vfkit listen address must be unixgram:// address" before the
    // listener is created. The libkrun-end of the socket connects
    // to the absolute path; the URL prefix only carries the scheme.
    let listen_url = {
        let mut s = OsString::from("unixgram://");
        s.push(socket_path.as_os_str());
        s
    };
    let mut cmd = Command::new(&gvproxy_bin);
    cmd.arg("-listen-vfkit")
        .arg(listen_url)
        .arg("-log-file")
        .arg(OsString::from(&log_path))
        // Redirect stdout/stderr to /dev/null — gvproxy's `-log-file`
        // captures what we need and any spillover noise on stderr
        // pollutes the supervisor's own diagnostic stream.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().map_err(GvproxyError::Spawn)?;

    // Poll for the socket to appear, with a bounded budget. gvproxy
    // creates the file synchronously inside its main loop on startup,
    // so this should resolve within ~tens of ms. We also re-check
    // `try_wait()` every iteration so an early exit (missing arg,
    // permission denied, etc.) surfaces immediately instead of as
    // a SocketTimeout.
    let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
    loop {
        if socket_path.exists() {
            return Ok(GvproxyHandle {
                child: Some(child),
                socket_path,
            });
        }
        if let Some(status) = child.try_wait().map_err(GvproxyError::Spawn)? {
            return Err(GvproxyError::EarlyExit { status });
        }
        if Instant::now() >= deadline {
            // Kill the still-running child before bailing — leaking it
            // would block whatever the caller does next.
            let _ = child.kill();
            let _ = child.wait();
            return Err(GvproxyError::SocketTimeout { socket_path });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for GvproxyHandle {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        // Already-dead is fine.
        if matches!(child.try_wait(), Ok(Some(_))) {
            let _ = std::fs::remove_file(&self.socket_path);
            return;
        }

        // SIGTERM → wait → SIGKILL.
        let pid = child.id() as i32;
        // SAFETY: pid is valid until we wait; SIGTERM on a stale pid
        // returns ESRCH which we treat as "already gone".
        unsafe { libc::kill(pid, libc::SIGTERM) };

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => break,
            }
        }

        if matches!(child.try_wait(), Ok(None)) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = child.wait();
        }

        // Best-effort socket cleanup — if libkrun was holding the fd
        // open, the inode goes away when the last fd closes, but the
        // path entry remains. Removing it explicitly keeps
        // `~/.cache/mvm/builder-vm/vms/<vm>/` tidy.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn install_hint_is_platform_specific() {
        let hint = install_hint();
        assert!(!hint.is_empty());
        if cfg!(target_os = "macos") {
            assert!(hint.contains("brew install"), "hint: {hint}");
        }
    }

    #[test]
    fn locate_gvproxy_is_optional() {
        let _ = locate_gvproxy();
    }

    #[test]
    fn spawn_without_gvproxy_returns_not_installed() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let original_path = std::env::var_os("PATH");
        // SAFETY: tests are single-threaded; serialize env mutation.
        unsafe {
            std::env::set_var("PATH", tmp.path());
        }
        let result = spawn(tmp.path());
        unsafe {
            match original_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
        match result {
            Err(GvproxyError::NotInstalled { install_hint }) => {
                assert!(!install_hint.is_empty());
            }
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    /// Spawn gvproxy, verify the socket path exists, then drop the
    /// handle. Skipped when gvproxy isn't installed.
    #[test]
    fn spawn_then_drop_reaps_child() {
        let Some(_) = locate_gvproxy() else {
            eprintln!("test skipped: gvproxy not on PATH");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let handle = spawn(tmp.path()).expect("spawn gvproxy");
        let socket = handle.socket_path().to_path_buf();
        assert!(socket.exists(), "socket missing: {}", socket.display());
        drop(handle);
        // After Drop the socket file is cleaned up.
        assert!(
            !socket.exists(),
            "socket lingered after Drop: {}",
            socket.display()
        );
    }
}
