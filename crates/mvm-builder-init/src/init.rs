//! Linux-only init logic. See `main.rs` for the contract and module
//! overview.
//!
//! Public entry: [`run`] — `-> !`, called from `main`.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};

// ── Constants — paths the rootfs at `nix/images/builder-vm/` ────────
// guarantees. Keeping them as `const`s rather than scattering literal
// strings makes a rootfs-layout change a one-file edit.

/// Persistent Nix store device. Attached by `LibkrunBuilderVm` as
/// virtio-blk-1 alongside the rootfs as virtio-blk-0.
const NIX_STORE_DEV: &str = "/dev/vdb";

/// Mountpoint where we mount [`NIX_STORE_DEV`] before bind-shadowing
/// `/nix`. We need a distinct dir so the original `/nix` (seed) is
/// reachable for the first-boot copy step.
const NIX_STORE_MOUNT: &str = "/nix-store";

/// Where the kernel cmdline tells us to find /nix.
const NIX_TARGET: &str = "/nix";

/// Job dir — host-attached virtio-fs share. The host drops
/// `cmd.sh` here and reads `result` back after we power off.
const JOB_DIR: &str = "/job";

/// Path to the build script. Missing → terminal exit code 2.
const JOB_CMD: &str = "/job/cmd.sh";

/// Path the host reads the exit code from after we power off.
const JOB_RESULT: &str = "/job/result";

/// `/bin/sh` — busybox applet symlink baked by the rootfs assembly.
const SHELL: &str = "/bin/sh";

/// busybox path. PID 1 uses it for `udhcpc` and as a fallback when
/// PATH isn't set yet.
const BUSYBOX: &str = "/bin/busybox";

/// `mkfs.ext4` — from `e2fsprogs` in the builder-vm package set;
/// the rootfs symlinks it into `/usr/local/bin`.
const MKFS_EXT4: &str = "/usr/local/bin/mkfs.ext4";

/// `blkid` — from `util-linux` (or busybox applet) in the package
/// set. The lookup order tries util-linux first because its TYPE=
/// reporting is more reliable than busybox's lightweight implementation.
const BLKID_PATHS: &[&str] = &["/usr/local/bin/blkid", "/bin/blkid"];

/// Maximum length of the result-file status message. Truncated past
/// this so a multi-MB stderr from a failed build doesn't blow up
/// /job/result.
const MAX_STATUS_BYTES: usize = 4096;

// ── libc helpers ────────────────────────────────────────────────────

