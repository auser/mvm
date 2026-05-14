//! Plan 57 W3 / Plan 72 — cross-platform companion to
//! `crates/mvm-libkrun/examples/libkrun-smoke.rs`: validates that
//! cloud-hypervisor boots a Nix-built Linux kernel + ext4 rootfs on a
//! Linux host. This is the gate the `vmm-bootcheck` CI lane consumes
//! to prove the Linux side of the microsandbox-free artifact pipeline.
//!
//! Run (Linux host with KVM available + `cloud-hypervisor` on PATH —
//! `just setup-cloud-hypervisor` covers the install):
//!
//! ```text
//! cargo run --example ch-bootcheck -p mvm-backend
//! ```
//!
//! Artifact discovery: defaults to
//! `nix/images/dev-prebuilt/<host-arch>/{vmlinux, rootfs.ext4}`,
//! matching the in-repo dev-image layout. Override per-path with
//! `MVM_CH_BOOTCHECK_KERNEL` / `MVM_CH_BOOTCHECK_ROOTFS`. The CI
//! workflow builds the dev-image flake on the runner (host Nix, no
//! microsandbox) and points the env vars at the freshly built files.
//!
//! Exit codes:
//!
//! - `0` — PASS: VMM viable. Either the kernel reached userspace OR
//!   it booted far enough to panic at a later stage (kernel panic at
//!   `mount_root` still proves CH did its job). Set
//!   `MVM_CH_BOOTCHECK_STRICT=1` to require userspace markers.
//! - `1` — PARTIAL (strict-mode only): kernel booted but did not
//!   reach userspace.
//! - `2` — FAIL: no kernel output observed. Either CH rejected the
//!   config or the kernel never started.
//! - `74` — `cloud-hypervisor` not on PATH (install step missing).
//! - `75` — artifacts missing.
//!
//! No mvm-backend symbols are linked from here — the example shells
//! out to the standalone `cloud-hypervisor` binary so it doesn't
//! drag the backend's heavier deps into the CI job's build closure.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KERNEL_ENV: &str = "MVM_CH_BOOTCHECK_KERNEL";
const ROOTFS_ENV: &str = "MVM_CH_BOOTCHECK_ROOTFS";
const CMDLINE_ENV: &str = "MVM_CH_BOOTCHECK_CMDLINE";
const CH_BIN_ENV: &str = "MVM_CH_BIN";

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Default kernel cmdline. CH's `--serial file=…` routes the guest's
/// 8250 UART to the host file, so `console=ttyS0` is correct here.
/// `panic=1` makes panics exit fast; `loglevel=7` keeps DEBUG-level
/// messages visible so a stuck early boot still emits a usable trace.
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
        eprintln!("  just setup-cloud-hypervisor");
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
        eprintln!("to override.");
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

    // CH's stderr carries pre-boot rejection details (kernel format
    // mismatch, /dev/kvm EACCES, etc.) that don't reach the guest
    // serial file. Capture it so a NoOutput failure can be diagnosed
    // from the CI log alone — the first version of this example
    // discarded stderr and the failure mode was unclassifiable.
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
        .stdout(Stdio::piped())
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

    // CH stays running after the guest exits (waiting for API
    // commands); we never talk to its API here so always kill.
    let _ = child.kill();
    let exit_status = child.wait().ok();

    let mut ch_stdout = String::new();
    let mut ch_stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut ch_stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut ch_stderr);
    }

    let serial = std::fs::read_to_string(&serial_path).unwrap_or_default();
    let _ = std::fs::remove_file(&serial_path);

    if !ch_stdout.trim().is_empty() {
        eprintln!("\n──── cloud-hypervisor stdout ────");
        eprintln!("{}", ch_stdout.trim_end());
        eprintln!("──── end stdout ────");
    }
    if !ch_stderr.trim().is_empty() {
        eprintln!("\n──── cloud-hypervisor stderr ────");
        eprintln!("{}", ch_stderr.trim_end());
        eprintln!("──── end stderr ────");
    }

    eprintln!("\n──── guest serial tail (last 1024 bytes) ────");
    let tail = if serial.len() > 1024 {
        &serial[serial.len() - 1024..]
    } else {
        &serial
    };
    eprintln!("{tail}");
    eprintln!("──── end guest serial tail ────\n");

    eprintln!("[ch-bootcheck] child exit: {exit_status:?}");
    eprintln!("[ch-bootcheck] outcome: {outcome:?}");

    // The gate question is "is the VMM viable on this host?" —
    // KernelBooted (kernel reached boot diagnostics but rootfs/init
    // failed) is a VMM success. Strict mode tightens to require
    // userspace markers — used by smoke tests that also validate
    // the rootfs.
    let strict = std::env::var("MVM_CH_BOOTCHECK_STRICT").is_ok();
    match outcome {
        Outcome::ReachedUserspace => {
            println!("PASS — cloud-hypervisor booted Linux to userspace on this host");
            0
        }
        Outcome::KernelBooted if !strict => {
            println!(
                "PASS — cloud-hypervisor booted the Linux kernel (rootfs/init incomplete; \
                 not part of the VMM-viability gate)"
            );
            0
        }
        Outcome::KernelBooted => {
            println!(
                "PARTIAL — kernel booted but did not reach userspace. \
                 (strict mode set MVM_CH_BOOTCHECK_STRICT=1)"
            );
            1
        }
        Outcome::NoOutput => {
            println!(
                "FAIL — no kernel output observed. Cmdline / serial mis-routed, \
                 or CH never started the guest."
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
