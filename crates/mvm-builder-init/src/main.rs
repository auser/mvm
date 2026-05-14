//! PID 1 for the mvm builder microVM (plan 72 W3 / ADR-046).
//!
//! The builder VM is an ephemeral appliance: the kernel boots, this
//! binary stages the build environment (persistent `/nix` store on
//! `/dev/vdb`, virtio-fs mounts for `/work`/`/out`/`/job`, eth0 DHCP),
//! execs `/job/cmd.sh`, writes the exit code to `/job/result`, then
//! powers the VM off. The host (`LibkrunBuilderVm`, plan 72 W1) reads
//! the artifacts in `/out` and the result code in `/job/result`.
//!
//! ## Why a Rust binary, not a shell PID 1
//!
//! ADR-046 §"Open questions" debated `mvm-builder-init` as either a
//! shell `/init` script (busybox) or a Rust binary. The plan picks
//! Rust because (a) the format-or-mount handshake on `/dev/vdb` is
//! easier to express correctly with libc syscalls than with a chain
//! of busybox applets that have to handle "is ext4 already" themselves,
//! (b) virtio-fs `mount(2)` flag plumbing is brittle in shell, and
//! (c) reboot semantics are direct — one `libc::reboot(RB_POWER_OFF)`
//! call instead of relying on a busybox applet's behaviour.
//!
//! ## Kernel cmdline contract (set by `LibkrunBuilderVm`)
//!
//!   console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/usr/local/bin/mvm-builder-init
//!
//! The plan-72 W2 spec named `/sbin/mvm-builder-init` as the init
//! path. `mkGuest`'s `packages` arg lands binaries at
//! `/usr/local/bin/<name>` via store-path symlinks; rather than
//! extend `mkGuest` with a separate sbin-install hook just for the
//! builder VM, we use the path `mkGuest` already produces. The
//! `cmdline` file emitted alongside `vmlinux` + `rootfs.ext4` carries
//! this exact string so the launcher and the rootfs agree.
//!
//! ## Virtio-fs tags (host-side `LibkrunBuilderVm::run_build`)
//!
//!   "mvm-work" → /work   (read-only bind of the workspace)
//!   "mvm-out"  → /out    (read-write artifact dir)
//!   "mvm-job"  → /job    (read-write, contains cmd.sh + env + result)
//!
//! All three are best-effort here: the qemu smoke test (plan 72 W3
//! acceptance) boots without virtio-fs and relies on the in-rootfs
//! versions of those dirs. Inside libkrun the host attaches them.
//!
//! ## Linux-only
//!
//! Builds as a stub on non-Linux targets so `cargo build --workspace`
//! on macOS still succeeds — the crate is in the workspace member
//! list and gets walked by every `cargo` invocation.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("mvm-builder-init: Linux-only PID 1; this build is a stub");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> ! {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::fs;
    use std::io;
    use std::os::unix::fs::FileExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    const NIX_STORE_DEV: &str = "/dev/vdb";
    const NIX_STORE_MOUNT: &str = "/nix-store";
    const NIX_TARGET: &str = "/nix";
    const SEED_MARKER_SUBDIR: &str = "store";
    const JOB_DIR: &str = "/job";
    const CMD_SCRIPT: &str = "/job/cmd.sh";
    const RESULT_FILE: &str = "/job/result";
    const POWEROFF_SETTLE_MS: u64 = 200;

    /// virtio-fs share tags. Must match host-side `LibkrunBuilderVm`.
    /// Kept as constants here so both halves see the same set when the
    /// host wiring lands in plan 72 W1.
    const VIRTIOFS_SHARES: &[(&str, &str)] = &[
        ("mvm-work", "/work"),
        ("mvm-out", "/out"),
        ("mvm-job", JOB_DIR),
    ];

    pub fn run() -> ! {
        msg("mvm-builder-init: starting");

        // Pseudofs first — nothing else is possible without /proc, /dev.
        // These are required: if they fail, panic + console-log + power
        // off so the host sees the failure rather than a stuck VM.
        require_mount("proc", "/proc", "proc", 0, "");
        require_mount("sysfs", "/sys", "sysfs", 0, "");
        require_mount("devtmpfs", "/dev", "devtmpfs", 0, "");
        require_mount("tmpfs", "/tmp", "tmpfs", 0, "mode=1777");

        // Persistent /nix store on /dev/vdb. Format-if-blank then mount
        // + bind. ensure_nix_store panics if /dev/vdb is present but
        // unrecoverable; with no virtio-blk attached, ensure_nix_store
        // logs + skips (qemu smoke test path).
        ensure_nix_store();

        // virtio-fs shares — best-effort. The qemu smoke fixture has
        // none of these attached, so failures are warnings.
        for (tag, target) in VIRTIOFS_SHARES {
            mount_virtiofs(tag, target);
        }

        // Network — best-effort. Offline builds (no eth0, or no DHCP
        // response) still proceed; nix substituters fail, which the job
        // script's exit code surfaces. Don't gate on this.
        bring_up_network();

        let exit_code = run_job();
        let _ = fs::write(RESULT_FILE, format!("{exit_code}\n"));
        msg(&format!(
            "mvm-builder-init: job exited {exit_code}, powering off"
        ));

        sync_and_poweroff();
    }

    // ──────────────────────── /nix store setup ─────────────────────────

    fn ensure_nix_store() {
        if !Path::new(NIX_STORE_DEV).exists() {
            // No virtio-blk attached (qemu smoke test or misconfigured
            // launch). Skip the persistent store; nix builds will write
            // to the rootfs's /nix/store and lose their cache on poweroff,
            // but the boot path still completes.
            msg(&format!(
                "mvm-builder-init: {NIX_STORE_DEV} absent — skipping persistent /nix"
            ));
            return;
        }

        if !ensure_dir(NIX_STORE_MOUNT) || !ensure_dir(NIX_TARGET) {
            // create_dir_all failed — log and skip the bind; nix builds
            // still work against the rootfs's /nix.
            return;
        }

        if !is_ext4(NIX_STORE_DEV) {
            msg("mvm-builder-init: /dev/vdb has no ext4 superblock, formatting");
            if run_status("/usr/local/bin/mkfs.ext4", &[
                "-F",
                "-L",
                "mvm-nix-store",
                NIX_STORE_DEV,
            ]) != 0
            {
                msg("mvm-builder-init: mkfs.ext4 failed — proceeding without persistent /nix");
                return;
            }
        }

        if let Err(e) = do_mount(NIX_STORE_DEV, NIX_STORE_MOUNT, "ext4", 0, "") {
            msg(&format!(
                "mvm-builder-init: mount {NIX_STORE_DEV} → {NIX_STORE_MOUNT} failed: {e}"
            ));
            return;
        }
        msg("mvm-builder-init: /dev/vdb mounted at /nix-store");

        // Seed-copy: first boot only. We test for /nix-store/store as
        // the marker because that's what a populated nix store always
        // contains; the seed copy writes /nix-store/store +
        // /nix-store/var on first boot, and the marker lets subsequent
        // boots skip the (~15s) copy.
        let seed_marker = format!("{NIX_STORE_MOUNT}/{SEED_MARKER_SUBDIR}");
        if !Path::new(&seed_marker).exists() {
            msg("mvm-builder-init: seeding /nix-store from rootfs /nix");
            // `/bin/cp` is a busybox applet symlink installed by mkGuest.
            // `-a` preserves perms + symlinks + timestamps; trailing `/.`
            // on the src copies directory *contents*, not the dir itself.
            if run_status("/bin/cp", &["-a", "/nix/.", NIX_STORE_MOUNT]) != 0 {
                msg("mvm-builder-init: warn: seed copy returned non-zero");
            }
        }

        if let Err(e) = do_mount(NIX_STORE_MOUNT, NIX_TARGET, "", libc::MS_BIND, "") {
            msg(&format!(
                "mvm-builder-init: bind {NIX_STORE_MOUNT} → {NIX_TARGET} failed: {e}"
            ));
            return;
        }
        msg("mvm-builder-init: /nix bind-mounted onto persistent volume");
    }

    /// Read the ext4 superblock magic (offset 0x438, 2 bytes LE).
    /// Returns false on any I/O error — the caller treats "unknown"
    /// the same as "blank" and re-formats.
    fn is_ext4(device: &str) -> bool {
        let Ok(f) = fs::File::open(device) else {
            return false;
        };
        let mut buf = [0u8; 2];
        if f.read_exact_at(&mut buf, 0x438).is_err() {
            return false;
        }
        u16::from_le_bytes(buf) == 0xEF53
    }

    // ──────────────────────── virtio-fs mounts ─────────────────────────

    fn mount_virtiofs(tag: &str, target: &str) {
        if !ensure_dir(target) {
            return;
        }
        // Probe for a virtio-fs device with this tag by attempting the
        // mount. The kernel returns ENODEV / EINVAL when no virtio-fs
        // backend offers the tag. We swallow both — qemu smoke + offline
        // boots have no shares attached and that's OK.
        match do_mount(tag, target, "virtiofs", 0, "") {
            Ok(()) => msg(&format!("mvm-builder-init: virtiofs {tag} mounted at {target}")),
            Err(e) => msg(&format!(
                "mvm-builder-init: virtiofs {tag} → {target} skipped: {e}"
            )),
        }
    }

    // ─────────────────────────── network ───────────────────────────────

    fn bring_up_network() {
        // `ip` and `udhcpc` are busybox applets installed by mkGuest at
        // /bin/<name>. We call by absolute path because the init's env
        // is empty (kernel sets only argv[0]).
        if run_status("/bin/ip", &["link", "set", "eth0", "up"]) != 0 {
            msg("mvm-builder-init: warn: ip link set eth0 up failed");
            return;
        }
        // -n: exit if no lease in -t tries
        // -q: quit after one lease
        // -t 10: try 10 times before giving up (~10s with default backoff)
        let rc = run_status("/bin/udhcpc", &[
            "-i", "eth0",
            "-n",
            "-q",
            "-t", "10",
        ]);
        if rc == 0 {
            msg("mvm-builder-init: eth0 up via udhcpc");
        } else {
            msg(&format!(
                "mvm-builder-init: warn: udhcpc exited {rc} — proceeding offline"
            ));
        }
    }

    // ─────────────────────────── job exec ──────────────────────────────

    fn run_job() -> i32 {
        if !Path::new(CMD_SCRIPT).exists() {
            msg(&format!("mvm-builder-init: no {CMD_SCRIPT}, exiting 2"));
            return 2;
        }
        // /bin/sh is a busybox applet symlink. `-e` aborts on error,
        // `-u` aborts on undefined-var reference — the same posture the
        // microsandbox builder script used (see plan 72 W0.3).
        match Command::new("/bin/sh").args(["-eu", CMD_SCRIPT]).status() {
            Ok(status) => exit_code_from_status(status),
            Err(e) => {
                msg(&format!("mvm-builder-init: spawn /bin/sh failed: {e}"));
                127
            }
        }
    }

    /// Map a unix `ExitStatus` to an i32 result code.
    /// - Normal exit → the program's exit code.
    /// - Killed by signal → 128 + signum (the conventional shell
    ///   encoding). Callers reading /job/result see e.g. 137 for SIGKILL.
    fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            return code;
        }
        128 + status.signal().unwrap_or(0)
    }

    // ─────────────────────────── poweroff ──────────────────────────────

    fn sync_and_poweroff() -> ! {
        unsafe {
            libc::sync();
        }
        // Brief settle so the console buffer flushes before the kernel
        // halts — without this, the last "powering off" message
        // sometimes doesn't make it to the host-side console capture.
        std::thread::sleep(Duration::from_millis(POWEROFF_SETTLE_MS));
        unsafe {
            libc::reboot(libc::RB_POWER_OFF);
        }
        // `reboot(RB_POWER_OFF)` either powers off or errors. If we
        // return, sleep forever so the kernel doesn't panic on PID 1
        // exit (which surfaces as "Kernel panic - not syncing: Attempted
        // to kill init!" and obscures the real failure on console).
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    // ───────────────────────── helpers ─────────────────────────────────

    fn require_mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) {
        if let Err(e) = do_mount(source, target, fstype, flags, data) {
            // Fatal: try to surface the failure on console then halt.
            // The kernel will panic on PID 1 exit if we just return.
            msg(&format!(
                "mvm-builder-init: FATAL: mount {source} → {target} ({fstype}): {e}"
            ));
            std::thread::sleep(Duration::from_millis(POWEROFF_SETTLE_MS));
            unsafe {
                libc::reboot(libc::RB_POWER_OFF);
            }
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    fn do_mount(
        source: &str,
        target: &str,
        fstype: &str,
        flags: libc::c_ulong,
        data: &str,
    ) -> Result<(), String> {
        let _ = fs::create_dir_all(target);
        let src = CString::new(source).map_err(|_| "source has NUL")?;
        let tgt = CString::new(target).map_err(|_| "target has NUL")?;
        let typ = CString::new(fstype).map_err(|_| "fstype has NUL")?;
        let dat = CString::new(data).map_err(|_| "data has NUL")?;
        let rc = unsafe {
            libc::mount(
                src.as_ptr(),
                tgt.as_ptr(),
                typ.as_ptr(),
                flags,
                dat.as_ptr().cast(),
            )
        };
        if rc != 0 {
            return Err(format!(
                "mount({source}→{target},{fstype}): {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn ensure_dir(path: &str) -> bool {
        match fs::create_dir_all(path) {
            Ok(()) => true,
            Err(e) => {
                msg(&format!("mvm-builder-init: create_dir_all({path}): {e}"));
                false
            }
        }
    }

    fn run_status(prog: &str, args: &[&str]) -> i32 {
        match Command::new(prog).args(args).status() {
            Ok(s) => exit_code_from_status(s),
            Err(e) => {
                msg(&format!(
                    "mvm-builder-init: spawn {prog} {args:?} failed: {e}"
                ));
                -1
            }
        }
    }

    fn msg(s: &str) {
        // Best-effort console write. /dev/console is created by
        // devtmpfs at boot before init runs; the first message logs
        // before we mount /dev so the write may silently fail on the
        // very first line — eprintln! covers the early window via the
        // kernel-attached stdio.
        let _ = fs::write("/dev/console", format!("{s}\n"));
        eprintln!("{s}");
    }

    // ──────────────────────── unit tests ───────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::ExitStatusExt;

        #[test]
        fn exit_code_normal_exit() {
            // ExitStatus::from_raw on Linux uses the waitpid encoding:
            // (status & 0xff00) >> 8 = exit code on a normal exit.
            let s = std::process::ExitStatus::from_raw(7 << 8);
            assert_eq!(exit_code_from_status(s), 7);
        }

        #[test]
        fn exit_code_zero() {
            let s = std::process::ExitStatus::from_raw(0);
            assert_eq!(exit_code_from_status(s), 0);
        }

        #[test]
        fn exit_code_signal() {
            // Signal 9 (SIGKILL): low 7 bits = signum; bit 7 = core-dump
            // (we don't care). The conventional shell encoding maps this
            // to 128 + signum = 137.
            let s = std::process::ExitStatus::from_raw(9);
            assert_eq!(exit_code_from_status(s), 137);
        }

        #[test]
        fn virtiofs_shares_match_host_contract() {
            // If this list changes, plan 72 W1's LibkrunBuilderVm must
            // attach the same tags or the guest can't see the shares.
            let tags: Vec<&str> = VIRTIOFS_SHARES.iter().map(|(t, _)| *t).collect();
            assert_eq!(tags, vec!["mvm-work", "mvm-out", "mvm-job"]);
        }
    }
}
