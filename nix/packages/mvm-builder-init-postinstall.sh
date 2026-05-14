#!/usr/bin/env bash
# `postInstall` body for `mvm-builder-init.nix`. Runs after
# buildRustPackage's cargo install step in the same build env.
#
# `buildRustPackage` defaults the binary to `$out/bin/`. The builder
# VM's kernel cmdline expects `/sbin/mvm-builder-init`, and the
# consuming flake's rootfs assembly copies from `$out/sbin/`. Move
# it post-install so the contract surface stays the same as the W2
# shell-script stub.
#
# Required env var (set by the surrounding nix-shell env):
#   out   The package's output directory.

set -euo pipefail

mkdir -p "$out/sbin"
mv "$out/bin/mvm-builder-init" "$out/sbin/mvm-builder-init"
rmdir "$out/bin"
