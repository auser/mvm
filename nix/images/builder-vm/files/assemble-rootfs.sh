#!/usr/bin/env bash
# Builder-VM rootfs tree assembly. Invoked from the runCommand at
# `../flake.nix::rootfsTree`.
#
# All paths arrive via environment variables set in the runCommand's
# attrset (Nix exports any string-valued attr as an env var of the
# same name). No Nix interpolation happens inside this file — the
# script is pure-bash so the flake stays slim and the assembly
# logic is reviewable as text.
#
# Required env vars:
#   out                  Build output directory (Nix sets this)
#   busybox              Static busybox derivation (`$busybox/bin/busybox`)
#   mvmBuilderInit       PID 1 binary derivation (`$mvmBuilderInit/sbin/mvm-builder-init`)
#   passwdFile           Path to ./files/etc/passwd
#   groupFile            Path to ./files/etc/group
#   nsswitchFile         Path to ./files/etc/nsswitch.conf
#   profileFile          Path to ./files/etc/profile
#   builderPackagePaths  Newline-separated list of package store paths to
#                        install into /usr/local/bin (their /bin/* entries).

set -euo pipefail

mkdir -p "$out"

# FHS dirs. /work, /job, /out are mount points the host attaches via
# virtio-fs (plan 72 W1 §Inner shape); /nix is shadowed at runtime by
# /dev/vdb (mvm-builder-init Stage 2).
mkdir -p "$out"/{bin,sbin,etc,proc,sys,dev,tmp,run,var,root,nix/store,nix/var/nix}
mkdir -p "$out"/{work,job,out}
chmod 1777 "$out/tmp"
chmod 0755 "$out/run"

# busybox + applet symlinks. Pre-baked so PID 1 has no first-boot
# setup step.
cp "$busybox/bin/busybox" "$out/bin/busybox"
chmod 0755 "$out/bin/busybox"
for applet in $("$busybox/bin/busybox" --list); do
  ln -sf /bin/busybox "$out/bin/$applet"
done

# mvm-builder-init at the kernel cmdline path. We do NOT bake /init
# or /sbin/init — a misconfigured cmdline then fails loudly with
# "init not found" instead of silently running the wrong thing.
cp "$mvmBuilderInit/sbin/mvm-builder-init" "$out/sbin/mvm-builder-init"
chmod 0555 "$out/sbin/mvm-builder-init"

# Static /etc files — copied byte-for-byte from ./files/etc/.
mkdir -p "$out/etc"
install -m 0644 "$passwdFile"   "$out/etc/passwd"
install -m 0644 "$groupFile"    "$out/etc/group"
install -m 0644 "$nsswitchFile" "$out/etc/nsswitch.conf"
install -m 0644 "$profileFile"  "$out/etc/profile"

# /etc/resolv.conf — empty placeholder. busybox udhcpc rewrites this
# atomically on first lease.
: > "$out/etc/resolv.conf"
chmod 0644 "$out/etc/resolv.conf"

# Slim package install — symlink each package's /bin/* under
# /usr/local/bin so they land on PATH alongside the busybox applets.
# Same pattern as mkGuest's loop.
mkdir -p "$out/usr/local/bin"
while IFS= read -r pkg; do
  [ -z "$pkg" ] && continue
  if [ -d "$pkg/bin" ]; then
    for bin in "$pkg"/bin/*; do
      [ -e "$bin" ] || continue
      ln -sf "$bin" "$out/usr/local/bin/$(basename "$bin")"
    done
  fi
done <<< "$builderPackagePaths"
