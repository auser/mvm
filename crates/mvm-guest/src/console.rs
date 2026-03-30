//! PTY-over-vsock console for interactive guest access.
//!
//! The guest agent allocates a PTY, forks a shell, and relays I/O over a
//! dedicated vsock data port. The host connects to the data port for raw
//! byte streaming — no JSON framing, no Ed25519 signing on the data channel.
//!
//! Security: Console sessions are dev-mode only and authenticated via the
//! control channel (the `ConsoleOpen` request goes through the normal
//! authenticated vsock protocol).

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::vsock::CONSOLE_PORT_BASE;

/// Tracks the active console session. Only one session at a time.
static CONSOLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONSOLE_SESSION_ID: AtomicU32 = AtomicU32::new(0);
/// Active PTY master fd for resize support. -1 when no session is active.
static CONSOLE_MASTER_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Result of opening a console session.
pub struct ConsoleSession {
    pub session_id: u32,
    pub data_port: u32,
    pub master_fd: RawFd,
    pub child_pid: i32,
}

/// Errors from console operations.
#[derive(Debug)]
pub enum ConsoleError {
    AlreadyActive,
    OpenPtyFailed,
    ForkFailed,
    BindFailed(u32),
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyActive => write!(f, "a console session is already active"),
            Self::OpenPtyFailed => write!(f, "openpty() failed"),
            Self::ForkFailed => write!(f, "fork() failed"),
            Self::BindFailed(port) => write!(f, "failed to bind vsock port {port}"),
        }
    }
}

impl std::error::Error for ConsoleError {}

// FFI declarations for PTY operations
unsafe extern "C" {
    fn openpty(
        amaster: *mut i32,
        aslave: *mut i32,
        name: *mut u8,
        termp: *const core::ffi::c_void,
        winp: *const Winsize,
    ) -> i32;
    fn setsid() -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn execvp(file: *const u8, argv: *const *const u8) -> i32;
    fn fork() -> i32;
    fn close(fd: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;

    // Vsock
    fn socket(domain: i32, typ: i32, protocol: i32) -> i32;
    fn bind(sockfd: i32, addr: *const core::ffi::c_void, addrlen: u32) -> i32;
    fn listen(sockfd: i32, backlog: i32) -> i32;
    fn accept(sockfd: i32, addr: *mut core::ffi::c_void, addrlen: *mut u32) -> i32;
}

const AF_VSOCK: i32 = 40;
const SOCK_STREAM: i32 = 1;
const VMADDR_CID_ANY: u32 = 0xFFFF_FFFF;
const SIGTERM: i32 = 15;

/// ioctl request for setting window size (Linux).
#[cfg(target_os = "linux")]
const TIOCSWINSZ: u64 = 0x5414;
#[cfg(not(target_os = "linux"))]
const TIOCSWINSZ: u64 = 0x80087467;

#[repr(C)]
struct SockAddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

/// Terminal window size (matches struct winsize in sys/ioctl.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

/// Open a PTY console session.
///
/// Allocates a PTY pair, forks a shell process attached to the slave,
/// and returns the master fd + session info. The caller is responsible
/// for starting the vsock data relay.
pub fn open_session(cols: u16, rows: u16) -> Result<ConsoleSession, ConsoleError> {
    // Only one session at a time
    if CONSOLE_ACTIVE.swap(true, Ordering::SeqCst) {
        return Err(ConsoleError::AlreadyActive);
    }

    let session_id = CONSOLE_SESSION_ID.fetch_add(1, Ordering::SeqCst) + 1;
    let data_port = CONSOLE_PORT_BASE + session_id;

    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let mut master_fd: i32 = -1;
    let mut slave_fd: i32 = -1;

    // SAFETY: openpty with valid pointers
    let rc = unsafe {
        openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        CONSOLE_ACTIVE.store(false, Ordering::SeqCst);
        return Err(ConsoleError::OpenPtyFailed);
    }

    // SAFETY: fork()
    let pid = unsafe { fork() };
    if pid < 0 {
        unsafe {
            close(master_fd);
            close(slave_fd);
        }
        CONSOLE_ACTIVE.store(false, Ordering::SeqCst);
        return Err(ConsoleError::ForkFailed);
    }

    if pid == 0 {
        // Child process — attach to PTY slave and exec shell
        unsafe {
            close(master_fd);
            setsid();
            // Redirect stdin/stdout/stderr to the PTY slave
            dup2(slave_fd, 0);
            dup2(slave_fd, 1);
            dup2(slave_fd, 2);
            if slave_fd > 2 {
                close(slave_fd);
            }

            // Set TERM environment variable
            let term = b"TERM=xterm-256color\0";
            libc_putenv(term.as_ptr());

            // Exec /bin/sh
            let shell = b"/bin/sh\0";
            let dash_i = b"-i\0";
            let argv: [*const u8; 3] = [shell.as_ptr(), dash_i.as_ptr(), std::ptr::null()];
            execvp(shell.as_ptr(), argv.as_ptr());

            // execvp only returns on error
            std::process::exit(127);
        }
    }

    // Parent — close slave fd, store master fd for resize
    unsafe {
        close(slave_fd);
    }
    CONSOLE_MASTER_FD.store(master_fd, std::sync::atomic::Ordering::SeqCst);

    Ok(ConsoleSession {
        session_id,
        data_port,
        master_fd,
        child_pid: pid,
    })
}

/// Resize the active console session's PTY window.
///
/// Called from the guest agent when it receives a `ConsoleResize` request.
/// Uses the globally tracked master fd.
pub fn resize_active_session(cols: u16, rows: u16) -> bool {
    let fd = CONSOLE_MASTER_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return false;
    }
    resize_pty(fd, cols, rows);
    true
}

