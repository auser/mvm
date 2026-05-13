//! Plan 57 W3 — end-to-end libkrun boot validation.
//!
//! Run with:
//!
//! ```text
//! cargo run --example libkrun-smoke --features libkrun-sys
//! ```
//!
//! What it does:
//!
//! 1. On macOS, ensures the binary carries the `com.apple.security.hypervisor`
//!    entitlement via [`mvm_providers::apple_container::ensure_signed`]; if
//!    not, codesigns ad-hoc and re-execs (the existing pattern shared with
//!    the VZ backend).
//! 2. Binds a Unix listener at `$TMPDIR/mvm-libkrun-smoke.sock`.
//! 3. Spawns itself with `MVM_LIBKRUN_SMOKE_CHILD=1`. The child constructs
//!    a `KrunContext` pointing at the artifacts from `examples/minimal/`
//!    and calls `mvm_libkrun::boot(...)`, which dispatches into
//!    `krun_start_enter`. libkrun documents that call as never returning
//!    on success — the VMM takes over the process and `exit()`s with the
//!    guest's status when the microVM powers off. The child therefore
//!    represents the libkrun process boundary.
//! 4. The parent `accept`s the connection from the guest's `vsock_ok`
//!    payload, reads `"ok\n"`, then `wait`s on the child PID.
//!
//! Artifact discovery (parent reads, child reads):
//!
//! - `MVM_LIBKRUN_SMOKE_KERNEL` — path to `vmlinux` (Image / bzImage).
//! - `MVM_LIBKRUN_SMOKE_ROOTFS` — path to `rootfs.ext4`.
//!
//! If unset, the parent defaults to the symlinks `nix build` creates next
//! to `examples/minimal/flake.nix`. If neither the env vars nor the
//! default symlinks point at real files, the parent prints a hint and
//! exits 75 (per `sysexits.h` "user data error" — the user has not yet
//! produced the artifacts).
//!
//! Exit codes:
//!
//! - `0` — PASS: guest wrote `"ok"` over vsock and the child exited cleanly.
//! - `70` — internal error: child fell through `krun_start_enter` (libkrun
//!   returned a negative errno before booting the microVM).
//! - `72` — guest connected but didn't write `"ok"` on the socket.
//! - `73` — child exited non-zero (microVM panicked or libkrun rejected
//!   the configuration).
//! - `75` — artifacts missing.

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use mvm_libkrun::KrunContext;

/// Guest vsock port the guest's `vsock_ok` payload connects to.
/// Must match `VSOCK_PORT` in `examples/minimal/vsock_ok.c`.
const VSOCK_PORT: u32 = 1234;

const CHILD_ENV: &str = "MVM_LIBKRUN_SMOKE_CHILD";
const SOCKET_ENV: &str = "MVM_LIBKRUN_SMOKE_SOCKET";
const KERNEL_ENV: &str = "MVM_LIBKRUN_SMOKE_KERNEL";
const ROOTFS_ENV: &str = "MVM_LIBKRUN_SMOKE_ROOTFS";

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    // Codesign / re-exec on macOS. No-op on other platforms and on
    // already-signed binaries. Must run before any libkrun call so the
    // child process inherits an entitled binary.
    mvm_providers::apple_container::ensure_signed();

    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        // `boot()` returns only on configuration error; if we reach
        // this line, libkrun rejected the configuration before
        // entering the VMM loop.
        std::process::exit(70);
    }

    let code = run_parent();
    std::process::exit(code);
}

fn run_parent() -> i32 {
    let kernel = resolve_artifact(KERNEL_ENV, "vmlinux");
    let rootfs = resolve_artifact(ROOTFS_ENV, "rootfs.ext4");
    if !kernel.is_file() || !rootfs.is_file() {
        print_artifact_hint(&kernel, &rootfs);
        return 75;
    }

    let socket_path = std::env::temp_dir().join("mvm-libkrun-smoke.sock");
    // Stale socket from a previous interrupted run would make bind()
    // fail with EADDRINUSE; clean it up unconditionally. The host
    // controls this path so racing with another smoke run is the
    // caller's problem.
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[smoke] bind {} failed: {e}", socket_path.display());
            return 71;
        }
    };
    eprintln!("[smoke] parent listening on {}", socket_path.display());

    // Spawn ourselves as the libkrun-owning child. The child inherits
    // the MVM_SIGNED=1 marker that ensure_signed set, so it won't try
    // to re-sign or re-exec again.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[smoke] current_exe failed: {e}");
            return 70;
        }
    };
    let mut child = match Command::new(&exe)
        .env(CHILD_ENV, "1")
        .env(SOCKET_ENV, &socket_path)
        .env(KERNEL_ENV, &kernel)
        .env(ROOTFS_ENV, &rootfs)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[smoke] failed to spawn libkrun child: {e}");
            return 70;
        }
    };
    eprintln!("[smoke] spawned libkrun child pid={}", child.id());

    // The smoke kernel is configured to boot promptly; if it stalls
    // past the accept timeout, kill the child rather than hanging the
    // test. `set_read_timeout` doesn't apply to `accept()` directly,
    // so we set the listener nonblocking and poll in short ticks.
    let payload = match read_ok_with_timeout(&listener, ACCEPT_TIMEOUT) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[smoke] {e}");
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket_path);
            return 72;
        }
    };
    eprintln!("[smoke] received {:?} from guest", payload);

    let exit = child.wait();
    let _ = std::fs::remove_file(&socket_path);

    let exit = match exit {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[smoke] wait() failed: {e}");
            return 73;
        }
    };
    eprintln!("[smoke] libkrun child exited: {exit:?}");

    if !payload.contains("ok") {
        eprintln!("[smoke] guest did not write 'ok' over vsock");
        return 72;
    }
    if !exit.success() {
        eprintln!("[smoke] libkrun child exited non-zero");
        return 73;
    }

    println!("PASS — libkrun booted, guest wrote 'ok' over vsock, child exited cleanly");
    0
}

