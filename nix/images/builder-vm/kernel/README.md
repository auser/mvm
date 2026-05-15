# Builder-VM kernel — TSI patch series

The builder VM boots a stock nixpkgs Linux 6.12 kernel augmented
with libkrunfw's TSI (Transparent Socket Impersonation) patches.
Without these, libkrun's AF_INET-via-vsock egress path has no
guest-side counterpart and the in-guest `mvm-egress-proxy` can't
reach upstream.

Patches under `patches/` are vendored from
`github.com/containers/libkrunfw`, dual-licensed
LGPL-2.1-only AND GPL-2.0-only.

## Current pin

- **Upstream:** libkrunfw `v5.2.0`
- **Upstream kernel target:** Linux 6.12.68
- **Our kernel:** nixpkgs-25.11 `linuxPackages.kernel` (Linux 6.12.87)
- **New Kconfig:** `CONFIG_TSI=y` (added by patch 0009; the rest
  are code-only)

## Rebase procedure

Bumping libkrunfw or the kernel happens in three steps:

1. Identify the new patch set:

   ```bash
   gh api 'repos/containers/libkrunfw/contents/patches?ref=vX.Y.Z' \
     --jq '.[].name'
   ```

2. Replace the contents of `patches/`:

   ```bash
   git clone --depth 1 --branch vX.Y.Z \
     https://github.com/containers/libkrunfw.git /tmp/libkrunfw-vX.Y.Z
   rm -f patches/*.patch
   cp /tmp/libkrunfw-vX.Y.Z/patches/*.patch patches/
   ```

3. Verify patches still apply to the kernel nixpkgs ships in
   `linuxPackages.kernel`. Download the kernel source matching
   nixpkgs' pin (check `nix/flake.lock` → `nixpkgs.rev` →
   `pkgs/os-specific/linux/kernel/kernels-org.json`), extract,
   and run `patch -p1` sequentially:

   ```bash
   for p in patches/*.patch; do
     patch -p1 --dry-run < "$p" || { echo "FAIL: $p"; break; }
   done
   ```

   If any patch fails: either pin to a kernel version closer to
   libkrunfw's target, or rebase the affected hunks manually
   (preferred — keeps us on the LTS nixpkgs tracks).

4. Update `crates/mvm-libkrun/kernel-pins.toml` with the new
   kernel-bytes SHA-256 (computed from the flake-emitted
   `vmlinux`).

5. Bump the libkrunfw version pin in `crates/mvm-cli/src/doctor.rs`
   so `mvmctl doctor` warns on host-version drift.

## Why we vendor rather than depend on libkrunfw's bundled kernel

Two reasons, documented in ADR-046 §"Builder VM kernel + vendored
TSI patches":

1. **We own the kernel.** CVE backports, hardening, custom modules,
   and config changes happen on our timeline, not libkrunfw's.
2. **Determinism.** Our kernel is built deterministically from a
   flake input set we pin. The hash in `kernel-pins.toml` pins
   *that* build, not a third-party binary that ships with whatever
   host package manager produced.