/// Resize the PTY window.
pub fn resize_pty(master_fd: RawFd, cols: u16, rows: u16) {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl with valid fd and pointer
    unsafe {
        ioctl(master_fd, TIOCSWINSZ, &ws);
    }
}

/// Close a console session — kill the shell and clean up.
pub fn close_session(session: &ConsoleSession) -> i32 {
    // Kill the shell process
    unsafe {
        kill(session.child_pid, SIGTERM);
    }

    // Wait for it to exit
    let mut status: i32 = 0;
    let _ = unsafe { waitpid(session.child_pid, &mut status, 0) };

    // Close the master fd
    unsafe {
        close(session.master_fd);
    }

    CONSOLE_MASTER_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
    CONSOLE_ACTIVE.store(false, Ordering::SeqCst);

    // Extract exit code
    if status & 0x7f == 0 {
        (status >> 8) & 0xff // normal exit
    } else {
        128 + (status & 0x7f) // signal
    }
}

/// Start the vsock data relay for a console session.
///
/// Binds a vsock listener on `session.data_port`, accepts one connection,
/// and relays raw bytes between the vsock socket and the PTY master fd.
/// Blocks until the session ends (shell exits or connection drops).
///
/// Returns the shell exit code.
pub fn run_console_relay(session: &ConsoleSession) -> i32 {
    // Bind vsock listener on data_port
    let listen_fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
    if listen_fd < 0 {
        eprintln!("console: failed to create vsock socket");
        return close_session(session);
    }

    let addr = SockAddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: session.data_port,
        svm_cid: VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };

    let rc = unsafe {
        bind(
            listen_fd,
            &addr as *const SockAddrVm as *const core::ffi::c_void,
            std::mem::size_of::<SockAddrVm>() as u32,
        )
    };
    if rc != 0 {
        eprintln!("console: failed to bind vsock port {}", session.data_port);
        unsafe {
            close(listen_fd);
        }
        return close_session(session);
    }

    if unsafe { listen(listen_fd, 1) } != 0 {
        eprintln!(
            "console: failed to listen on vsock port {}",
            session.data_port
        );
        unsafe {
            close(listen_fd);
        }
        return close_session(session);
    }

    eprintln!(
        "console: waiting for host connection on vsock port {}",
        session.data_port
    );

    // Accept one connection
    let conn_fd = unsafe { accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
    unsafe {
        close(listen_fd);
    } // Don't accept more connections
    if conn_fd < 0 {
        eprintln!("console: accept failed");
        return close_session(session);
    }

    eprintln!("console: host connected, starting PTY relay");

    // Relay: PTY master ↔ vsock connection using raw byte I/O
    // Two threads: vsock→pty and pty→vsock
    let master_fd = session.master_fd;
    let child_pid = session.child_pid;

    // SAFETY: valid fd from accept
    let mut vsock_read = unsafe { std::os::unix::net::UnixStream::from_raw_fd(conn_fd as RawFd) };
    let Ok(mut vsock_write) = vsock_read.try_clone() else {
        eprintln!("console: failed to clone vsock stream");
        return close_session(session);
    };

    // Set read timeout for idle detection (15 minutes)
    let idle_timeout = std::time::Duration::from_secs(15 * 60);
    let _ = vsock_read.set_read_timeout(Some(idle_timeout));

    // SAFETY: valid fd from openpty
    let mut pty_read = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let Ok(mut pty_write) = pty_read.try_clone() else {
        eprintln!("console: failed to clone PTY fd");
        std::mem::forget(pty_read);
        return close_session(session);
    };

    let done = std::sync::Arc::new(AtomicBool::new(false));
    let done2 = done.clone();

    // vsock → PTY (host input → shell)
    // Idle timeout: if no input for 15 minutes, the read times out and we close.
    let h1 = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match vsock_read.read(&mut buf) {
                Ok(0) => {
                    done2.store(true, Ordering::SeqCst);
                    break;
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    eprintln!("console: idle timeout reached, closing session");
                    done2.store(true, Ordering::SeqCst);
                    break;
                }
                Err(_) => {
                    done2.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    if pty_write.write_all(&buf[..n]).is_err() {
                        done2.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
    });

    // PTY → vsock (shell output → host)
    let h2 = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if vsock_write.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = vsock_write.flush();
                }
            }
        }
    });

    // Wait for either direction to finish
    let _ = h2.join(); // PTY output ends when shell exits
    let _ = h1.join();

    // Wait for child and get exit code
    let mut status: i32 = 0;
    unsafe {
        kill(child_pid, SIGTERM);
        waitpid(child_pid, &mut status, 0);
    }

    CONSOLE_MASTER_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
    CONSOLE_ACTIVE.store(false, Ordering::SeqCst);

    // Don't call close_session — we already waited and the fds are owned
    // by the File/UnixStream objects which will drop.
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

