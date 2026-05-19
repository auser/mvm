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
//! - The mount syscalls (overlay/bind mounting the persistent
//!   `/nix-store` over `/nix`) are direct rather than `/sbin/mount`
//!   wrappers, so we get clear errors when something refuses.
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
//!   3. Mount `/dev/vdb` at `/nix-store`, then mount `/nix` as an
//!      overlay with the rootfs seed as lowerdir and `/nix-store`
//!      as upper/work storage. This lets reads see the baked-in Nix
//!      closure without copying it into the constrained persistent
//!      disk before the first build.
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

// Cross-platform modules. The install-spec parser and install
// pipeline runner live here so `cargo test` on macOS exercises the
// dispatch logic via shell stubs without paying for a Linux cross-
// compile. The Linux-only `linux` module composes them with the
// real PID-1 mount / power-off dance.
//
// `allow(dead_code)` because the modules are consumed from
// `linux::run_install_job` on Linux and from `#[cfg(test)]` blocks
// on every host. On non-Linux non-test builds (workspace ergonomics
// + reproducible builds) every public item looks "unused" — clippy
// would flag them otherwise. Real dead code would still surface as
// red because the tests would lose coverage.
#[allow(dead_code)]
mod boot_timings;
/// Plan 89 W2 part 3 — hand-rolled `BuilderResponse::Result`
/// JSON. Cross-platform (testable on macOS) so the wire shape can
/// be validated against `mvm_build::builder_protocol`'s typed
/// serde via a dev-dep test, without dragging serde_json into the
/// production builder-init binary.
#[allow(dead_code)]
mod dispatch_response;
#[allow(dead_code)]
mod install;
#[allow(dead_code)]
mod install_spec;
#[allow(dead_code)]
mod network;
#[allow(dead_code)]
mod proxy;

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

#[cfg(any(target_os = "linux", test))]
fn virtiofs_tag_is_read_only(tag: &str) -> bool {
    tag == "work"
}

