//! Plan 57 W3 — does libkrun boot a Linux kernel on this host at all?
//!
//! This example answers the W3.6 gate question in isolation from the
//! `examples/minimal` flake. It points libkrun at *any* pre-built
//! `vmlinux + rootfs.ext4` pair (defaults to the existing
//! `nix/images/dev-prebuilt/<arch>/` artifacts), redirects the
//! implicit virtio-console to a host tempfile, and tails that file
//! looking for kernel boot markers. The vsock signaling work that
//! `examples/libkrun-smoke.rs` does is deliberately NOT exercised
//! here — we're separating "libkrun-boots-Linux" from "our-guest-
//! payload-talks-vsock" so a failure tells us which half is broken.
//!
//! Run:
//!
//! ```text
//! cargo run --example libkrun-bootcheck --features libkrun-sys
//! ```
//!
//! Outcome categories (printed to stdout):
//!
//! - **kernel-reached-userspace** — saw "Run /init as init process",
//!   "Welcome to", or a getty/shell prompt marker. libkrun viable.
//! - **kernel-booted-but-init-failed** — saw "Linux version" and
//!   "Booting Linux" but kernel panic or no userspace markers.
//!   libkrun viable; rootfs/init mismatch.
//! - **kernel-started-but-no-output** — process ran for the timeout
//!   without writing to the console. Either kernel cmdline mis-
//!   directed console, or libkrun never got the kernel to start.
//! - **libkrun-rejected-config** — `krun_start_enter` returned an
//!   error before the VMM loop. Wrapper code or config bug.
//!
//! Artifact discovery: defaults to
//! `nix/images/dev-prebuilt/<host-arch>/{vmlinux, rootfs.ext4}`.
//! Override with `MVM_LIBKRUN_BOOTCHECK_KERNEL` /
//! `MVM_LIBKRUN_BOOTCHECK_ROOTFS`.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use mvm_libkrun::KrunContext;

const CHILD_ENV: &str = "MVM_LIBKRUN_BOOTCHECK_CHILD";
const KERNEL_ENV: &str = "MVM_LIBKRUN_BOOTCHECK_KERNEL";
const ROOTFS_ENV: &str = "MVM_LIBKRUN_BOOTCHECK_ROOTFS";
const CONSOLE_ENV: &str = "MVM_LIBKRUN_BOOTCHECK_CONSOLE";
const CMDLINE_ENV: &str = "MVM_LIBKRUN_BOOTCHECK_CMDLINE";

const BOOT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default cmdline. Tuned for libkrun's virtio-console + the typical
/// busybox-as-PID-1 rootfs the dev image ships. Override with
/// `MVM_LIBKRUN_BOOTCHECK_CMDLINE` to iterate the W3.6 risk surface.
const DEFAULT_CMDLINE: &str = "console=hvc0 root=/dev/vda rw panic=1 loglevel=7";

fn main() {
    mvm_providers::apple_container::ensure_signed();

    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        std::process::exit(70);
    }
    std::process::exit(run_parent());
}