/// Check if a console session is currently active.
pub fn is_active() -> bool {
    CONSOLE_ACTIVE.load(Ordering::SeqCst)
}

// FFI for putenv
unsafe extern "C" {
    fn putenv(string: *const u8) -> i32;
}

unsafe fn libc_putenv(s: *const u8) {
    unsafe { putenv(s) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_winsize_layout() {
        assert_eq!(std::mem::size_of::<Winsize>(), 8);
    }

    #[test]
    fn test_console_error_display() {
        assert_eq!(
            ConsoleError::AlreadyActive.to_string(),
            "a console session is already active"
        );
        assert_eq!(ConsoleError::OpenPtyFailed.to_string(), "openpty() failed");
        assert_eq!(ConsoleError::ForkFailed.to_string(), "fork() failed");
        assert_eq!(
            ConsoleError::BindFailed(20001).to_string(),
            "failed to bind vsock port 20001"
        );
    }

    #[test]
    fn test_data_port_calculation() {
        assert_eq!(CONSOLE_PORT_BASE + 1, 20001);
        assert_eq!(CONSOLE_PORT_BASE + 42, 20042);
    }

    #[test]
    fn test_is_active_default() {
        // Reset state for test (note: tests run in parallel, so this
        // tests the initial value only in isolation)
        CONSOLE_ACTIVE.store(false, Ordering::SeqCst);
        assert!(!is_active());
    }
}
