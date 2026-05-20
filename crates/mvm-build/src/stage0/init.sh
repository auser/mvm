#!/bin/sh
# PID 1 of the Stage 0 bootstrap VM.
#
# Boots inside a libkrunfw kernel against an in-memory initramfs
# (built by `mvm_build::stage0::build_initramfs`). The initramfs
# carries busybox + nix-portable + this script. There is no rootfs
# ext4 disk: the kernel decompresses the initramfs to a tmpfs and
# runs `/init` (this file) directly.
#
# Job: build the in-repo `nix/images/builder-vm` flake into a
# kernel + rootfs.ext4 and write them to the `/out` virtio-fs share.
# When the host sees the artifacts land, it promotes them into the
# steady-state builder-vm cache and we're done with Stage 0 for the
# lifetime of this machine.

set -eu

# busybox installs itself as every applet via this command. The
# initramfs has /bin/busybox as a real file and every other tool
# (sh, mount, ip, udhcpc, …) as a symlink to it. `--install -s`
# materializes those symlinks under /bin so PATH lookups resolve
# without needing the host to pre-populate them.
/bin/busybox --install -s /bin

export PATH=/bin:/usr/local/bin

# Standard pseudofs essentials. libkrun's container mode (set_root)
# pre-mounts several of these in its in-VM init; gate each on
# `mountpoint -q` so the script doesn't trip `set -eu` on a
# benign EBUSY.
mountpoint -q /proc || mount -t proc     proc     /proc
mountpoint -q /sys  || mount -t sysfs    sysfs    /sys
mountpoint -q /dev  || mount -t devtmpfs devtmpfs /dev || true
mountpoint -q /tmp  || mount -t tmpfs    -o mode=1777 tmpfs /tmp
mountpoint -q /run  || mount -t tmpfs    -o mode=0755 tmpfs /run

# Bring eth0 up explicitly before udhcpc. busybox 1.36.x udhcpc
# does not auto-up the interface — sendto returns ENETDOWN and
# udhcpc loops forever. The libkrun virtio-net device is admin-DOWN
# at probe time; this ioctl flips it.
ip link set lo up
ip link set eth0 up

# DHCP. libkrun runs a DHCP server on its host side (gvproxy/passt
# leases an RFC1918 address). udhcpc gets the lease and writes
# /etc/resolv.conf via the default script. -n exits if no lease,
# -q exits after lease, -i pins the interface.
udhcpc -i eth0 -n -q || echo "stage0-init: udhcpc failed (offline; nix build will likely fail at substituter)" >&2

# nix-portable wants HOME for its self-extraction cache.
export HOME=/tmp/np-home
mkdir -p "$HOME"

# Virtio-fs shares from the host. In libkrun's set_root mode the
# kernel exposes them as virtio devices but the in-VM init does
# not mount them — the guest has to do it. Tags match the
# `add_virtio_fs(tag, ...)` calls in `LibkrunBuilderVm::run_stage0`.
mountpoint -q /work || mount -t virtiofs work /work
mountpoint -q /out  || mount -t virtiofs out  /out
if ! mountpoint -q /work; then
  echo "stage0-init: /work mount failed; aborting." >&2
  exit 64
fi
if ! mountpoint -q /out; then
  echo "stage0-init: /out mount failed; aborting." >&2
  exit 65
fi

ARCH="$(uname -m)"
FLAKE_REF="path:/work/nix/images/builder-vm#packages.${ARCH}-linux.default"

echo "stage0-init: building ${FLAKE_REF}" >&2

# nix-portable runs nix without requiring a /nix/store or nix-daemon
# on the guest. --no-write-lock-file lets us build against a
# read-only workspace mount. --print-out-paths spits the result path
# to stdout for us to copy from. --option connect-timeout 30 caps
# substituter HTTP connect waits at 30s so an unreachable mirror
# fails over fast instead of stalling the whole build behind the
# OS-default TCP timeout (~75-120s).
set +e
nix-portable nix build "$FLAKE_REF" \
    --extra-experimental-features "nix-command flakes" \
    --option connect-timeout 30 \
    --no-link --no-write-lock-file --impure \
    --print-out-paths --print-build-logs \
    > /tmp/store-path 2> /out/nix-stderr.log
NIX_RC=$?
set -e

if [ "$NIX_RC" -ne 0 ]; then
  echo "stage0-init: nix build exited $NIX_RC (see /out/nix-stderr.log)" >&2
  exit "$NIX_RC"
fi

NIX_OUT="$(cat /tmp/store-path)"
if [ -z "$NIX_OUT" ]; then
  echo "stage0-init: nix build emitted no /nix/store path" >&2
  exit 1
fi

# The flake output convention: $out/vmlinux and $out/rootfs.ext4.
# Tolerate the kernel being named Image or bzImage to match other
# flake conventions in the repo.
if   [ -f "$NIX_OUT/vmlinux" ]; then cp -L "$NIX_OUT/vmlinux" /out/vmlinux
elif [ -f "$NIX_OUT/Image"   ]; then cp -L "$NIX_OUT/Image"   /out/vmlinux
elif [ -f "$NIX_OUT/bzImage" ]; then cp -L "$NIX_OUT/bzImage" /out/vmlinux
else
  echo "stage0-init: no kernel in $NIX_OUT" >&2
  exit 1
fi
if [ ! -f "$NIX_OUT/rootfs.ext4" ]; then
  echo "stage0-init: no rootfs.ext4 in $NIX_OUT" >&2
  exit 1
fi
cp -L "$NIX_OUT/rootfs.ext4" /out/rootfs.ext4
[ -f "$NIX_OUT/cmdline.txt" ] && cp -L "$NIX_OUT/cmdline.txt" /out/cmdline.txt

sync
echo "stage0-init: done; halting" >&2
poweroff -f