fn run_parent() -> i32 {
    let kernel = resolve_artifact(KERNEL_ENV, "vmlinux");
    let rootfs = resolve_artifact(ROOTFS_ENV, "rootfs.ext4");
    if !kernel.is_file() || !rootfs.is_file() {
        eprintln!("bootcheck: artifacts missing");
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

    // Tempfile for the console redirect. We can't use NamedTempFile's
    // auto-cleanup because the child takes ownership of the path —
    // remove it manually at the end.
    let console_path =
        std::env::temp_dir().join(format!("mvm-libkrun-bootcheck-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&console_path);
    // Pre-create so the child's `krun_set_console_output` finds an
    // existing path to write into (libkrun opens with O_WRONLY|O_APPEND
    // — pre-creation is harmless either way).
    std::fs::File::create(&console_path).expect("create console tempfile");

    let cmdline = std::env::var(CMDLINE_ENV).unwrap_or_else(|_| DEFAULT_CMDLINE.to_string());

    eprintln!("[bootcheck] kernel: {}", kernel.display());
    eprintln!("[bootcheck] rootfs: {}", rootfs.display());
    eprintln!("[bootcheck] cmdline: {cmdline}");
    eprintln!("[bootcheck] console -> {}", console_path.display());
    eprintln!("[bootcheck] timeout: {}s", BOOT_TIMEOUT.as_secs());

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&exe)
        .env(CHILD_ENV, "1")
        .env(KERNEL_ENV, &kernel)
        .env(ROOTFS_ENV, &rootfs)
        .env(CONSOLE_ENV, &console_path)
        .env(CMDLINE_ENV, &cmdline)
        .spawn()
        .expect("spawn libkrun child");

    eprintln!("[bootcheck] spawned libkrun child pid={}", child.id());

    let outcome = tail_for_boot_markers(&console_path);

    // libkrun's child either calls `exit()` from inside the VMM (on a
    // successful guest poweroff) or stays blocked forever. We give it
    // a chance to exit naturally then kill if needed.
    std::thread::sleep(Duration::from_millis(200));
    let exit_status = match child.try_wait() {
        Ok(Some(s)) => Some(s),
        _ => {
            eprintln!("[bootcheck] child still running — killing");
            let _ = child.kill();
            child.wait().ok()
        }
    };

    let console = std::fs::read_to_string(&console_path).unwrap_or_default();
    let _ = std::fs::remove_file(&console_path);

    // Always dump the last KB of console so the user can see what
    // happened even when we classify it as "no output."
    eprintln!("\n──── console tail (last 1024 bytes) ────");
    let tail = if console.len() > 1024 {
        &console[console.len() - 1024..]
    } else {
        &console
    };
    eprintln!("{tail}");
    eprintln!("──── end console tail ────\n");

    eprintln!("[bootcheck] child exit: {exit_status:?}");
    eprintln!("[bootcheck] outcome: {outcome:?}");
    match outcome {
        Outcome::ReachedUserspace => {
            println!("PASS — libkrun booted Linux to userspace on this host");
            0
        }
        Outcome::KernelBooted => {
            println!("PARTIAL — kernel booted; init/rootfs broke. libkrun itself works.");
            1
        }
        Outcome::NoOutput => {
            println!(
                "FAIL — no kernel output observed. Cmdline / console mis-routed, or kernel never started."
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
        // Userspace markers — anything that proves /init ran.
        if content.contains("Run /init as init process")
            || content.contains("Welcome to")
            || content.contains("/init started")
            || content.contains("BusyBox v")
            || content.contains("# ")
        // shell prompt
        {
            return Outcome::ReachedUserspace;
        }
        // Kernel markers.
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

fn run_child() {
    let kernel = std::env::var(KERNEL_ENV).expect("child requires KERNEL");
    let rootfs = std::env::var(ROOTFS_ENV).expect("child requires ROOTFS");
    let console = std::env::var(CONSOLE_ENV).expect("child requires CONSOLE");
    let cmdline = std::env::var(CMDLINE_ENV).expect("child requires CMDLINE");

    let ctx = KrunContext::new("bootcheck", &kernel, &rootfs)
        .with_resources(1, 256)
        .with_kernel_cmdline(&cmdline)
        .with_console_output(&console);

    eprintln!("[bootcheck child] calling mvm_libkrun::boot");

    // Only the Err arm is reachable (krun_start_enter exits on success).
    let Err(e) = mvm_libkrun::boot(&ctx);
    eprintln!("[bootcheck child] boot() returned: {e}");
    std::process::exit(70);
}

fn resolve_artifact(env_var: &str, default_basename: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        return PathBuf::from(v);
    }
    // Default: nix/images/dev-prebuilt/<host-arch>/<file>
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

// Silence unused-import lints when libkrun-sys is off (the example
// requires the feature so this is informational, not actually reached).
#[allow(dead_code)]
fn _force_use(_: &mut Vec<u8>, _r: &mut dyn Read) {}
