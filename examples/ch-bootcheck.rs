//! Plan 57 W3 / cross-platform — does cloud-hypervisor boot a Linux
//! kernel on this host?
//!
//! Sibling to `examples/libkrun-bootcheck.rs`. Same shape (point at any
//! pre-built `vmlinux + rootfs.ext4`, observe kernel output, classify
//! the outcome) but for cloud-hypervisor instead of libkrun. CH is a
//! standalone-binary VMM driven via REST API on a Unix socket; this
//! example skips the API surface and uses CH's plain CLI flags, which
//! is enough for a one-shot boot validation. The full CH-driving code
//! lives in `crates/mvm-backend/src/ch_runtime.rs`.
//!
//! Run (on a Linux host with KVM available + `cloud-hypervisor` on PATH):
//!
//! ```text
//! cargo run --example ch-bootcheck
//! ```
//!
//! No cargo feature gate — the example shells out to the
//! `cloud-hypervisor` binary, so it has no link-time dependency on
//! libkrun or any optional crate.
//!
//! Artifact discovery: same shape as libkrun-bootcheck. Defaults to
//! `nix/images/dev-prebuilt/<host-arch>/{vmlinux, rootfs.ext4}`; env
//! vars `MVM_CH_BOOTCHECK_KERNEL` / `MVM_CH_BOOTCHECK_ROOTFS` override.
//!
//! Exit codes mirror libkrun-bootcheck:
//!
//! - `0` — PASS: kernel reached userspace.
//! - `1` — PARTIAL: kernel booted, init/rootfs failed.
//! - `2` — FAIL: no kernel output observed.
//! - `74` — `cloud-hypervisor` not found on PATH.
//! - `75` — artifacts missing.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KERNEL_ENV: &str = "MVM_CH_BOOTCHECK_KERNEL";
const ROOTFS_ENV: &str = "MVM_CH_BOOTCHECK_ROOTFS";
const CMDLINE_ENV: &str = "MVM_CH_BOOTCHECK_CMDLINE";
const CH_BIN_ENV: &str = "MVM_CH_BIN";

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default kernel cmdline for cloud-hypervisor. CH's `--serial` flag
/// puts the boot console on ttyS0 (8250 UART model), so we tell the
/// kernel to log there. `panic=1` makes panics exit immediately
/// instead of hanging the VM. `loglevel=7` prints DEBUG-level
/// messages so a stuck boot still emits enough to diagnose.
const DEFAULT_CMDLINE: &str = "console=ttyS0 root=/dev/vda rw panic=1 loglevel=7";

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let ch_bin = std::env::var(CH_BIN_ENV).unwrap_or_else(|_| "cloud-hypervisor".to_string());
    if Command::new(&ch_bin).arg("--version").output().is_err() {
        eprintln!("ch-bootcheck: `{ch_bin}` not on PATH.");
        eprintln!();
        eprintln!("Install cloud-hypervisor:");
        eprintln!("  Linux (apt): sudo apt install cloud-hypervisor");
        eprintln!("  Or download: https://github.com/cloud-hypervisor/cloud-hypervisor/releases");
        eprintln!("Or set {CH_BIN_ENV}=<path> to point elsewhere.");
        return 74;
    }

    let kernel = resolve_artifact(KERNEL_ENV, "vmlinux");
    let rootfs = resolve_artifact(ROOTFS_ENV, "rootfs.ext4");
    if !kernel.is_file() || !rootfs.is_file() {
        eprintln!("ch-bootcheck: artifacts missing");
        eprintln!(
            "  kernel: {} ({})",
            kernel.display(),
            if kernel.is_file() { "ok" } else { "MISSING" }
        );
        eprintln!(
            "  rootfs: {} ({})",
            rootfs.display(),
            if rootfs.is_file() { "ok" } else { "MISSING" }
        );
        eprintln!();
        eprintln!("Defaults to nix/images/dev-prebuilt/<arch>/. Set");
        eprintln!("  {KERNEL_ENV}=<path>  {ROOTFS_ENV}=<path>");
        eprintln!("to point elsewhere.");
        return 75;
    }

    let serial_path =
        std::env::temp_dir().join(format!("mvm-ch-bootcheck-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&serial_path);
    std::fs::File::create(&serial_path).expect("create serial tempfile");

    let cmdline = std::env::var(CMDLINE_ENV).unwrap_or_else(|_| DEFAULT_CMDLINE.to_string());

    eprintln!("[ch-bootcheck] ch: {ch_bin}");
    eprintln!("[ch-bootcheck] kernel: {}", kernel.display());
    eprintln!("[ch-bootcheck] rootfs: {}", rootfs.display());
    eprintln!("[ch-bootcheck] cmdline: {cmdline}");
    eprintln!("[ch-bootcheck] serial -> {}", serial_path.display());
    eprintln!("[ch-bootcheck] timeout: {}s", BOOT_TIMEOUT.as_secs());

    // CH's `--serial file=PATH` routes the guest's ttyS0 to a host
    // file. `--console off` disables the secondary virtio-console so
    // we don't have two competing sinks. `--cpus boot=1 --memory
    // size=256M` matches libkrun-bootcheck's shape. `--disk
    // path=<rootfs>` exposes the ext4 as `/dev/vda` in the guest.
    let mut child = match Command::new(&ch_bin)
        .args([
            "--cpus",
            "boot=1",
            "--memory",
            "size=256M",
            "--kernel",
            kernel.to_str().expect("kernel path utf-8"),
            "--cmdline",
            &cmdline,
            "--disk",
            &format!("path={}", rootfs.display()),
            "--serial",
            &format!("file={}", serial_path.display()),
            "--console",
            "off",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ch-bootcheck] failed to spawn {ch_bin}: {e}");
            let _ = std::fs::remove_file(&serial_path);
            return 70;
        }
    };
    eprintln!("[ch-bootcheck] spawned cloud-hypervisor pid={}", child.id());

    let outcome = tail_for_boot_markers(&serial_path);

    // CH doesn't exit() the caller — it's a peer process. Always kill
    // explicitly. (Even on a successful guest poweroff, CH stays
    // running waiting for API commands; the boot-check pattern doesn't
    // talk to its API.)
    let _ = child.kill();
    let exit_status = child.wait().ok();

    let serial = std::fs::read_to_string(&serial_path).unwrap_or_default();
    let _ = std::fs::remove_file(&serial_path);

    eprintln!("\n──── serial tail (last 1024 bytes) ────");
    let tail = if serial.len() > 1024 {
        &serial[serial.len() - 1024..]
    } else {
        &serial
    };
    eprintln!("{tail}");
    eprintln!("──── end serial tail ────\n");

    eprintln!("[ch-bootcheck] child exit: {exit_status:?}");
    eprintln!("[ch-bootcheck] outcome: {outcome:?}");
    match outcome {
        Outcome::ReachedUserspace => {
            println!("PASS — cloud-hypervisor booted Linux to userspace on this host");
            0
        }
        Outcome::KernelBooted => {
            println!("PARTIAL — kernel booted; init/rootfs broke. cloud-hypervisor itself works.");
            1
        }
        Outcome::NoOutput => {
            println!(
                "FAIL — no kernel output observed. Cmdline / serial mis-routed, or CH never started the guest."
            );
            2
        }
    }
}

#[derive(Debug)]
enum Outcome {
    ReachedUserspace,
    KernelBooted,
    NoOutput,
}

fn tail_for_boot_markers(path: &PathBuf) -> Outcome {
    let start = Instant::now();
    let mut seen_kernel = false;
    let mut seen_panic = false;

    while start.elapsed() < BOOT_TIMEOUT {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.contains("Run /init as init process")
            || content.contains("Run /sbin/init as init process")
            || content.contains("Welcome to")
            || content.contains("/init started")
            || content.contains("BusyBox v")
            || content.contains("# ")
        {
            return Outcome::ReachedUserspace;
        }
        if content.contains("Linux version") || content.contains("Booting Linux") {
            seen_kernel = true;
        }
        if content.contains("Kernel panic") || content.contains("end Kernel panic") {
            seen_panic = true;
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    if seen_panic || seen_kernel {
        Outcome::KernelBooted
    } else {
        Outcome::NoOutput
    }
}

fn resolve_artifact(env_var: &str, default_basename: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        return PathBuf::from(v);
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join("nix")
        .join("images")
        .join("dev-prebuilt")
        .join(arch)
        .join(default_basename)
}
