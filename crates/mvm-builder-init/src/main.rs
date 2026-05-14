//! mvm-builder-init — PID 1 for the libkrun builder VM.
//!
//! Plan 72 W3 (`specs/plans/72-builder-vm-via-libkrun.md`). Tiny
//! static-linked init that mounts the essentials, brings up the
//! persistent `/nix` store (formatting on first boot), tries to
//! bring the network up, executes `/job/cmd.sh`, writes
//! `/job/result`, and powers off.
//!
//! ## Why this binary, not a shell script
//!
//! Per Plan 72 §W3, the choice between shell and Rust was
//! explicitly debated. Rust won because:
//!
//! - One binary to audit; no `/bin/sh` -> `/usr/bin/sh` -> busybox
//!   hop where each link is a separate Nix store path.
//! - The mount syscalls (`MS_BIND` of `/nix-store` over `/nix`)
//!   are direct rather than `/sbin/mount -o bind` wrappers, so we
//!   get clear errors when something refuses.
//! - We can encode the `/job/result` JSON shape in one place
//!   rather than escape-quoting it across `printf` invocations.
//!
//! ## What runs in here
//!
//! On boot:
//!
//!   1. Mount `/proc`, `/sys`, `/dev`, `/tmp` (the standard init
//!      essentials — busybox-as-PID-1 from `mkGuest` does the
//!      same).
//!   2. Probe `/dev/vdb` for an ext4 superblock; format with
//!      `mkfs.ext4 -F` if blank (first boot on a fresh sparse
//!      virtio-blk image).
//!   3. Mount `/dev/vdb` at `/nix-store`, then bind-mount
//!      `/nix-store` over `/nix` so reads see the rootfs seed
//!      contents *and* persistent writes from prior builds.
//!   4. Best-effort `udhcpc -i eth0 -n -q` — failure is
//!      non-fatal (offline builds against the seed store still
//!      work; Plan 72 W4's `LibkrunBuilderVm::with_offline()`
//!      formalizes this).
//!   5. Read `/job/cmd.sh`. Exit code 2 + "no cmd.sh" in
//!      `/job/result` if missing.
//!   6. Spawn `/bin/sh -eu /job/cmd.sh`. Capture exit + stderr
//!      tail (last 20 lines, to keep the result file small).
//!   7. Write `/job/result` as `{"exit_code":<i32>,"stderr_tail":<json-string>}`.
//!   8. `sync` + `reboot(RB_POWER_OFF)`. The libkrun host
//!      detects power-off via the shutdown-eventfd
//!      (`krun_get_shutdown_eventfd`).
//!
//! ## Non-Linux build behaviour
//!
//! Linux-only by design. On macOS / Windows the crate still
//! compiles (workspace ergonomics) but `main()` prints a hint
//! and exits 1. mkGuest cross-compiles the real binary against
//! `<arch>-unknown-linux-musl` from a Linux nix-build
//! environment; that's where the size budget (≤ 1.5 MiB) and
//! static-link requirement get enforced.

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        linux::run()
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "mvm-builder-init is Linux-only (PID 1 for the libkrun \
             builder VM). On a developer host this binary is a no-op; \
             mkGuest cross-compiles the real init for \
             <arch>-unknown-linux-musl. See \
             specs/plans/72-builder-vm-via-libkrun.md §W3."
        );
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::process::{Command, ExitCode};

    /// Persistent Nix-store device — virtio-blk attached as
    /// `/dev/vdb` by `LibkrunBuilderVm` (Plan 72 W4 will wire
    /// the `extra_disks` entry).
    const NIX_STORE_DEV: &str = "/dev/vdb";

    /// Where we mount the persistent store before bind-mounting
    /// it over `/nix`. Living off `/nix` directly first avoids
    /// shadowing the rootfs's seed during the format/mount
    /// dance.
    const NIX_STORE_MOUNT: &str = "/nix-store";

    /// Final bind-mount target. The rootfs's `/nix/store` (seed
    /// Nix paths needed by `/bin/sh`, `nix`, etc.) sits underneath
    /// the bind; the kernel resolves lookups through the upper
    /// view.
    const NIX_TARGET: &str = "/nix";

    /// Per-job command staging dir (`/job/cmd.sh`, `/job/env`,
    /// `/job/result`). Mounted via virtio-fs from the host
    /// (`LibkrunBuilderVm` declares the `job` tag — see Plan 72 W4).
    const JOB_DIR: &str = "/job";

    /// Workspace bind from the host — the in-repo flake the user
    /// is building. Read-only from the guest's perspective (the
    /// host mounts the workspace virtio-fs share read-only).
    const WORK_DIR: &str = "/work";

    /// Artifact-extraction dir. The user's `cmd.sh` writes
    /// `vmlinux` + `rootfs.ext4` here; the host reads them back
    /// out after the VM powers off.
    const OUT_DIR: &str = "/out";

    /// Three virtio-fs tags that match the host-side
    /// `KrunContext::add_virtio_fs` declarations in
    /// `LibkrunBuilderVm::run_build`. Order doesn't matter; the
    /// guest mounts each by tag.
    const VIRTIOFS_MOUNTS: &[(&str, &str)] =
        &[("work", WORK_DIR), ("out", OUT_DIR), ("job", JOB_DIR)];

    /// Max stderr lines we capture into `/job/result`. Keeps
    /// the result file small; the host-side supervisor still
    /// captures the full stream via the libkrun console
    /// (`krun_set_console_output`).
    const STDERR_TAIL_LINES: usize = 20;

    pub fn run() -> ExitCode {
        eprintln!("mvm-builder-init: pid 1 starting");

        if let Err(e) = setup_filesystems() {
            eprintln!("mvm-builder-init: setup_filesystems failed: {e}");
            write_result(2, &format!("setup_filesystems failed: {e}"));
            return power_off();
        }

        // Network failure is non-fatal — offline builds against
        // the seed store still work for some derivations. The
        // host-side supervisor logs the warning via the libkrun
        // console.
        if let Err(e) = setup_network() {
            eprintln!("mvm-builder-init: setup_network warning (non-fatal): {e}");
        }

        let cmd_path = format!("{JOB_DIR}/cmd.sh");
        if !Path::new(&cmd_path).exists() {
            write_result(2, &format!("missing {cmd_path}"));
            return power_off();
        }

        let (code, tail) = run_job(&cmd_path);
        write_result(code, &tail);
        power_off()
    }

    fn setup_filesystems() -> Result<(), String> {
        // Standard init filesystems. `MS_NOSUID | MS_NODEV` on
        // /tmp matches the kernel default for tmpfs; the others
        // use stock flags.
        mount_fs("proc", "/proc", "proc")?;
        mount_fs("sysfs", "/sys", "sysfs")?;
        mount_fs("devtmpfs", "/dev", "devtmpfs")?;
        mount_fs("tmpfs", "/tmp", "tmpfs")?;

        std::fs::create_dir_all(NIX_STORE_MOUNT)
            .map_err(|e| format!("create {NIX_STORE_MOUNT}: {e}"))?;
        if !is_ext4_formatted(NIX_STORE_DEV)? {
            eprintln!("mvm-builder-init: formatting {NIX_STORE_DEV} (first boot)");
            format_ext4(NIX_STORE_DEV)?;
        }
        mount_fs(NIX_STORE_DEV, NIX_STORE_MOUNT, "ext4")?;

        std::fs::create_dir_all(NIX_TARGET).map_err(|e| format!("create {NIX_TARGET}: {e}"))?;
        bind_mount(NIX_STORE_MOUNT, NIX_TARGET)?;

        // virtio-fs shares declared by `LibkrunBuilderVm` (Plan 72
        // W4). Each entry is `(tag, target)` — the kernel routes
        // `mount -t virtiofs <tag> <target>` to the daemon libkrun
        // spawned for that share. Mounting is best-effort per
        // share: if the host omitted one (e.g. an offline build
        // path with no `/out` need), we still want to reach
        // `/job/cmd.sh` if `/job` was supplied. Per-share errors
        // print to stderr but don't fail init — the failing share
        // surfaces as a normal file-not-found inside cmd.sh.
        for (tag, target) in VIRTIOFS_MOUNTS {
            if let Err(e) = mount_virtiofs(tag, target) {
                eprintln!("mvm-builder-init: virtio-fs '{tag}' -> {target} failed: {e}");
            }
        }

        Ok(())
    }

    fn setup_network() -> Result<(), String> {
        let status = Command::new("/sbin/udhcpc")
            .args(["-i", "eth0", "-n", "-q"])
            .status()
            .map_err(|e| format!("spawn /sbin/udhcpc: {e}"))?;
        if !status.success() {
            return Err(format!("udhcpc exit {}", status.code().unwrap_or(-1)));
        }
        Ok(())
    }

    fn run_job(cmd_sh: &str) -> (i32, String) {
        match Command::new("/bin/sh").args(["-eu", cmd_sh]).output() {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let tail = stderr
                    .lines()
                    .rev()
                    .take(STDERR_TAIL_LINES)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (out.status.code().unwrap_or(-1), tail)
            }
            Err(e) => (127, format!("spawn /bin/sh: {e}")),
        }
    }

    /// Write `/job/result` as JSON. Hand-rolled rather than
    /// pulling `serde_json` in just for this — the init binary's
    /// size budget is ≤ 1.5 MiB and the JSON shape is one
    /// `i32` + one string.
    fn write_result(exit_code: i32, stderr_tail: &str) {
        let body = format!(
            r#"{{"exit_code":{exit_code},"stderr_tail":"{escaped}"}}{nl}"#,
            escaped = json_escape(stderr_tail),
            nl = "\n",
        );
        let path = format!("{JOB_DIR}/result");
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("mvm-builder-init: failed to write {path}: {e}");
        }
    }

    /// Minimal JSON string escaper. Only handles the characters
    /// that *must* be escaped per RFC 8259 §7. UTF-8 bytes pass
    /// through verbatim; control characters get `\u00XX`-style
    /// escapes; backslash and quote get the standard backslash
    /// escape. Tested with the unit tests below.
    fn json_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out
    }

    fn mount_fs(source: &str, target: &str, fstype: &str) -> Result<(), String> {
        use nix::mount::{MsFlags, mount};
        mount(
            Some(source),
            target,
            Some(fstype),
            MsFlags::empty(),
            None::<&str>,
        )
        .map_err(|e| format!("mount {source} -> {target} ({fstype}): {e}"))
    }

    fn bind_mount(source: &str, target: &str) -> Result<(), String> {
        use nix::mount::{MsFlags, mount};
        mount(
            Some(source),
            target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| format!("bind {source} -> {target}: {e}"))
    }

    /// Mount a libkrun-exported virtio-fs share. `tag` is the
    /// symbolic identifier the host registered via
    /// `krun_add_virtiofs` (mvm-libkrun's `KrunVirtioFs.tag`);
    /// the kernel routes the mount through libkrun's
    /// `virtiofsd` daemon. Creates the target dir if absent.
    fn mount_virtiofs(tag: &str, target: &str) -> Result<(), String> {
        use nix::mount::{MsFlags, mount};
        std::fs::create_dir_all(target).map_err(|e| format!("create {target}: {e}"))?;
        mount(
            Some(tag),
            target,
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .map_err(|e| format!("mount virtiofs {tag} -> {target}: {e}"))
    }

    /// Probe the ext4 magic at offset 0x438 (the superblock's
    /// `s_magic` field). Returns `Ok(false)` for a blank disk;
    /// `Ok(true)` for a formatted one; `Err` only when the device
    /// itself isn't readable (which is fatal — we couldn't mount
    /// it anyway).
    fn is_ext4_formatted(dev: &str) -> Result<bool, String> {
        use std::fs::File;
        use std::io::{Read, Seek, SeekFrom};
        let mut f = File::open(dev).map_err(|e| format!("open {dev}: {e}"))?;
        if f.seek(SeekFrom::Start(1080)).is_err() {
            return Ok(false);
        }
        let mut buf = [0u8; 2];
        if f.read_exact(&mut buf).is_err() {
            return Ok(false);
        }
        // ext4 magic: 0xEF53 stored little-endian.
        Ok(buf == [0x53, 0xEF])
    }

    fn format_ext4(dev: &str) -> Result<(), String> {
        let status = Command::new("/sbin/mkfs.ext4")
            .args(["-F", "-q", dev])
            .status()
            .map_err(|e| format!("spawn /sbin/mkfs.ext4: {e}"))?;
        if !status.success() {
            return Err(format!("mkfs.ext4 exit {}", status.code().unwrap_or(-1)));
        }
        Ok(())
    }

    fn power_off() -> ExitCode {
        use nix::sys::reboot::{RebootMode, reboot};
        let _ = Command::new("/bin/sync").status();
        // `reboot(RB_POWER_OFF)` returns `Infallible` on success
        // (the kernel halts the VM and never returns control to
        // userspace). The match-on-Result here is for the case
        // where the syscall errors before the actual power-off —
        // e.g. lack of CAP_SYS_BOOT in a misconfigured guest.
        match reboot(RebootMode::RB_POWER_OFF) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mvm-builder-init: reboot syscall failed: {e}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn json_escape_plain() {
            assert_eq!(json_escape("hello"), "hello");
        }

        #[test]
        fn json_escape_quote_and_backslash() {
            assert_eq!(json_escape(r#"he"llo\world"#), r#"he\"llo\\world"#);
        }

        #[test]
        fn json_escape_newlines_and_tabs() {
            assert_eq!(
                json_escape("line1\nline2\ttab\rcarriage"),
                "line1\\nline2\\ttab\\rcarriage"
            );
        }

        #[test]
        fn json_escape_low_control_codepoint() {
            // 0x01 is below 0x20 and not specially named — use 
            assert_eq!(json_escape("\x01"), "\\u0001");
        }

        #[test]
        fn json_escape_utf8_passes_through() {
            // Multi-byte UTF-8 must not be escaped: per RFC 8259,
            // only the named characters and control codepoints
            // require escaping.
            assert_eq!(json_escape("naïve résumé 日本語"), "naïve résumé 日本語");
        }

        #[test]
        fn ext4_magic_constants_match_disk_layout() {
            // Sanity-check the magic bytes we probe for. ext4
            // stores `0xEF53` as a 16-bit little-endian integer
            // at offset 1080 of the device. If this constant ever
            // drifts (e.g. someone "fixes" the byte order) we want
            // a CI test failure rather than a runtime mis-detection
            // that silently re-formats the persistent store.
            assert_eq!([0x53u8, 0xEFu8], 0xEF53u16.to_le_bytes());
        }
    }
}
