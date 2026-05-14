# `mvm-builder-init` — PID 1 for the Plan 72 builder VM.
#
# **Phase B (this file as written below):** statically-linked Rust
# binary built from `crates/mvm-builder-init/` via `pkgsStatic`'s
# `buildRustPackage`. Output contract is unchanged from the W2
# shell-script stub: `$out/sbin/mvm-builder-init`, executable,
# self-contained (no /nix/store references in the binary so it
# runs inside the rootfs without a Nix store).
#
# **Phase A (W2 stub, retained at `./mvm-builder-init.sh` for the
# review trail):** POSIX shell script using busybox utilities.
# Functionally equivalent; useful while bootstrapping. Once the
# Rust binary is exercised end-to-end in the qemu smoke test (W2
# acceptance), the .sh file can be deleted in a follow-up cleanup.
#
# Contract:
#   - `$out/sbin/mvm-builder-init` exists and is executable.
#   - The binary is statically linked (no dynamic libc deps in the
#     rootfs's ld.so.cache, which doesn't exist).
#   - `passthru.isStub` — false here; the consuming flake's
#     `manifest.json` reads this so a release artifact honestly
#     labels which init it carries.
#
# Cross-compile note:
#   We use `pkgsStatic` so the crate's deps (`libc` and the std
#   library it links into) come from a musl + crt-static toolchain.
#   The host running `nix build` doesn't need to be Linux — Nix
#   composes the cross-compile via the same machinery as
#   `pkgs.pkgsCross.aarch64-multiplatform`. In practice the only
#   `nix build` site is the builder VM itself (running on Linux
#   inside the libkrun guest), so the cross dance is rare; the
#   release CI on Linux runners gets it natively.

{ pkgs }:

let
  # pkgsStatic exposes a static-musl toolchain. rustPlatform under
  # it produces binaries with `-C target-feature=+crt-static`,
  # which is what we need for PID 1 in a rootfs that has no /lib
  # directory.
  rustPlatform = pkgs.pkgsStatic.rustPlatform;
in
rustPlatform.buildRustPackage {
  pname = "mvm-builder-init";
  version = "0.14.0";

  # Workspace root. `buildRustPackage` vendors the full Cargo.lock
  # but only the named package compiles, so even though the workspace
  # has microsandbox + openssl-sys + etc. as siblings, the
  # mvm-builder-init build is tiny.
  src = ../..;

  cargoLock.lockFile = ../../Cargo.lock;

  cargoBuildFlags = [
    "--package" "mvm-builder-init"
    "--bin" "mvm-builder-init"
  ];

  # Skip cargo's test runner inside this derivation. The init's
  # behavior is exercised end-to-end by the qemu smoke test
  # (`crates/mvm-builder-init/tests/qemu_smoke.rs` — plan 72 W3
  # acceptance), and the unit tests inside the crate run in the
  # workspace's main `cargo test` lane on every PR.
  doCheck = false;

  # `buildRustPackage` defaults the binary to `$out/bin/`. The
  # builder VM's kernel cmdline expects `/sbin/mvm-builder-init`,
  # and the consuming flake's rootfs assembly copies from the
  # `$out/sbin/` path. Move it post-install so the contract surface
  # stays the same as the W2 shell-script stub. Script body lives in
  # ./mvm-builder-init-postinstall.sh.
  postInstall = "bash ${./mvm-builder-init-postinstall.sh}";

  passthru = {
    # W3-or-later marker. Consumed by `manifest.json` emission in
    # `nix/images/builder-vm/flake.nix`.
    isStub = false;
  };

  meta = with pkgs.lib; {
    description = "PID 1 inside the mvm builder VM (Plan 72 W3 — replaces the W2 shell-script stub).";
    homepage = "https://github.com/tinylabscom/mvm";
    license = licenses.asl20;
    platforms = platforms.linux;
  };
}
