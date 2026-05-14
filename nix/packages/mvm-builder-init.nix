# `mvm-builder-init` — PID 1 for the builder microVM (plan 72 W3).
#
# Built from the workspace at `mvmSrc` via `rustPlatform.buildRustPackage`,
# selecting only the `mvm-builder-init` crate's bin so the workspace's
# heavy members (microsandbox, libkrun bindings, the full mvm CLI) don't
# enter the closure. The resulting binary lands at `$out/bin/mvm-builder-init`
# and is referenced from `nix/images/builder-vm/flake.nix` as one of the
# `packages` mkGuest installs into the rootfs — mkGuest's symlink loop
# puts it at `/usr/local/bin/mvm-builder-init`, which is the path the
# kernel cmdline's `init=` points at.
#
# Mirrors `nix/packages/mvm-guest-agent.nix`. The two could share a
# build-helper later but the size/feature flags differ enough that
# duplicating the small `buildRustPackage` call is cheaper than the
# abstraction.

{ pkgs
, lib
, mvmSrc
}:

pkgs.rustPlatform.buildRustPackage {
  pname = "mvm-builder-init";
  version = "0.14.0";

  src = mvmSrc;

  # Workspace lockfile is the source of truth — same as the guest-agent
  # build. `buildRustPackage` vendors the closure even though we only
  # build the one crate; unused deps compile zero code.
  cargoLock.lockFile = mvmSrc + "/Cargo.lock";

  # Single bin. The crate's main.rs is `#![cfg]`-gated to a Linux-only
  # implementation; on macOS it compiles to a stub that exits 1. Inside
  # this builder we always target Linux so the real impl is what gets
  # built.
  cargoBuildFlags = [
    "--package" "mvm-builder-init"
    "--bin" "mvm-builder-init"
  ];

  # Pure-logic unit tests (exit-status decoding, virtio-fs tag contract)
  # run by the workspace's `cargo test` lane. Skip here so the package
  # build stays focused on the binary itself.
  doCheck = false;

  meta = with lib; {
    description = "PID 1 for the mvm builder microVM (plan 72)";
    homepage = "https://github.com/tinylabscom/mvm";
    license = licenses.asl20;
    platforms = platforms.linux;
  };
}