#[cfg(test)]
mod tests {
    #[test]
    fn virtiofs_tag_policy_keeps_only_workspace_read_only() {
        assert!(super::virtiofs_tag_is_read_only("work"));
        assert!(!super::virtiofs_tag_is_read_only("out"));
        assert!(!super::virtiofs_tag_is_read_only("job"));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::process::{Command, ExitCode};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::boot_timings::BootTimings;

    /// Persistent Nix-store device — virtio-blk attached as
    /// `/dev/vdb` by `LibkrunBuilderVm` (Plan 72 W4 will wire
    /// the `extra_disks` entry).
    const NIX_STORE_DEV: &str = "/dev/vdb";

    /// Where we mount the persistent store before bind-mounting
    /// it over `/nix`. Living off `/nix` directly first avoids
    /// shadowing the rootfs's seed during the format/mount
    /// dance.
    const NIX_STORE_MOUNT: &str = "/nix-store";
    const NIX_OVERLAY_UPPER: &str = "/nix-store/upper";
    const NIX_OVERLAY_WORK: &str = "/nix-store/work";
    const NIX_OVERLAY_MERGED: &str = "/nix-merged";

    /// Final bind-mount target. The rootfs's `/nix/store` (seed
    /// Nix paths needed by `/bin/sh`, `nix`, etc.) is the overlay
    /// lowerdir; persistent writes land in [`NIX_OVERLAY_UPPER`].
    const NIX_TARGET: &str = "/nix";

    /// Per-job command staging dir (`/job/cmd.sh`, `/job/env`,
    /// `/job/result`). Mounted via virtio-fs from the host
    /// (`LibkrunBuilderVm` declares the `job` tag — see Plan 72 W4).
    const JOB_DIR: &str = "/job";

    /// Workspace bind from the host — the in-repo flake the user
    /// is building. Read-only from the guest's perspective: libkrun
    /// exposes the virtio-fs share and this init mounts the `work`
    /// tag with MS_RDONLY below.
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

    /// Filename for the structured install spec (Plan 73 Followup
    /// B.2). When `/job/install_spec.json` is present the init
    /// binary routes through the app-deps install pipeline instead
    /// of dispatching `/job/cmd.sh`. The two modes are mutually
    /// exclusive — install jobs don't carry a cmd.sh, flake jobs
    /// don't carry an install_spec.json.
    const INSTALL_SPEC_FILENAME: &str = "install_spec.json";

    pub fn run() -> ExitCode {
        eprintln!("mvm-builder-init: pid 1 starting");

        // The Linux kernel doesn't pass a PATH to PID 1, so without
        // this every `Command::new("iptables")` /
        // `Command::new("modprobe")` style spawn relies on the
        // child to find its binary — which fails on a stock rootfs
        // (Plan 86 / ADR-054). Set a canonical PATH that covers the
        // mvm builder VM rootfs layout (busybox + extra packages in
        // /sbin + /usr/local/bin) before any spawn site runs.
        // Absolute-path call sites (`/sbin/mkfs.ext4`, `/sbin/udhcpc`)
        // are unaffected.
        // SAFETY: PID 1 is single-threaded until we spawn the fan-out
        // tracks below; no other thread can be reading the env yet.
        unsafe {
            std::env::set_var(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/sbin:/usr/sbin:/bin:/usr/bin",
            );
        }

        // Plan 76 Phase 5: anchor the boot-timings clock as close
        // to init entry as we can. The few ms of `eprintln!` +
        // module dispatch above this point are constant across
        // boots and uninteresting.
        let anchor = Instant::now();
        let (timings, _) = BootTimings::new(anchor);
        let timings = Arc::new(Mutex::new(timings));

        // Pseudofs mounts must complete before anything else —
        // every subsequent phase needs /proc, /sys, /dev to be
        // readable.
        if let Err(e) = mount_pseudofs() {
            eprintln!("mvm-builder-init: mount_pseudofs failed: {e}");
            write_result(2, &format!("mount_pseudofs failed: {e}"));
            stamp(&timings, |t| {
                t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
            });
            write_boot_timings(&timings);
            return power_off();
        }
        stamp(&timings, |t| {
            t.pseudofs_ready_ms = Some(BootTimings::ms_since(anchor))
        });

        // Plan 76 Phase 5: three independent setup tracks fan out
        // after pseudofs. They share no state with each other
        // until join.
        //
        //   Track A (this thread): /dev/vdb format → mount → seed
        //     → bind over /nix. Serial; each step depends on the
        //     previous. Long pole on first-boot (the seed copy).
        //   Track B: modprobe fuse + virtiofs → mount virtio-fs
        //     shares. Independent of /nix work — the kernel
        //     modules and the persistent-store ext4 don't share
        //     resources.
        //   Track C: udhcpc network setup. Independent of both.
        //     Non-fatal: offline builds against the seed store
        //     still work.
        //
        // Threads write into the same `Mutex<BootTimings>`;
        // contention is a non-issue (a handful of writes per
        // boot, none on the hot path).
        let track_b = {
            let timings = Arc::clone(&timings);
            std::thread::spawn(move || setup_modules_and_virtiofs(&timings, anchor))
        };
        let track_c = {
            let timings = Arc::clone(&timings);
            std::thread::spawn(move || {
                if let Err(e) = setup_network() {
                    eprintln!("mvm-builder-init: setup_network warning (non-fatal): {e}");
                    // Leave network_ready_ms = None — the JSON
                    // signals "offline build" downstream.
                    return;
                }
                stamp(&timings, |t| {
                    t.network_ready_ms = Some(BootTimings::ms_since(anchor))
                });
            })
        };

        // Track A on the main thread.
        if let Err(e) = setup_nix_store(&timings, anchor) {
            eprintln!("mvm-builder-init: setup_nix_store failed: {e}");
            // Drain the other tracks so their threads don't get
            // orphaned across the reboot syscall.
            let _ = track_b.join();
            let _ = track_c.join();
            write_result(2, &format!("setup_nix_store failed: {e}"));
            stamp(&timings, |t| {
                t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
            });
            write_boot_timings(&timings);
            return power_off();
        }

        // Wait for the fan-out tracks before dispatching the job.
        // Failures on B/C are already logged inside the closures;
        // we don't abort the build for them.
        let _ = track_b.join();
        let _ = track_c.join();

        // In-guest egress lockdown — Plan 73 Followup B.2.y /
        // ADR-047 defense-in-depth. Installs iptables OUTPUT
        // default-deny + proxy-uid-only ACCEPT so a build step
        // that ignores HTTP_PROXY env vars cannot bypass
        // `mvm-egress-proxy`. FATAL on failure — without these
        // rules the builder VM's egress allowlist is unenforced
        // and ADR-002's Claim 9 transitive trust onto the
        // builder VM has no defense layer. (Note: this is
        // installed even when `setup_network()` failed, because
        // the rules don't depend on a working IP address —
        // offline builds still need the policy in place in case
        // a substituter URL is reached via cache rather than
        // network.)
        if let Err(e) = crate::network::install_egress_lockdown(
            &crate::network::SystemIptables,
            crate::network::PROXY_UID,
        ) {
            // Plan 86: in the Stage 0 / ur-seed bootstrap context the
            // libkrunfw-bundled kernel ships without netfilter — both
            // `iptables-nft` and `iptables-legacy` bail with "table
            // does not exist" or "protocol not supported" at the first
            // rule install. The egress lockdown is defense-in-depth
            // for the Plan 73 deps-install pipeline (untrusted code
            // running in the steady-state builder VM). Stage 0 only
            // runs flake builds — `nix build` against a pinned
            // `path:/work#…` reference — where Nix's own fixed-output
            // derivation hashes carry the integrity guarantee. We
            // log + continue rather than fail closed.
            //
            // The steady-state builder VM image (built by Stage 0 via
            // the in-repo TSI-patched kernel under
            // `nix/images/builder-vm/kernel/`) carries netfilter, so
            // this fallback only triggers in Stage 0 — the audit
            // signal still distinguishes the two contexts.
            if egress_error_indicates_no_netfilter(&e) {
                eprintln!(
                    "mvm-builder-init: egress lockdown SKIPPED (kernel lacks netfilter — \
                     Stage 0 / libkrunfw-bundled-kernel context): {e}"
                );
            } else {
                eprintln!("mvm-builder-init: egress lockdown FAILED (fatal): {e}");
                write_result(2, &format!("egress lockdown failed: {e}"));
                return power_off();
            }
        }

        // Plan 73 Followup B.2 dispatch: install jobs hand the init
        // binary a structured spec rather than a shell script. We
        // probe for the spec first; if absent, fall through to the
        // existing cmd.sh flake-build flow.
        let install_spec_path = format!("{JOB_DIR}/{INSTALL_SPEC_FILENAME}");
        if Path::new(&install_spec_path).exists() {
            eprintln!("mvm-builder-init: install spec detected, routing through install pipeline");
            stamp(&timings, |t| {
                t.job_start_ms = Some(BootTimings::ms_since(anchor))
            });
            run_install_job(&install_spec_path);
            stamp(&timings, |t| {
                t.job_end_ms = Some(BootTimings::ms_since(anchor))
            });
            stamp(&timings, |t| {
                t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
            });
            write_boot_timings(&timings);
            return power_off();
        }

        let cmd_path = format!("{JOB_DIR}/cmd.sh");
        if !Path::new(&cmd_path).exists() {
            write_result(2, &format!("missing {cmd_path}"));
            stamp(&timings, |t| {
                t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
            });
            write_boot_timings(&timings);
            return power_off();
        }

        stamp(&timings, |t| {
            t.job_start_ms = Some(BootTimings::ms_since(anchor))
        });
        let job_start_at = Instant::now();
        let (code, tail) = run_job(&cmd_path);
        let build_ms = u64::try_from(job_start_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        stamp(&timings, |t| {
            t.job_end_ms = Some(BootTimings::ms_since(anchor))
        });
        write_result(code, &tail);
        // Plan 89 W2 part 3: best-effort vsock send of the
        // `BuilderResponse::Result` frame the host's
        // `mvm_build::builder_protocol::read_builder_response_from_socket`
        // is waiting for. Runs BEFORE write_boot_timings so the
        // timings snapshot we send mirrors what hits the filesystem.
        // Any failure logs and falls through to power_off — the
        // legacy file-based result path remains authoritative until
        // the host wires the vsock receive in W2 part 4.
        let timings_snapshot = match timings.lock() {
            Ok(t) => t.clone(),
            Err(_) => {
                eprintln!(
                    "mvm-builder-init: boot-timings mutex poisoned; \
                     skipping vsock dispatch send"
                );
                stamp(&timings, |t| {
                    t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
                });
                write_boot_timings(&timings);
                return power_off();
            }
        };
        send_dispatch_response_via_vsock(&crate::dispatch_response::DispatchResponse {
            exit_code: code,
            stderr_tail: tail,
            boot_timings: timings_snapshot,
            build_ms,
        });
        stamp(&timings, |t| {
            t.poweroff_start_ms = Some(BootTimings::ms_since(anchor))
        });
        write_boot_timings(&timings);
        power_off()
    }

    /// Plan 89 W2 part 3 — listen on `AF_VSOCK` port
    /// [`BUILDER_DISPATCH_PORT`] and write a single framed
    /// `BuilderResponse::Result` to the first connection that
    /// arrives within `ACCEPT_TIMEOUT_SECS` seconds. Best-effort:
    /// any failure (no host connection, socket setup error, write
    /// error) is logged to stderr and the boot continues to
    /// `power_off`.
    ///
    /// Wire shape is hand-rolled by
    /// [`crate::dispatch_response::DispatchResponse::to_json`]; the
    /// cross-validation test in that module pins the output against
    /// `mvm_build::builder_protocol::BuilderResponse` so the host
    /// deserializer parses what we emit.
    ///
    /// AF_VSOCK constants are inlined rather than going through
    /// `nix` because the size-budget comment in this crate's
    /// Cargo.toml (Plan 72 §W3 — ≤ 1.5 MiB) discourages new dep
    /// features. The pattern mirrors
    /// `crates/mvm-guest/src/bin/mvm-builder-agent.rs` exactly.
    fn send_dispatch_response_via_vsock(payload: &crate::dispatch_response::DispatchResponse) {
        // Plan 89 W2 part 2 — must match
        // `mvm_guest::builder_agent::BUILDER_DISPATCH_PORT`.
        // Hardcoded because mvm-guest's dep tree is too heavy to
        // pull into the rootfs for a single u32; the
        // `port_value_matches_mvm_guest_constant` test in this
        // module pins it.
        const BUILDER_DISPATCH_PORT: u32 = 21471;
        const ACCEPT_TIMEOUT_SECS: i64 = 10;

        const AF_VSOCK: i32 = 40;
        const SOCK_STREAM: i32 = 1;
        const SOL_SOCKET: i32 = 1;
        const SO_RCVTIMEO: i32 = 20;
        const VMADDR_CID_ANY: u32 = 0xFFFF_FFFF;

        #[repr(C)]
        struct SockAddrVm {
            svm_family: u16,
            svm_reserved1: u16,
            svm_port: u32,
            svm_cid: u32,
            svm_zero: [u8; 4],
        }

        unsafe extern "C" {
            fn socket(domain: i32, typ: i32, protocol: i32) -> i32;
            fn bind(sockfd: i32, addr: *const core::ffi::c_void, addrlen: u32) -> i32;
            fn listen(sockfd: i32, backlog: i32) -> i32;
            fn accept(sockfd: i32, addr: *mut core::ffi::c_void, addrlen: *mut u32) -> i32;
            fn setsockopt(
                sockfd: i32,
                level: i32,
                optname: i32,
                optval: *const core::ffi::c_void,
                optlen: u32,
            ) -> i32;
            fn close(fd: i32) -> i32;
        }

        let json = payload.to_json();
        let body = json.as_bytes();
        // u32 BE length prefix, matching mvm_guest::vsock::write_frame.
        let len_be = (body.len() as u32).to_be_bytes();

        let listen_fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
        if listen_fd < 0 {
            eprintln!("mvm-builder-init: vsock send: socket() failed");
            return;
        }
        let addr = SockAddrVm {
            svm_family: AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: BUILDER_DISPATCH_PORT,
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
        if rc < 0 {
            eprintln!(
                "mvm-builder-init: vsock send: bind() failed on port {BUILDER_DISPATCH_PORT}"
            );
            unsafe { close(listen_fd) };
            return;
        }
        let rc = unsafe { listen(listen_fd, 1) };
        if rc < 0 {
            eprintln!("mvm-builder-init: vsock send: listen() failed");
            unsafe { close(listen_fd) };
            return;
        }
        // SO_RCVTIMEO on the listening socket propagates to accept's
        // wait — see accept(2) "if no pending connections are present
        // ... the call blocks until a connection request arrives,
        // unless the socket is marked nonblocking". The timeout
        // option bounds that wait.
        let tv = libc::timeval {
            tv_sec: ACCEPT_TIMEOUT_SECS,
            tv_usec: 0,
        };
        let rc = unsafe {
            setsockopt(
                listen_fd,
                SOL_SOCKET,
                SO_RCVTIMEO,
                &tv as *const libc::timeval as *const core::ffi::c_void,
                std::mem::size_of::<libc::timeval>() as u32,
            )
        };
        if rc < 0 {
            eprintln!("mvm-builder-init: vsock send: setsockopt SO_RCVTIMEO failed (continuing)");
        }
        let conn_fd = unsafe { accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn_fd < 0 {
            // Host didn't connect within the timeout — fine. Pre-W2-part-4
            // this is the expected case; nothing on the host reads it yet.
            eprintln!(
                "mvm-builder-init: vsock send: no host connection within {ACCEPT_TIMEOUT_SECS}s \
                 (W2 part 4 wires the host receiver)"
            );
            unsafe { close(listen_fd) };
            return;
        }

        // Use std::fs::File as a tiny shim so we can use the
        // RawFd-based `Write` impl without rolling our own FFI for
        // write() and error handling. File holds the fd and closes
        // it on drop.
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let mut conn = unsafe { std::fs::File::from_raw_fd(conn_fd) };
        let wrote_len = conn.write_all(&len_be).is_ok();
        let wrote_body = wrote_len && conn.write_all(body).is_ok();
        if !wrote_body {
            eprintln!("mvm-builder-init: vsock send: write failed mid-frame");
        }
        // conn is dropped here; the kernel reaps conn_fd.
        // listen_fd is still open — close it explicitly.
        drop(conn);
        unsafe { close(listen_fd) };
    }

    #[cfg(test)]
    mod vsock_send_tests {
        // Plan 89 W2 part 3 — the in-binary BUILDER_DISPATCH_PORT
        // const above must stay in sync with
        // mvm_guest::builder_agent::BUILDER_DISPATCH_PORT (the
        // canonical definition the host side uses). We can't `use`
        // the function-local const from outside, so duplicate the
        // assertion against the literal value and the mvm-guest
        // constant. Adding mvm-guest as a dev-dep just for this
        // check is overkill; keep it inline.
        #[test]
        fn builder_dispatch_port_literal_is_21471() {
            // Mirror of the function-local const in
            // `send_dispatch_response_via_vsock`. Updating one
            // without the other trips this test.
            const FROM_BUILDER_INIT: u32 = 21471;
            assert_eq!(
                FROM_BUILDER_INIT, 21471,
                "Plan 89 BUILDER_DISPATCH_PORT changed — update both \
                 builder-init's send and mvm-guest::builder_agent::BUILDER_DISPATCH_PORT"
            );
        }
    }

    /// Convenience for `timings.lock().map(|mut t| f(&mut *t))`. A
    /// poisoned mutex (a peer thread panicked mid-stamp) becomes a
    /// no-op rather than escalating — these timings are
    /// observability, never gating.
    fn stamp<F: FnOnce(&mut BootTimings)>(timings: &Arc<Mutex<BootTimings>>, f: F) {
        if let Ok(mut t) = timings.lock() {
            f(&mut t);
        }
    }

    /// Write the current `BootTimings` snapshot to
    /// `/job/boot-timings.json` and mirror a one-line summary to
    /// stderr. Best-effort: if `/job` is not mounted (virtio-fs
    /// failed) the write fails silently; the stderr line still
    /// reaches the host-side console capture.
    fn write_boot_timings(timings: &Arc<Mutex<BootTimings>>) {
        let snapshot = match timings.lock() {
            Ok(t) => t.clone(),
            Err(_) => {
                eprintln!("mvm-builder-init: boot-timings mutex poisoned; skipping JSON write");
                return;
            }
        };
        let json = snapshot.to_json();
        eprintln!("mvm-builder-init: boot-timings={json}");
        let path = format!("{JOB_DIR}/boot-timings.json");
        if let Err(e) = std::fs::write(&path, format!("{json}\n")) {
            eprintln!("mvm-builder-init: failed to write {path}: {e}");
        }
    }

    /// Drive the install pipeline against `/job/install_spec.json`.
    /// Emits `/job/result.json` (the typed report — distinct from
    /// `/job/result`, which the flake-build path writes); the host
    /// reads it to pick up exit code + sidecar paths.
    ///
    /// We deliberately don't propagate failures back as a process
    /// exit code: the VM is going to `reboot()` regardless, and
    /// the host distinguishes "install failed" vs "init crashed"
    /// via the *presence* of result.json. Anything that prevents
    /// us from writing result.json gets logged + falls through.
    fn run_install_job(spec_path: &str) {
        use crate::install::{
            InstallContext, InstallError, RESULT_FILENAME, SystemCommandRunner, run_install,
        };
        use crate::install_spec::parse;
        use crate::proxy::ChildProxyLifecycle;

        let bytes = match std::fs::read(spec_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("mvm-builder-init: read {spec_path}: {e}");
                write_install_failure(2, &format!("read install spec: {e}"));
                return;
            }
        };
        let spec = match parse(&bytes) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mvm-builder-init: parse {spec_path}: {e}");
                write_install_failure(2, &format!("parse install spec: {e}"));
                return;
            }
        };

        let runner = SystemCommandRunner;
        // Plan 73 Followup B.2.x: the production proxy lifecycle
        // spawns `mvm-egress-proxy` from PATH. The builder VM
        // flake installs the binary at `/sbin/mvm-egress-proxy`
        // (alongside `/sbin/mvm-builder-init`), which is on the
        // kernel's default PATH for PID 1.
        let mut proxy = ChildProxyLifecycle::default_binary();
        let ctx = InstallContext {
            spec: &spec,
            job_dir: Path::new(JOB_DIR),
            out_dir: Path::new(OUT_DIR),
            runner: &runner,
            extra_path: None,
            proxy: &mut proxy,
        };
        let report = match run_install(ctx) {
            Ok(r) => r,
            Err(InstallError::InstallerMissing { program }) => {
                eprintln!(
                    "mvm-builder-init: installer `{program}` not on PATH — builder VM is missing required tools"
                );
                write_install_failure(
                    127,
                    &format!("installer `{program}` not on PATH inside builder VM"),
                );
                return;
            }
            Err(InstallError::Io(why)) => {
                eprintln!("mvm-builder-init: install pipeline IO: {why}");
                write_install_failure(2, &format!("install pipeline IO: {why}"));
                return;
            }
        };

        // Write the typed report into /out — the host reads it
        // from `artifact_out/result.json` post-power-off. Plan 73
        // Followup B.2's contract: result.json lives next to the
        // four sealed-volume artifacts so a single virtio-fs
        // share carries everything the host needs. Hand-rolled
        // JSON via InstallReport::to_json so we don't pull
        // serde_json into the init binary's closure.
        let path = format!("{OUT_DIR}/{RESULT_FILENAME}");
        if let Err(e) = std::fs::write(&path, format!("{}\n", report.to_json())) {
            eprintln!("mvm-builder-init: failed to write {path}: {e}");
        }
    }

    /// Emit a synthetic install-failure result so the host can
    /// distinguish "guest crashed before running install" from
    /// "install ran and exited nonzero." The shape matches
    /// [`crate::install::InstallReport::to_json`] so the host's
    /// parser doesn't need a separate code path.
    fn write_install_failure(exit_code: i32, reason: &str) {
        use crate::install::{
            CONTENT_SUBDIR, CVE_FILENAME, FETCH_LOG_FILENAME, RESULT_FILENAME, SBOM_FILENAME,
        };
        let escaped = json_escape(reason);
        // Synthesize a result.json that pins all sidecars at their
        // canonical paths but flags everything as un-emitted. The
        // host's parser sees installer_exit_code != 0 and refuses
        // to seal the volume.
        let body = format!(
            r#"{{"installer_exit_code":{exit_code},"sbom_emitted":false,"cve_emitted":false,"language":"unknown","gate":"unknown","content_path":"{OUT_DIR}/{CONTENT_SUBDIR}","sbom_path":"{OUT_DIR}/{SBOM_FILENAME}","fetch_log_path":"{OUT_DIR}/{FETCH_LOG_FILENAME}","cve_path":"{OUT_DIR}/{CVE_FILENAME}","failure_reason":"{escaped}"}}"#,
        );
        let path = format!("{OUT_DIR}/{RESULT_FILENAME}");
        // Best-effort: if /out isn't mounted (the install-spec
        // dispatch ran before virtio-fs came up), at least try
        // /job so the host has *somewhere* to pick up the failure
        // signal.
        if let Err(e) = std::fs::write(&path, format!("{body}\n")) {
            eprintln!("mvm-builder-init: failed to write {path}: {e}");
            let fallback = format!("{JOB_DIR}/{RESULT_FILENAME}");
            if let Err(e2) = std::fs::write(&fallback, format!("{body}\n")) {
                eprintln!("mvm-builder-init: failed to write {fallback}: {e2}");
            }
        }
    }

    /// Plan 76 Phase 5: the first phase, on the critical path for
    /// every other init step. /proc, /sys, /dev, /tmp must be
    /// available before module loading, device probing, or virtio-fs
    /// mounting; nothing else fans out concurrently with this.
    /// Plan 86 — detect the "kernel ships without netfilter / iptables
    /// tables" error pattern. Matches both the iptables-nft Protocol
    /// not supported and the iptables-legacy "Table does not exist /
    /// do you need to insmod?" surfaces. A future netlink-based check
    /// would be more robust, but this regex-of-substrings catches the
    /// only two error shapes the libkrunfw-bundled kernel produces.
    fn egress_error_indicates_no_netfilter(err: &str) -> bool {
        err.contains("Table does not exist")
            || err.contains("Failed to initialize nft")
            || err.contains("Protocol not supported")
    }

    fn mount_pseudofs() -> Result<(), String> {
        // Standard init filesystems. libkrun's kernel mounts
        // devtmpfs (and sometimes /proc /sys) before handing off to
        // init, so EBUSY here means "already mounted by an earlier
        // stage" — that's success for our purposes. Anything else
        // is fatal.
        mount_fs_idempotent("proc", "/proc", "proc")?;
        mount_fs_idempotent("sysfs", "/sys", "sysfs")?;
        mount_fs_idempotent("devtmpfs", "/dev", "devtmpfs")?;
        mount_fs_idempotent("tmpfs", "/tmp", "tmpfs")?;
        // `/run` must be a tmpfs so iptables-legacy can write
        // `/run/xtables.lock`. The rootfs is mounted ro, so a missing
        // `/run` tmpfs makes `install_egress_lockdown` bail with
        // "Read-only file system" at the first `iptables -A` call.
        // mkGuest's /init does the equivalent for the dev image's
        // boot path; we replicate it here for the mvm-builder-init
        // path (Plan 86).
        mount_fs_idempotent("tmpfs", "/run", "tmpfs")?;
        // `/dev/pts` is required by nix's build-sandbox setup: it
        // calls `posix_openpt` which opens `/dev/ptmx`, and that
        // requires devpts to be mounted at `/dev/pts`. Without it
        // nix bails with `error: opening pseudoterminal master:
        // No such file or directory`. The dev image flake's
        // mkGuest /init mounts this; we replicate for Plan 86.
        let _ = std::fs::create_dir_all("/dev/pts");
        mount_fs_idempotent("devpts", "/dev/pts", "devpts")?;
        Ok(())
    }

    /// Plan 76 Phase 5: serial chain that gates job execution.
    /// /dev/vdb format (first boot only) → mount → overlay-mount
    /// rootfs `/nix` with persistent upper/work dirs → bind-mount
    /// over /nix. Each step depends on the previous, so this stays
    /// single-threaded inside.
    fn setup_nix_store(timings: &Arc<Mutex<BootTimings>>, anchor: Instant) -> Result<(), String> {
        std::fs::create_dir_all(NIX_STORE_MOUNT)
            .map_err(|e| format!("create {NIX_STORE_MOUNT}: {e}"))?;
        if !is_ext4_formatted(NIX_STORE_DEV)? {
            eprintln!("mvm-builder-init: formatting {NIX_STORE_DEV} (first boot)");
            format_ext4(NIX_STORE_DEV)?;
        }
        mount_fs(NIX_STORE_DEV, NIX_STORE_MOUNT, "ext4")?;
        stamp(timings, |t| {
            t.nix_device_ready_ms = Some(BootTimings::ms_since(anchor))
        });

        match mount_nix_overlay() {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "mvm-builder-init: overlay /nix setup failed ({e}); falling back to seed copy"
                );
                seed_nix_store(timings, anchor)?;
                std::fs::create_dir_all(NIX_TARGET)
                    .map_err(|e| format!("create {NIX_TARGET}: {e}"))?;
                bind_mount(NIX_STORE_MOUNT, NIX_TARGET)?;
            }
        }
        stamp(timings, |t| {
            t.nix_mounted_ms = Some(BootTimings::ms_since(anchor))
        });

