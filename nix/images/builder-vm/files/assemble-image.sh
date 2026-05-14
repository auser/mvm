#!/usr/bin/env bash
# Builder-VM final image assembly. Invoked from the outer runCommand
# at `../flake.nix::mkBuilderVmImage`. Copies the kernel + rootfs.ext4
# into `$out`, emits the substituted `$out/manifest.json`, and
# enforces the W2 §Acceptance size budget.
#
# All paths + values arrive via environment variables. No Nix
# interpolation in this file — see ./assemble-rootfs.sh for the same
# pattern and rationale.
#
# Required env vars:
#   out                       Build output directory (Nix sets this)
#   kernelPkg                 Path to the kernel derivation
#   kernelFile                "Image" or "bzImage" (per-arch hint)
#   rootfsImage               Path to the make-ext4-fs.nix output
#                             (either a regular file or a directory
#                             containing *.img / *.ext4)
#   coreutils                 Path to GNU coreutils (sha256sum, stat, cut)
#   gnused                    Path to GNU sed (for placeholder substitution)
#   gnugrep                   Path to GNU grep (for the placeholder sanity check)
#   manifestTemplate          Path to ./files/manifest.template.json
#   templateSystem            "aarch64-linux" | "x86_64-linux"
#   templateKernelCmdline     The recommended cmdline burned into the manifest
#   templateInitIsStub        "true" or "false" — matches mvmBuilderInit.passthru.isStub

set -euo pipefail

mkdir -p "$out"

# Kernel copy with fallback ladder. Mirrors the existing logic in
# `nix/images/builder/flake.nix` because the produced kernel file
# name varies by configuration.
if [ -f "$kernelPkg/$kernelFile" ]; then
  cp "$kernelPkg/$kernelFile" "$out/vmlinux"
elif [ -f "$kernelPkg/Image" ]; then
  cp "$kernelPkg/Image" "$out/vmlinux"
elif [ -f "$kernelPkg/bzImage" ]; then
  cp "$kernelPkg/bzImage" "$out/vmlinux"
else
  echo "kernel package $kernelPkg did not produce Image or bzImage" >&2
  ls -la "$kernelPkg" >&2
  exit 1
fi

# Rootfs copy. make-ext4-fs.nix sometimes produces a directory with
# the .img inside (depending on nixpkgs version), sometimes a file
# directly — handle both.
if [ -f "$rootfsImage" ]; then
  cp "$rootfsImage" "$out/rootfs.ext4"
else
  img=$(find "$rootfsImage" -maxdepth 1 \( -name '*.img' -o -name '*.ext4' \) | head -1)
  if [ -z "$img" ]; then
    echo "make-ext4-fs output at $rootfsImage contained no image" >&2
    ls -la "$rootfsImage" >&2
    exit 1
  fi
  cp "$img" "$out/rootfs.ext4"
fi

chmod 0644 "$out/vmlinux" "$out/rootfs.ext4"

# manifest.json — populated by sed-substituting the template.
# Plan 72 W2 §Outputs spec.
VMLINUX_SHA=$("$coreutils/bin/sha256sum" "$out/vmlinux" | "$coreutils/bin/cut" -d' ' -f1)
ROOTFS_SHA=$("$coreutils/bin/sha256sum" "$out/rootfs.ext4" | "$coreutils/bin/cut" -d' ' -f1)
VMLINUX_SIZE=$("$coreutils/bin/stat" -c %s "$out/vmlinux")
ROOTFS_SIZE=$("$coreutils/bin/stat" -c %s "$out/rootfs.ext4")

"$gnused/bin/sed" \
  -e "s|@SYSTEM@|$templateSystem|g" \
  -e "s|@KERNEL_CMDLINE@|$templateKernelCmdline|g" \
  -e "s|@INIT_IS_STUB@|$templateInitIsStub|g" \
  -e "s|@VMLINUX_SHA@|$VMLINUX_SHA|g" \
  -e "s|@VMLINUX_SIZE@|$VMLINUX_SIZE|g" \
  -e "s|@ROOTFS_SHA@|$ROOTFS_SHA|g" \
  -e "s|@ROOTFS_SIZE@|$ROOTFS_SIZE|g" \
  "$manifestTemplate" > "$out/manifest.json"
chmod 0644 "$out/manifest.json"

# Sanity check — the template must have no unsubstituted placeholders
# left. Catches the case where a new field gets added to the template
# but not to the sed list above.
if "$gnugrep/bin/grep" -q '@[A-Z_]*@' "$out/manifest.json"; then
  echo "manifest.json still contains unsubstituted placeholders:" >&2
  "$gnugrep/bin/grep" -o '@[A-Z_]*@' "$out/manifest.json" >&2
  exit 1
fi

# W2 §Acceptance: rootfs.ext4 ≤ 1.2 GiB uncompressed. Hard fail in CI
# if exceeded so a misconfiguration that pulls in something heavy
# fails the build deterministically instead of bloating release
# artifacts silently. The compressed budget (≤ 300 MiB) is enforced
# by the release workflow's compression step.
MAX_UNCOMPRESSED=$((1200 * 1024 * 1024))
if [ "$ROOTFS_SIZE" -gt "$MAX_UNCOMPRESSED" ]; then
  echo "rootfs.ext4 is $ROOTFS_SIZE bytes; exceeds 1.2 GiB budget (plan 72 W2)" >&2
  exit 1
fi
