#!/bin/busybox sh
# mvm-builder-init — Plan 72 W2 stub PID 1 for the builder VM.
#
# This file is the W2 placeholder. Plan 72 W3 replaces it with a
# statically-linked Rust binary at `crates/mvm-builder-init/` exposed
# at the same `$out/sbin/mvm-builder-init` path so the consuming flake
# at `nix/images/builder-vm/` doesn't change.
#
# Behavior (matching W3's spec):
#
#   1. mount /proc /sys /dev /tmp /run (EBUSY tolerated)
#   2. format + mount /dev/vdb as ext4 → /nix-store, bind to /nix
#   3. bring up eth0 via udhcpc (non-fatal — offline builds are legal)
#   4. run /job/cmd.sh, capture exit code
#   5. write /job/result, reboot -f
#
# All paths assume the rootfs at `nix/images/builder-vm/flake.nix`:
# busybox at /bin/busybox, mkfs.ext4 at /usr/local/bin/mkfs.ext4 (from
# e2fsprogs in the builder package set), `/job` and `/out` as virtio-fs
# mount points the host attaches via `LibkrunBuilderVm` (plan 72 W1
# §Inner shape steps 3-4).

set -u
BB=/bin/busybox

# Helper — write {code} and {msg} to /job/result and poweroff.
# Every exit path goes through this so the host always sees a result
# file (not silence).
finish() {
  code="$1"; msg="$2"
  $BB mkdir -p /job 2>/dev/null
  printf '%s\n%s\n' "$code" "$msg" > /job/result 2>/dev/null || true
  $BB sync
  exec $BB reboot -f
}

# Stage 1 — kernel pseudofs. EBUSY (already mounted) is fine — micro-
# VMs occasionally get a partial pre-mount from the kernel.
$BB mount -t proc     proc     /proc 2>/dev/null || true
$BB mount -t sysfs    sysfs    /sys  2>/dev/null || true
$BB mount -t devtmpfs devtmpfs /dev  2>/dev/null || true
$BB mount -t tmpfs    tmpfs    /tmp -o mode=1777,nosuid,nodev 2>/dev/null || true
$BB mount -t tmpfs    tmpfs    /run -o mode=0755,nosuid,nodev 2>/dev/null || true

# Stage 2 — persistent Nix store on /dev/vdb. First boot finds an
# unformatted block device; later boots find ext4 already there. `blkid`
# reports the fs type; absence means "format me." When the device is
# absent entirely (qemu smoke without a /dev/vdb passthrough), skip the
# whole stage — Stage 4 still runs, just without persistence.
NIX_DEV=/dev/vdb
if [ -b "$NIX_DEV" ]; then
  FSTYPE=$($BB blkid -o value -s TYPE "$NIX_DEV" 2>/dev/null || echo "")
  FRESH_STORE=0
  if [ "$FSTYPE" != "ext4" ]; then
    # First boot: format. Path comes from e2fsprogs in the builder
    # package set (symlinked into /usr/local/bin by the rootfs
    # assembly).
    /usr/local/bin/mkfs.ext4 -q -L mvm-nix-store "$NIX_DEV" || \
      finish 10 "mkfs.ext4 failed on $NIX_DEV"
    FRESH_STORE=1
  fi
  $BB mkdir -p /nix-store
  $BB mount -t ext4 "$NIX_DEV" /nix-store || \
    finish 11 "mount $NIX_DEV /nix-store failed"

  # On a fresh /nix-store, seed it with the closure baked into the
  # rootfs at /nix (bash + coreutils + nix + the builder packages).
  # Subsequent boots skip the copy — the seed is on disk.
  if [ "$FRESH_STORE" = "1" ] && [ -d /nix/store ]; then
    $BB cp -a /nix/store /nix-store/ 2>/dev/null || true
    $BB mkdir -p /nix-store/var/nix
  fi

  # Bind the persistent store over the rootfs's /nix so writes land
  # on the host-backed virtio-blk.
  $BB mount --bind /nix-store /nix || \
    finish 12 "bind mount /nix-store -> /nix failed"
fi

# Stage 3 — network. udhcpc is busybox's DHCP client; failure is
# non-fatal because an offline build (NIX_CONFIG="substituters =")
# is a legitimate mode and PID 1 shouldn't fail closed on it.
$BB udhcpc -i eth0 -n -q -t 3 -T 2 >/dev/null 2>&1 || true

# Stage 4 — run the job. stdout/stderr go to /dev/console so the
# host's vsock console reader (plan 72 W4) sees live progress, not
# silence-then-dump.
JOB=/job/cmd.sh
if [ ! -f "$JOB" ]; then
  finish 2 "no /job/cmd.sh in builder VM"
fi
$BB sh -eu "$JOB" >/dev/console 2>&1
rc=$?

finish "$rc" "ok"
