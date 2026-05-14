//! qemu boot smoke test for `mvm-builder-init` (plan 72 W3
//! acceptance criterion).
//!
//! Boots the builder-vm flake's kernel + rootfs.ext4 under
//! `qemu-system-aarch64` (or `qemu-system-x86_64` on x86) with:
//!
//!   - virtio-blk on /dev/vdb: a 5 MiB raw image (blank — init formats it)
//!   - virtio-fs on tag `mvm-job`: a host dir containing `cmd.sh` that
//!     runs `echo ok > /out/result.txt`
//!   - virtio-fs on tag `mvm-out`: an empty host dir to receive output
//!
//! Asserts:
//!   - boot-to-poweroff completes within 8s wall clock
//!   - /out/result.txt on the host contains `ok\n`
//!   - the host's job dir's `result` file contains `0\n`
//!
//! Status: **stub** — gated behind `#[ignore]` until plan 72 W2 lands
//! a built kernel + rootfs.ext4 the test can consume. The bare crate
//! `cargo build` (cross-compiled to aarch64-linux) is W3's primary
//! acceptance until then; this file documents the qemu fixture
//! contract so the reviewer of plan 72 W4 has a concrete target.
//!
//! When un-ignoring this, point `MVM_BUILDER_VM_IMAGE_DIR` at a
//! directory containing `vmlinux` + `rootfs.ext4` produced by
//! `nix build path:nix/images/builder-vm#packages.<system>.default`.

#![cfg(target_os = "linux")]

use std::env;
use std::path::PathBuf;

#[test]
#[ignore = "plan 72 W3 qemu fixture — requires built builder-vm image + qemu-system"]
fn boots_runs_job_and_powers_off() {
    let image_dir = match env::var("MVM_BUILDER_VM_IMAGE_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => panic!(
            "MVM_BUILDER_VM_IMAGE_DIR not set — point at a dir containing \
             vmlinux + rootfs.ext4 (from nix build nix/images/builder-vm#packages.<system>.default)"
        ),
    };

    let vmlinux = image_dir.join("vmlinux");
    let rootfs = image_dir.join("rootfs.ext4");
    assert!(
        vmlinux.exists() && rootfs.exists(),
        "{vmlinux:?} / {rootfs:?} not present — did the builder-vm flake build?"
    );

    // TODO(plan 72 W3 follow-on): wire qemu-system invocation here.
    // The full fixture (5 MiB virtio-blk + two virtiofsd backends +
    // exit-on-poweroff trapping) is non-trivial; landing it alongside
    // W4 (network + vsock plumbing) keeps the in-flight PR focused on
    // the init binary itself rather than the test rig.
}
