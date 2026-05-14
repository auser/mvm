# `mvm-builder-init` — PID 1 for the Plan 72 builder VM.
#
# Two-phase delivery, single contract surface:
#
#   Phase A (Plan 72 W2 — this file as written below):
#     Installs the POSIX shell script from `./mvm-builder-init.sh`
#     at `$out/sbin/mvm-builder-init`. busybox-resolved internally
#     (`#!/bin/busybox sh`, `BB=/bin/busybox`), so the rootfs at
#     `nix/images/builder-vm/` only needs busybox at the canonical
#     path — no Nix-store path leaks into the rootfs script.
#
#   Phase B (Plan 72 W3 — swapped in by changing the body here):
#     Replace the runCommand below with
#     `pkgs.rustPlatform.buildRustPackage` against the
#     `crates/mvm-builder-init/` crate. Same `$out/sbin/...` output
#     so the consuming flake doesn't change.
#
# Contract:
#   - `$out/sbin/mvm-builder-init` exists and is executable.
#   - `passthru.isStub` — true for the shell script, false for the
#     Rust binary. Surfaces in `manifest.json` so a consumer can tell
#     which variant they're booting.

{ pkgs }:

pkgs.runCommand "mvm-builder-init-stub-${pkgs.stdenv.hostPlatform.system}"
  {
    passthru = {
      isStub = true;
    };
  }
  ''
    install -D -m 0555 ${./mvm-builder-init.sh} $out/sbin/mvm-builder-init
  ''
