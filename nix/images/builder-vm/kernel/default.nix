# Builder-VM kernel — slim custom Linux 6.12.
#
# Built via `pkgs.linuxManualConfig` from a `.config` we generate
# inside this expression by running `make tinyconfig` on the
# kernel source, layering our `enables` / `disables` lists via
# `scripts/config`, and reconciling with `make olddefconfig`.
#
# Source of truth = the `enables` / `disables` lists below. No
# `.config` file is checked in.
#
# Why slim:
#   The stock `pkgs.linuxPackages.kernel` ships hundreds of `=m`
#   modules we never load. With a stock kernel, `mvm-builder-init`
#   has to modprobe each module it needs at the right time (overlay
#   before mount, vsock before socket(), iptables before rule
#   install) and the guest needs a `/lib/modules/<kver>/` tree
#   shipped alongside. Each `=m` is one more thing that can fail
#   silently. Slim build flips every feature we need to `=y`
#   (built-in); modprobes become no-ops, no module tree needed.
#
# Why no TSI patches:
#   Plan 87 / Plan 88 / ADR-055 moved builder-VM networking to
#   passt (Linux) / gvproxy (macOS) via virtio-net. The TSI
#   syscall-hijack path is no longer used in any builder VM.
#
# Why no `CONFIG_MODULES`:
#   `disables = [ "MODULES" ]` (plus disabling subsystems that
#   would otherwise force `=m`). Everything we need is `=y`. The
#   rootfs ships no `/lib/modules` tree; mkGuest's
#   `copy_module_closure` is unused for the builder-VM rootfs.

{ pkgs }:

let
  kernelArch =
    if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "x86_64";

  # Built-in features the builder VM requires. Each entry becomes a
  # `scripts/config --enable CONFIG_<name>` invocation. `make
  # olddefconfig` fills in transitive dependencies, so the list
  # below names only what we directly touch.
  enables = [
    # virtio bus + transport
    "VIRTIO" "VIRTIO_MENU" "VIRTIO_PCI" "VIRTIO_MMIO"
    "VIRTIO_BLK" "VIRTIO_NET" "VIRTIO_CONSOLE" "VIRTIO_FS"
    "VSOCKETS" "VIRTIO_VSOCKETS" "VIRTIO_BALLOON"
    "HW_RANDOM" "HW_RANDOM_VIRTIO"
    "PCI" "PCI_MSI"

    # filesystems
    "BLOCK" "EXT4_FS" "EXT4_USE_FOR_EXT2" "OVERLAY_FS" "FUSE_FS"
    "TMPFS" "TMPFS_POSIX_ACL" "TMPFS_XATTR"
    "DEVTMPFS" "DEVTMPFS_MOUNT" "PROC_FS" "SYSFS"

    # dm-verity (Plan 25 W3 / Claim 3 — verified boot of the rootfs)
    "MD" "BLK_DEV_DM" "DM_VERITY"

    # process basics
    "BINFMT_ELF" "BINFMT_SCRIPT" "FUTEX" "EPOLL" "SIGNALFD"
    "EVENTFD" "TIMERFD" "POSIX_MQUEUE" "SYSVIPC"
    "MULTIUSER" "SYSCTL" "PRINTK" "PRINTK_TIME" "KALLSYMS" "BUG"
    "RTC_CLASS" "HIGH_RES_TIMERS" "NO_HZ_IDLE"

    # namespaces + cgroups v2 + seccomp (nix build sandbox)
    "NAMESPACES" "UTS_NS" "IPC_NS" "USER_NS" "PID_NS" "NET_NS"
    "CGROUPS" "MEMCG" "BLK_CGROUP" "CGROUP_SCHED" "FAIR_GROUP_SCHED"
    "CGROUP_PIDS" "CGROUP_FREEZER" "CGROUP_DEVICE" "CGROUP_CPUACCT"
    "CPUSETS"
    "SECCOMP" "SECCOMP_FILTER"

    # net core
    "NET" "INET" "PACKET" "UNIX" "TCP_CONG_CUBIC"

    # iptables-legacy (egress lockdown, ADR-047 / Plan 73 B.2.y)
    "NETFILTER" "NETFILTER_ADVANCED" "NETFILTER_XTABLES"
    "NF_CONNTRACK" "NF_DEFRAG_IPV4"
    "IP_NF_IPTABLES" "IP_NF_FILTER" "IP_NF_TARGET_REJECT"
    "NETFILTER_XT_MATCH_OWNER"
    "NETFILTER_XT_MATCH_STATE" "NETFILTER_XT_MATCH_CONNTRACK"
    "NETFILTER_XT_MARK"

    # NLS (kept minimal — UTF-8 + ASCII only)
    "NLS" "NLS_UTF8" "NLS_ASCII"
  ];

  # Features tinyconfig leaves on (or that `make olddefconfig` would
  # propagate in) and we explicitly do not want.
  disables = [
    "MODULES"        # everything built-in; no /lib/modules tree
    "MODULE_SIG"     # NOP without MODULES; explicit
    "IPV6"           # builder VM has no v6 path
    "DRM" "SOUND" "USB" "WIRELESS" "BT" "FB"
  ];

  configfile = pkgs.runCommand "mvm-builder-vm-kernel-config" {
    nativeBuildInputs = with pkgs; [
      gnumake bison flex bc perl pkg-config openssl
    ];
    enableList = pkgs.lib.concatStringsSep " " enables;
    disableList = pkgs.lib.concatStringsSep " " disables;
  } ''
    set -euo pipefail

    mkdir -p linux
    tar -xf ${pkgs.linux_6_12.src} -C linux --strip-components=1
    cd linux
    chmod -R u+w .

    export ARCH=${kernelArch}

    make tinyconfig

    for s in $enableList; do
      ./scripts/config --enable "$s"
    done
    for s in $disableList; do
      ./scripts/config --disable "$s"
    done

    make olddefconfig

    cp .config $out
  '';

in
pkgs.linuxManualConfig {
  inherit (pkgs.linux_6_12) src version modDirVersion;
  inherit configfile;
  allowImportFromDerivation = true;
}
