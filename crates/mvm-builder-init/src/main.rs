//! `mvm-builder-init` — PID 1 inside the Plan 72 / ADR-046 builder
//! VM. This binary replaces the W2 shell-script stub at
//! `nix/packages/mvm-builder-init.sh`; both expose the same
//! `$out/sbin/mvm-builder-init` contract through their respective
//! Nix derivations so the consuming flake at
//! `nix/images/builder-vm/` does not change between the two.
//!
//! ## Scope
//!
//! The builder VM is single-shot: it boots, runs `/job/cmd.sh`,
//! writes the exit code + a short status string to `/job/result`,
//! and powers off. There is no vsock RPC, no privilege drop, no
//! integration manifest — those are workload concerns
//! (`mvm-guest-agent`), not builder concerns.
//!
//! ## Behavior contract (plan 72 W3 §Behaviour)
//!
//! 1. Mount kernel pseudofs (`/proc`, `/sys`, `/dev`, `/tmp`, `/run`).
//!    `EBUSY` (already-mounted) is tolerated — micro-VMs occasionally
//!    get a partial pre-mount from the kernel.
//! 2. Persistent Nix store on `/dev/vdb`. First boot finds the device
//!    unformatted; format ext4, mount at `/nix-store`, seed from the
//!    rootfs's `/nix/store` (the closure baked into the builder VM
//!    image). Later boots skip the format + copy.
//! 3. Bind-mount `/nix-store -> /nix` so subsequent writes land on
//!    the host-backed virtio-blk.
//! 4. Bring up `eth0` via `udhcpc`. Failure is non-fatal — an
//!    offline-build mode (`NIX_CONFIG="substituters ="`) is a legal
//!    operating mode and PID 1 must not fail closed on it.
//! 5. Read `/job/cmd.sh`. Missing → exit 2 with "no /job/cmd.sh".
//! 6. Run via `/bin/sh -eu /job/cmd.sh`. Pipe stdout/stderr to
//!    `/dev/console` so the host vsock console reader sees live
//!    progress.
//! 7. Write `<exit-code>\n<status>\n` to `/job/result`, sync, and
//!    `reboot(LINUX_REBOOT_CMD_POWER_OFF)`.
//!
//! Every terminal path goes through `finish()` so the host always
//! observes a result file rather than silence.
//!
//! ## Why libc directly, not the `nix` crate
//!
//! The crate's syscall surface is small (`mount`, `reboot`) and the
//! workspace already pins `libc` for workspace-wide use. The `nix`
//! crate would pull a non-trivial dep closure for two FFI calls.

// Non-Linux build: stub so the workspace compiles on macOS without
// dragging linux-only crates into the closure. mvm-builder-init has
// no business running anywhere but Linux PID 1; the message is
// load-bearing only when a contributor accidentally invokes the
// binary on their host.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "mvm-builder-init is a Linux PID 1 binary; \
         it is not callable from a host shell. See \
         specs/plans/72-builder-vm-via-libkrun.md §W3."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod init;

#[cfg(target_os = "linux")]
fn main() -> ! {
    init::run();
}
