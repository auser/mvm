//! Plan 57 W4 — one-libkrun-guest-per-process supervisor.
//!
//! Reads a [`SupervisorConfig`] JSON document on stdin, ad-hoc
//! codesigns itself for `Hypervisor.framework` (macOS W2 gate),
//! creates the per-VM state directory, writes its own PID, then
//! calls [`run_supervisor`] which blocks in `krun_start_enter`
//! until the guest powers off.
//!
//! ## Why one process per VM
//!
//! `krun_start_enter` calls `exit()` on the calling process when
//! the guest exits cleanly. An in-process registry (plan 57 W4
//! Option A) would tear down every other libkrun guest the parent
//! `mvmctl` is supervising. One process per VM scopes the `exit()`
//! to a single supervisor; the parent `mvmctl` returns immediately
//! after spawning and survives a guest's shutdown.
//!
//! ## Usage (manual)
//!
//! ```sh
//! cat <<EOF | mvm-libkrun-supervisor
//! {
//!   "krun": {
//!     "name": "mvm-libkrun-smoke",
//!     "kernel_path": "/Users/me/.mvm/dev/current/vmlinux",
//!     "rootfs_path": "/Users/me/.mvm/dev/current/rootfs.ext4",
//!     "vcpus": 1,
//!     "ram_mib": 512,
//!     "kernel_cmdline": "console=hvc0 root=/dev/vda rw init=/init",
//!     "vsock_ports": [5252],
//!     "extra_disks": [],
//!     "console_output_path": "/tmp/mvm-libkrun-smoke.log",
//!     "vsock_socket_dir": "/Users/me/.mvm/vms/mvm-libkrun-smoke"
//!   },
//!   "vm_state_dir": "/Users/me/.mvm/vms/mvm-libkrun-smoke",
//!   "pid_file_name": null
//! }
//! EOF
//! ```
//!
//! Production callers (`LibkrunBackend::start()` — plan 57 W4.2)
//! produce the JSON programmatically and pipe it via
//! `std::process::Command::stdin`.

use std::io::Read;
use std::process::ExitCode;

use mvm_libkrun::{SupervisorConfig, run_supervisor};

fn main() -> ExitCode {
    // macOS Hypervisor.framework rejects any process without
    // `com.apple.security.hypervisor`. Plan 57 W2's ad-hoc signer
    // self-signs + re-spawns the binary on first run; subsequent
    // invocations are silent (`MVM_SIGNED=1`). Without this,
    // `krun_start_enter` fails at VM creation with rc -22.
    mvm_providers::apple_container::ensure_signed();

    let mut json = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut json) {
        eprintln!("error: read SupervisorConfig JSON from stdin: {e}");
        return ExitCode::from(2);
    }

    let cfg: SupervisorConfig = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: parse SupervisorConfig JSON: {e}");
            return ExitCode::from(2);
        }
    };

    // run_supervisor returns Result<Infallible, _>; on success
    // libkrun has already called exit() on this process.
    match run_supervisor(&cfg) {
        Err(e) => {
            eprintln!("supervisor failed: {e}");
            ExitCode::from(1)
        }
    }
}