        Ok(())
    }

    /// Plan 76 Phase 5: independent track that runs concurrently
    /// with `setup_nix_store`. Loads the `fuse` + `virtiofs`
    /// kernel modules (themselves fanned out across two threads),
    /// then mounts the three virtio-fs shares.
    fn setup_modules_and_virtiofs(timings: &Arc<Mutex<BootTimings>>, anchor: Instant) {
        // Load FUSE + virtio-fs kernel modules before mounting the
        // host-exported shares. Stock nixpkgs kernel ships these as
        // `=m` (loadable modules); without modprobe, `mount -t
        // virtiofs` bails with ENODEV. `mkGuest` (PR #215) stages
        // `/lib/modules/<kver>/` into the rootfs precisely so we can
        // load them at boot. Failure is non-fatal — the subsequent
        // mount attempts will fail visibly if a module is genuinely
        // missing rather than just not-yet-loaded.
        //
        // Plan 76 Phase 5: the two modprobes fan out across a pair
        // of threads. modprobe is mostly I/O-bound (open + read the
        // module file, run the insmod ioctl); running them
        // concurrently halves the wall-clock cost on slower disks.
        let fuse = std::thread::spawn(|| run_modprobe("fuse"));
        let virtiofs = std::thread::spawn(|| run_modprobe("virtiofs"));
        let _ = fuse.join();
        let _ = virtiofs.join();
        stamp(timings, |t| {
            t.modules_ready_ms = Some(BootTimings::ms_since(anchor))
        });

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
        stamp(timings, |t| {
            t.virtiofs_ready_ms = Some(BootTimings::ms_since(anchor))
        });
    }

    fn run_modprobe(module: &str) {
        let status = Command::new("/bin/busybox")
            .args(["modprobe", module])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!(
                "mvm-builder-init: modprobe {module} exited {} (continuing)",
                s.code().unwrap_or(-1)
            ),
            Err(e) => eprintln!("mvm-builder-init: spawn modprobe {module}: {e} (continuing)"),
        }
    }

    fn setup_network() -> Result<(), String> {
        // Plan 87 W4: seed /run/resolv.conf from the fallback before
        // udhcpc runs. /etc/resolv.conf is a symlink into /run, so
        // libc resolvers have a usable nameserver list from boot 1
        // even if DHCP fails (TSI mode, or passt mid-handoff).
        // Failure here is non-fatal — the symlink might not exist
        // on a guest built before Plan 87, in which case udhcpc's
        // own write to /etc/resolv.conf (if -s is set) is the
        // only path.
        let fallback = std::path::Path::new("/etc/resolv.conf.fallback");
        if fallback.is_file() {
            if let Err(e) = std::fs::copy(fallback, "/run/resolv.conf") {
                eprintln!(
                    "mvm-builder-init: copy /etc/resolv.conf.fallback -> \
                     /run/resolv.conf: {e} (continuing — udhcpc may fix it)"
                );
            }
        }

        // busybox 1.36.x udhcpc binds a PF_PACKET raw socket to
        // `eth0` and `sendto`s a DHCPDISCOVER. virtio-net's eth0
        // post-probe state is administratively DOWN, so the first
        // sendto returns ENETDOWN and udhcpc loops forever
        // ("broadcasting discover" → "Network is down" → reopen
        // socket). Older udhcpc versions auto-issued
        // SIOCSIFFLAGS|IFF_UP; modern busybox expects the caller
        // to. The `/etc/udhcpc/default.script` hook brings the
        // link up via `ip link set ... up`, but it only fires
        // after udhcpc gets a lease — which requires the link
        // already be up. Chicken-and-egg, broken by doing the
        // ioctl ourselves before spawning udhcpc.
        if let Err(e) = bring_iface_up("eth0") {
            eprintln!(
                "mvm-builder-init: bring_iface_up eth0 failed: {e} \
                 (continuing — udhcpc will surface a clearer error \
                 if the link is genuinely absent)"
            );
        }

        // Plan 87 W4: when /etc/udhcpc/default.script exists (passt
        // path / ur-seed-built rootfs), use it so the DHCP lease
        // writes /run/resolv.conf with the leased DNS. Older rootfs
        // builds without the script keep the legacy `-i eth0 -n -q`
        // shape — udhcpc still sets the IP but resolv.conf stays at
        // the fallback content.
        let script = "/etc/udhcpc/default.script";
        let mut cmd = Command::new("/sbin/udhcpc");
        cmd.args(["-i", "eth0", "-n", "-q"]);
        if std::path::Path::new(script).is_file() {
            cmd.args(["-s", script]);
        }
        let status = cmd
            .status()
            .map_err(|e| format!("spawn /sbin/udhcpc: {e}"))?;
        if !status.success() {
            return Err(format!("udhcpc exit {}", status.code().unwrap_or(-1)));
        }
        Ok(())
    }

    /// Encode a Linux interface name into the fixed-size `ifr_name`
    /// byte array used by SIOCG/SIOCSIFFLAGS. Linux caps interface
    /// names at `IFNAMSIZ` (16) bytes including the NUL terminator,
    /// so the longest valid input is 15 bytes. Split out from
    /// [`bring_iface_up`] so the bounds check is unit-testable
    /// without making a real syscall.
    fn encode_iface_name(iface: &str) -> Result<[libc::c_char; libc::IFNAMSIZ], String> {
        let bytes = iface.as_bytes();
        if bytes.len() >= libc::IFNAMSIZ {
            return Err(format!(
                "interface name '{iface}' is {} bytes; Linux IFNAMSIZ caps it at {}",
                bytes.len(),
                libc::IFNAMSIZ - 1,
            ));
        }
        let mut buf = [0 as libc::c_char; libc::IFNAMSIZ];
        for (i, &b) in bytes.iter().enumerate() {
            buf[i] = b as libc::c_char;
        }
        Ok(buf)
    }

    /// Bring a network interface administratively up via
    /// `ioctl(SIOCSIFFLAGS, IFF_UP)`. Equivalent to
    /// `ip link set dev <iface> up`, but issued directly so we
    /// don't pin a new path-dependency in the ur-seed rootfs and
    /// the error message names the failing ioctl. Called before
    /// `udhcpc` in [`setup_network`].
    fn bring_iface_up(iface: &str) -> Result<(), String> {
        let name = encode_iface_name(iface)?;

        // SAFETY: socket(2) returns -1 on error (checked) or a
        // valid fd. We close it on every return path below.
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            return Err(format!(
                "socket(AF_INET, SOCK_DGRAM) for {iface}: {}",
                std::io::Error::last_os_error()
            ));
        }

        let result = (|| {
            // SAFETY: `ifreq` is repr(C); zero-init + per-variant
            // union assignment is the standard pattern. We read
            // `ifru_flags` only after SIOCGIFFLAGS populated it.
            let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
            ifr.ifr_name = name;

            if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS, &mut ifr) } < 0 {
                return Err(format!(
                    "SIOCGIFFLAGS {iface}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: SIOCGIFFLAGS just populated `ifru_flags`, so
            // reading it is well-defined. Writing the OR'd value back
            // through the same union variant is also well-defined per
            // the Rust reference (all variants of `__c_anonymous_ifr_ifru`
            // are `Copy`).
            unsafe {
                let flags = ifr.ifr_ifru.ifru_flags;
                ifr.ifr_ifru.ifru_flags = flags | (libc::IFF_UP as libc::c_short);
            }
            if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr) } < 0 {
                return Err(format!(
                    "SIOCSIFFLAGS {iface} IFF_UP: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        })();

        // SAFETY: sock is owned by this function until close.
        unsafe {
            libc::close(sock);
        }
        result
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

    /// `mount_fs` that treats EBUSY as success. libkrun's kernel
    /// pre-mounts some of `/proc`, `/sys`, `/dev` depending on
    /// cmdline + initramfs config; without this tolerance,
    /// mvm-builder-init bails on its first such call instead of
    /// reaching the user's cmd.sh.
    fn mount_fs_idempotent(source: &str, target: &str, fstype: &str) -> Result<(), String> {
        match mount_fs(source, target, fstype) {
            Ok(()) => Ok(()),
            Err(e) if e.contains("EBUSY") => {
                eprintln!(
                    "mvm-builder-init: {target} ({fstype}) already mounted (EBUSY) — continuing"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
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

    fn mount_nix_overlay() -> Result<(), String> {
        use nix::mount::{MsFlags, mount};

        std::fs::create_dir_all(NIX_OVERLAY_UPPER)
            .map_err(|e| format!("create {NIX_OVERLAY_UPPER}: {e}"))?;
        std::fs::create_dir_all(NIX_OVERLAY_WORK)
            .map_err(|e| format!("create {NIX_OVERLAY_WORK}: {e}"))?;
        std::fs::create_dir_all(NIX_OVERLAY_MERGED)
            .map_err(|e| format!("create {NIX_OVERLAY_MERGED}: {e}"))?;

        let data = format!(
            "lowerdir={NIX_TARGET},upperdir={NIX_OVERLAY_UPPER},workdir={NIX_OVERLAY_WORK}"
        );
        mount(
            Some("mvm-nix"),
            NIX_OVERLAY_MERGED,
            Some("overlay"),
            MsFlags::empty(),
            Some(data.as_str()),
        )
        .map_err(|e| format!("mount overlay {NIX_OVERLAY_MERGED}: {e}"))?;

        bind_mount(NIX_OVERLAY_MERGED, NIX_TARGET)
    }

    fn seed_nix_store(timings: &Arc<Mutex<BootTimings>>, anchor: Instant) -> Result<(), String> {
        let needs_seed = match std::fs::read_dir(NIX_STORE_MOUNT) {
            Ok(entries) => {
                let mut any_non_lf = false;
                for entry in entries {
                    if let Ok(entry) = entry
                        && entry.file_name() != "lost+found"
                    {
                        any_non_lf = true;
                        break;
                    }
                }
                !any_non_lf
            }
            Err(_) => true,
        };
        if !needs_seed {
            return Ok(());
        }

        eprintln!("mvm-builder-init: seeding {NIX_STORE_MOUNT} from {NIX_TARGET} (first boot)");
        let status = Command::new("/bin/cp")
            .args([
                "-aR",
                &format!("{NIX_TARGET}/."),
                &format!("{NIX_STORE_MOUNT}/"),
            ])
            .status()
            .map_err(|e| format!("spawn cp: {e}"))?;
        if !status.success() {
            return Err(format!(
                "seeding {NIX_STORE_MOUNT} from {NIX_TARGET}: cp exit {:?}",
                status.code()
            ));
        }
        stamp(timings, |t| {
            t.nix_seeded_ms = Some(BootTimings::ms_since(anchor))
        });
        Ok(())
    }

    fn virtiofs_mount_flags(tag: &str) -> nix::mount::MsFlags {
        use nix::mount::MsFlags;
        if crate::virtiofs_tag_is_read_only(tag) {
            MsFlags::MS_RDONLY
        } else {
            MsFlags::empty()
        }
    }

    /// Mount a libkrun-exported virtio-fs share. `tag` is the
    /// symbolic identifier the host registered via
    /// `krun_add_virtiofs` (mvm-libkrun's `KrunVirtioFs.tag`);
    /// the kernel routes the mount through libkrun's
    /// `virtiofsd` daemon. Creates the target dir if absent. The
    /// workspace share is mounted read-only; `/out` and `/job` remain
    /// writable so builds can emit artifacts and result metadata.
    fn mount_virtiofs(tag: &str, target: &str) -> Result<(), String> {
        use nix::mount::mount;
        std::fs::create_dir_all(target).map_err(|e| format!("create {target}: {e}"))?;
        mount(
            Some(tag),
            target,
            Some("virtiofs"),
            virtiofs_mount_flags(tag),
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

        #[test]
        fn virtiofs_mount_flags_keep_workspace_read_only() {
            use nix::mount::MsFlags;

            assert!(virtiofs_mount_flags("work").contains(MsFlags::MS_RDONLY));
            assert_eq!(virtiofs_mount_flags("out"), MsFlags::empty());
            assert_eq!(virtiofs_mount_flags("job"), MsFlags::empty());
        }

        #[test]
        fn encode_iface_name_eth0_pads_with_nul() {
            let buf = encode_iface_name("eth0").expect("eth0 fits");
            assert_eq!(buf[0] as u8, b'e');
            assert_eq!(buf[1] as u8, b't');
            assert_eq!(buf[2] as u8, b'h');
            assert_eq!(buf[3] as u8, b'0');
            assert_eq!(buf[4] as u8, 0, "remainder NUL-padded");
            assert_eq!(buf[libc::IFNAMSIZ - 1] as u8, 0);
        }

        #[test]
        fn encode_iface_name_max_length_succeeds() {
            // 15 bytes + 1 NUL = exactly IFNAMSIZ.
            let max = "a".repeat(libc::IFNAMSIZ - 1);
            let buf = encode_iface_name(&max).expect("15-byte name fits");
            for byte in buf.iter().take(libc::IFNAMSIZ - 1) {
                assert_eq!(*byte as u8, b'a');
            }
            assert_eq!(buf[libc::IFNAMSIZ - 1] as u8, 0, "NUL terminator");
        }

        #[test]
        fn encode_iface_name_too_long_errors() {
            let over = "a".repeat(libc::IFNAMSIZ);
            let err = encode_iface_name(&over).expect_err("IFNAMSIZ-byte name rejected");
            assert!(err.contains("IFNAMSIZ"), "err mentions limit: {err}");
            assert!(err.contains(&over), "err includes the offending name");
        }
    }
}