fn read_ok_with_timeout(listener: &UnixListener, timeout: Duration) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking(true) failed: {e}"))?;

    let start = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                listener
                    .set_nonblocking(false)
                    .map_err(|e| format!("set_nonblocking(false) failed: {e}"))?;
                let mut buf = Vec::new();
                stream
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("read from guest failed: {e}"))?;
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() > timeout {
                    return Err(format!(
                        "timed out after {}s waiting for guest to connect",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("accept() failed: {e}")),
        }
    }
}

fn run_child() {
    let kernel = std::env::var(KERNEL_ENV).expect("child requires MVM_LIBKRUN_SMOKE_KERNEL");
    let rootfs = std::env::var(ROOTFS_ENV).expect("child requires MVM_LIBKRUN_SMOKE_ROOTFS");
    let socket = std::env::var(SOCKET_ENV).expect("child requires MVM_LIBKRUN_SMOKE_SOCKET");

    // The kernel cmdline is the W3.4 risk surface. libkrun's stdout
    // pipe is the implicit virtio-console (hvc0), so we explicitly
    // wire `console=hvc0`. `panic=1` makes kernel panics surface as
    // an immediate microVM exit instead of a hung VM. `loglevel=4`
    // keeps the early output verbose enough to debug stuck boots
    // without flooding the serial console.
    let cmdline = "console=hvc0 root=/dev/vda rw panic=1 loglevel=4";

    let ctx = KrunContext::new("smoke", kernel, rootfs)
        .with_resources(1, 256)
        .with_kernel_cmdline(cmdline)
        .add_vsock_listener(VSOCK_PORT, socket);

    eprintln!(
        "[smoke child] booting libkrun: kernel={} rootfs={}",
        ctx.kernel_path, ctx.rootfs_path
    );

    // `boot` returns `Result<Infallible, Error>` — the Ok arm is
    // uninhabited because libkrun's `krun_start_enter` `exit()`s the
    // process from inside the VMM on success. So we always observe
    // `Err`, and that means the configuration was rejected before
    // the microVM ever ran.
    let Err(e) = mvm_libkrun::boot(&ctx);
    eprintln!("[smoke child] mvm_libkrun::boot failed: {e}");
    std::process::exit(70);
}

fn resolve_artifact(env_var: &str, default_basename: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        return PathBuf::from(v);
    }
    // `nix build .#default` from inside examples/minimal/ writes a
    // `result/` symlink containing both vmlinux and rootfs.ext4.
    // From the workspace root, that's `examples/minimal/result/<name>`.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join("examples")
        .join("minimal")
        .join("result")
        .join(default_basename)
}

fn print_artifact_hint(kernel: &Path, rootfs: &Path) {
    eprintln!();
    eprintln!("libkrun-smoke: required artifacts are missing.");
    eprintln!();
    eprintln!("Checked:");
    eprintln!("  kernel: {}", kernel.display());
    eprintln!("  rootfs: {}", rootfs.display());
    eprintln!();
    eprintln!("Build them once with either of:");
    eprintln!();
    eprintln!("  # On a Linux host (or Linux remote builder):");
    eprintln!("  (cd examples/minimal && nix build .#default)");
    eprintln!();
    eprintln!("  # On a macOS host (uses the existing microsandbox builder VM):");
    eprintln!("  cargo xtask build-libkrun-smoke-image");
    eprintln!();
    eprintln!("Then re-run:");
    eprintln!("  cargo run --example libkrun-smoke --features libkrun-sys");
    eprintln!();
    eprintln!("Or point at pre-built artifacts directly:");
    eprintln!(
        "  {KERNEL_ENV}=<vmlinux> {ROOTFS_ENV}=<rootfs.ext4> cargo run --example libkrun-smoke --features libkrun-sys"
    );
}