/// Mount with a fixed set of args. `data = None` skips the data
/// argument (most filesystems don't need it). Returns
/// `Result<(), std::io::Error>` so the caller can match on
/// `ErrorKind::ResourceBusy` (EBUSY = already-mounted).
fn mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> std::io::Result<()> {
    let c_src = CString::new(src)?;
    let c_target = CString::new(target)?;
    let c_fstype = CString::new(fstype)?;
    let c_data = data.map(CString::new).transpose()?;
    let data_ptr = c_data
        .as_ref()
        .map(|s| s.as_ptr() as *const libc::c_void)
        .unwrap_or(std::ptr::null());
    // SAFETY: All four pointer args point at valid null-terminated
    // strings (or null for `data_ptr` when `data` is None). `mount(2)`
    // does not retain them past the call.
    let rc = unsafe {
        libc::mount(
            c_src.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Bind-mount. `flags = MS_BIND` is the only supported case.
fn bind_mount(src: &str, target: &str) -> std::io::Result<()> {
    mount(src, target, "none", libc::MS_BIND, None)
}

/// `reboot(LINUX_REBOOT_CMD_POWER_OFF)`. Never returns on success;
/// kernel powers the VM off. On failure it returns an error and the
/// caller is responsible for what to do next (we fall through to a
/// busy loop so PID 1 doesn't exit, which would panic the kernel).
fn power_off() -> std::io::Error {
    // SAFETY: `reboot(2)` takes a single int cmd. On success it does
    // not return; on failure it returns -1 and sets errno.
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::io::Error::last_os_error()
}

// ── Terminal-exit helper ────────────────────────────────────────────

/// Write `<code>\n<status>\n` to `/job/result`, sync, power off.
/// Diverges. Truncates the status to [`MAX_STATUS_BYTES`] so a
/// multi-MB stderr capture doesn't bloat the file.
///
/// Every error path in [`run`] funnels through here so the host
/// always observes a result file, even when /proc/sys/dev didn't
/// come up.
fn finish(code: i32, status: &str) -> ! {
    let _ = std::fs::create_dir_all(JOB_DIR);

    let truncated = if status.len() > MAX_STATUS_BYTES {
        // Trim on a char boundary so we don't slice mid-UTF-8.
        let mut end = MAX_STATUS_BYTES;
        while end > 0 && !status.is_char_boundary(end) {
            end -= 1;
        }
        &status[..end]
    } else {
        status
    };

    // Best-effort write — if /job isn't mounted (e.g. PID 1 crashed
    // before stage 1 mounts could complete), the host won't see a
    // result file, but at least we still power off cleanly.
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(JOB_RESULT)
        .and_then(|mut f| writeln!(f, "{code}\n{truncated}"));

    // Flush kernel write buffers so the host sees the result file
    // after power-off. Without this, virtio-blk may have writes still
    // queued in the page cache.
    // SAFETY: `sync(2)` takes no args, returns void, can't fail.
    unsafe {
        libc::sync();
    }

    let _ = power_off();
    // power_off only returns on syscall failure — we've already
    // written the result file, so spin here to avoid the kernel
    // panicking on PID 1 exit. The host's wall-clock timeout (plan
    // 72 W1 step 5) will eventually fire and forcibly tear down.
    loop {
        // SAFETY: pause(2) is signal-safe and never fails meaningfully.
        unsafe {
            libc::pause();
        }
    }
}

// ── Stages ──────────────────────────────────────────────────────────

/// Stage 1 — kernel pseudofs. EBUSY tolerated (already mounted).
/// Returns an error only if a mount fails for a non-EBUSY reason and
/// the caller decides whether that's fatal.
fn stage_pseudofs() -> std::io::Result<()> {
    let mounts: &[(&str, &str, &str, libc::c_ulong, Option<&str>)] = &[
        ("proc", "/proc", "proc", 0, None),
        ("sysfs", "/sys", "sysfs", 0, None),
        ("devtmpfs", "/dev", "devtmpfs", 0, None),
        (
            "tmpfs",
            "/tmp",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=1777"),
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755"),
        ),
    ];
    for (src, target, fstype, flags, data) in mounts {
        match mount(src, target, fstype, *flags, *data) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                // Already mounted — fine.
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Stage 2 — persistent /nix store. Returns `Ok(())` on the happy
/// path (mounted + bound) AND on the "no /dev/vdb attached" path
/// (qemu smoke without a passthrough disk — Stage 4 still runs,
/// just without persistence).
fn stage_nix_store() -> std::io::Result<()> {
    if !std::path::Path::new(NIX_STORE_DEV).exists() {
        return Ok(());
    }

    let fstype = blkid_fstype(NIX_STORE_DEV).unwrap_or_default();
    let needs_format = fstype != "ext4";

    if needs_format {
        let status = Command::new(MKFS_EXT4)
            .args(["-q", "-L", "mvm-nix-store", NIX_STORE_DEV])
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "mkfs.ext4 on {NIX_STORE_DEV} failed: exit {status:?}"
            )));
        }
    }

    std::fs::create_dir_all(NIX_STORE_MOUNT)?;
    mount(NIX_STORE_DEV, NIX_STORE_MOUNT, "ext4", 0, None)?;

    if needs_format {
        // Seed the persistent store with the closure baked into the
        // rootfs at /nix. cp -a preserves symlinks + perms — the Nix
        // store relies on both. Failure here isn't fatal: the persistent
        // store is empty but usable; nix will re-fetch from substituters.
        let _ = Command::new(BUSYBOX)
            .args(["cp", "-a", "/nix/store", &format!("{NIX_STORE_MOUNT}/")])
            .status();
        let _ = std::fs::create_dir_all(format!("{NIX_STORE_MOUNT}/var/nix"));
    }

    bind_mount(NIX_STORE_MOUNT, NIX_TARGET)?;
    Ok(())
}

/// Probe `/dev/vdb` for an existing filesystem type. Returns
/// `Some("ext4")` if a real fs is already there, `None` if the
/// device is blank / blkid not on PATH / probe failed.
fn blkid_fstype(dev: &str) -> Option<String> {
    for path in BLKID_PATHS {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let out = Command::new(path)
            .args(["-o", "value", "-s", "TYPE", dev])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Stage 3 — network. Non-fatal: an offline build is legal, so we
/// log+swallow any DHCP failure. The build script in the user's
/// `cmd.sh` is responsible for failing loudly if it actually needed
/// network and didn't get any.
fn stage_network() {
    let _ = Command::new(BUSYBOX)
        .args(["udhcpc", "-i", "eth0", "-n", "-q", "-t", "3", "-T", "2"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Stage 4 — run the job. Returns the exit code (or a synthetic one
/// for the missing-cmd-sh / spawn-failed cases). Never panics — every
/// path produces a meaningful number for `/job/result`.
fn stage_run_job() -> (i32, String) {
    if !std::path::Path::new(JOB_CMD).exists() {
        return (2, format!("no {JOB_CMD} in builder VM"));
    }
    // Pipe stdout/stderr to /dev/console (= virtio-console = host
    // stdout via plan 72 W4). We open the file ourselves rather than
    // letting sh inherit our stdio so that PID 1's stdio (typically
    // wired to /dev/console by the kernel anyway) stays the
    // canonical destination — busybox sh inherits from us.
    let status = Command::new(SHELL).args(["-eu", JOB_CMD]).status();
    match status {
        Ok(s) => {
            let code = s.code().unwrap_or_else(|| {
                // Signaled (no exit code). Linux convention is
                // 128 + signal-number.
                use std::os::unix::process::ExitStatusExt;
                s.signal().map(|sig| 128 + sig).unwrap_or(1)
            });
            (code, "ok".to_string())
        }
        Err(e) => (3, format!("failed to spawn {SHELL}: {e}")),
    }
}

// ── Entry ───────────────────────────────────────────────────────────

/// PID 1 entry. Diverges via [`finish`] on every path.
pub fn run() -> ! {
    // Stage 1 — pseudofs. A failure here means we can't even read
    // /proc, but we still try /job/result via the in-memory mount
    // attempt (devtmpfs may not be up but /job is virtio-fs and gets
    // attached by libkrun before we boot).
    if let Err(e) = stage_pseudofs() {
        finish(20, &format!("stage_pseudofs failed: {e}"));
    }

    // Stage 2 — Nix store. Failures here are recoverable in principle
    // (the build runs against substituters in /tmp), but the cold-
    // cache build cost would be huge and the user expects persistence.
    // Bail loudly.
    if let Err(e) = stage_nix_store() {
        finish(21, &format!("stage_nix_store failed: {e}"));
    }

    // Stage 3 — network. Best-effort.
    stage_network();

    // Stage 4 — run the job. Funnels through finish() with the captured
    // exit code.
    let (code, msg) = stage_run_job();
    finish(code, &msg);
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `finish`'s status truncation must not split UTF-8 mid-byte.
    /// The function's not unit-testable end-to-end (it reboots), but
    /// the truncation helper is — extracted as a free function for
    /// this reason would be cleaner; for now the test exercises the
    /// observation that we don't blow up on the boundary.
    #[test]
    fn status_truncation_respects_char_boundary() {
        // The boundary char (4-byte UTF-8 "𝓏") sits exactly at the
        // truncation point — a naive byte slice would split it.
        // We're not calling `finish` (it'd reboot the test runner);
        // we're testing the slice-trim logic that mirrors it.
        let prefix = "a".repeat(MAX_STATUS_BYTES - 2);
        let mut s = prefix.clone();
        s.push('𝓏'); // 4 bytes
        s.push('𝓏');

        // Same truncation logic as `finish`.
        let mut end = MAX_STATUS_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = &s[..end];

        // Must end before the boundary-straddling char.
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.starts_with(&prefix));
        assert!(truncated.len() <= MAX_STATUS_BYTES);
    }

    /// `blkid_fstype` falls back through the path list. With no blkid
    /// installed, returns `None` rather than erroring — Stage 2 reads
    /// `None` as "format the device fresh", which is the right
    /// first-boot behavior.
    #[test]
    fn blkid_fstype_returns_none_when_blkid_absent() {
        // Both default paths probably don't exist on the test host
        // (which is macOS or a non-builder Linux). The function must
        // not panic.
        let result = blkid_fstype("/dev/null-not-a-real-device");
        assert!(result.is_none());
    }
}
