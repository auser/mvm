# Builder-VM kernel — stock nixpkgs Linux 6.12 + libkrunfw's TSI
# patch series, vendored in-tree.
#
# Why: libkrun relies on TSI (Transparent Socket Impersonation) to
# give the guest AF_INET egress without a userspace network stack
# on the host. TSI requires guest-side kernel patches that aren't
# upstream. Stock `pkgs.linuxPackages.kernel` boots fine but has
# no TSI, so the in-guest `mvm-egress-proxy` (Plan 73 Followup
# B.2.x) is dead code — its `TcpStream::connect()` finds no path
# off-VM.
#
# What: 22 patches under ./patches/ are vendored from
#   github.com/containers/libkrunfw @ v5.2.0
# (LGPL-2.1-only AND GPL-2.0-only — compatible with our use).
# libkrunfw v5.2.0 targets Linux 6.12.68; nixpkgs-25.11 ships
# 6.12.87 in the linux_6_12 LTS slot. Patches were verified to
# apply cleanly across the 19-patch-version gap (sequential
# `patch -p1` against a fresh 6.12.87 tree, no rejects).
#
# `CONFIG_TSI=y` is the one new kernel-config option introduced
# (by patch 0009 — `net/tsi/Kconfig`); the rest are code-only
# patches. `CONFIG_VSOCKETS` / `CONFIG_VIRTIO_VSOCKETS` are
# already in nixpkgs' default config.
#
# Rebase procedure: see ../README.md.

{ pkgs }:

let
  patchDir = ./patches;
  patchFiles = builtins.attrNames (builtins.readDir patchDir);
  tsiPatches = map (name: {
    inherit name;
    patch = patchDir + "/${name}";
  }) patchFiles;
in
pkgs.linuxPackages.kernel.override {
  kernelPatches = (pkgs.linuxPackages.kernel.kernelPatches or []) ++ tsiPatches;
  structuredExtraConfig = with pkgs.lib.kernel; {
    TSI = yes;
  };
}
