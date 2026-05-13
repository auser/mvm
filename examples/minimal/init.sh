#!/bin/sh
# Guest PID 1 for the libkrun smoke test (Plan 57 W3).
#
# The kernel hands control here; we mount the standard pseudo-filesystems
# so /dev/vsock and /proc are usable, run /bin/vsock_ok (which connects
# back to the host and writes "ok"), then orderly-shutdown by syncing
# disks and powering off via /proc/sysrq-trigger. We deliberately do NOT
# rely on `poweroff` or `reboot` from busybox — those expect a running
# init system; sysrq is the lowest-level path that works from a PID-1
# shell.
#
# Failure modes are all "best-effort": we never want to hang. If the
# vsock connect fails, we still power off so the smoke test sees the
# guest exit (and surfaces the error via the absence of "ok" rather
# than a hung VM).

set -u

# Mount the essentials. busybox' mount silently succeeds if a target
# is already populated, so re-mount races are harmless.
/bin/busybox mount -t proc     proc     /proc 2>/dev/null
/bin/busybox mount -t sysfs    sysfs    /sys  2>/dev/null
/bin/busybox mount -t devtmpfs devtmpfs /dev  2>/dev/null

echo "[init] mounted /proc /sys /dev"
echo "[init] kernel cmdline: $(/bin/busybox cat /proc/cmdline 2>/dev/null || echo '<unreadable>')"

# Run the vsock probe. Don't `exec` it — we need control back so we
# can power off regardless of the probe's exit code.
/bin/vsock_ok
echo "[init] vsock_ok exited rc=$?"

# Best-effort flush before halt.
/bin/busybox sync 2>/dev/null

# Trigger an immediate orderly poweroff via sysrq. We try the
# pre-poweroff sync ('s') first, then the actual poweroff ('o').
# If sysrq isn't compiled in, fall back to busybox poweroff -f.
if [ -w /proc/sysrq-trigger ]; then
    echo s > /proc/sysrq-trigger 2>/dev/null
    echo o > /proc/sysrq-trigger 2>/dev/null
fi

# Belt-and-suspenders: if sysrq didn't fire (kernel built without
# CONFIG_MAGIC_SYSRQ), force-poweroff via busybox.
/bin/busybox poweroff -f 2>/dev/null

# If we somehow reach here, loop forever so /init never returns
# (a returning /init panics the kernel and obscures the real error).
while :; do /bin/busybox sleep 60; done
