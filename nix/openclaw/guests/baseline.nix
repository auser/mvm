# Baseline NixOS configuration for mvm Firecracker guests.
#
# This module configures the guest OS for Firecracker:
# - Minimal kernel for VM boot
# - Console on ttyS0 (Firecracker serial)
# - Root filesystem on /dev/vda (ext4, the Nix-built rootfs image)
# - Network via systemd-networkd, IP passed from host via kernel cmdline
# - Mount points for mvm drives (config, secrets, data) by filesystem label
# - Automatic init of the NixOS system on boot
#
# mvm's drive model:
#   /dev/vda  = rootfs (ext4, read-write) — always present, contains NixOS + nix store
#   /dev/vd*  = config drive (ext4, label=mvm-config, read-only) — per-instance metadata
#   /dev/vd*  = data drive (ext4, label=mvm-data, read-write) — optional persistent storage
#   /dev/vd*  = secrets drive (ext4, label=mvm-secrets, read-only) — ephemeral tenant secrets
#
# Drives are mounted by filesystem label (not device path) so the guest
# config is independent of Firecracker drive ordering.
#
# Networking:
#   The host assigns each VM a static IP and passes it via Firecracker
#   kernel boot args: mvm.ip=<cidr> mvm.gw=<gateway>.  A one-shot
#   systemd service reads /proc/cmdline and writes a networkd config
#   before systemd-networkd starts.  No DHCP needed.

{ lib, pkgs, ... }:
{
  system.stateVersion = "24.11";

  # --- Boot ---
  boot.loader.grub.enable = false;
  boot.kernelParams = [
    "console=ttyS0"
    "reboot=k"
    "panic=1"
    # Force classic eth0 naming — Firecracker with --enable-pci would
    # otherwise assign predictable names (enp0s2) which are harder to
    # configure statically.
    "net.ifnames=0"
  ];

  # Ensure virtio drivers are loaded in the initrd so the network
  # interface exists by the time stage-2 systemd starts.
  boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" ];
  boot.initrd.kernelModules = [ "virtio_net" ];

  # --- Minimize boot time ---
  documentation.enable = false;
  boot.tmp.useTmpfs = true;
  services.timesyncd.enable = false;
  security.audit.enable = false;

  # --- Root filesystem ---
  # The rootfs ext4 image (built by make-ext4-fs.nix) is presented as /dev/vda.
  # It contains the complete NixOS system closure including /nix/store.
  fileSystems."/" = {
    device = "/dev/vda";
    fsType = "ext4";
    autoResize = true;
  };

  # --- Console ---
  systemd.services."serial-getty@ttyS0".enable = true;

  # --- Networking (systemd-networkd + kernel cmdline IP) ---
  # The host passes mvm.ip=<cidr> and mvm.gw=<ip> in Firecracker boot args.
  # A one-shot service reads these from /proc/cmdline and writes a networkd
  # .network file before networkd starts.  This avoids the 90s device-wait
  # timeout that legacy networking.interfaces generates.
  networking.useNetworkd = true;
  networking.useDHCP = false;
  systemd.network.enable = true;
  systemd.network.wait-online.enable = false;

  systemd.services.mvm-network-config = {
    description = "Configure network from mvm kernel parameters";
    before = [ "systemd-networkd.service" ];
    wantedBy = [ "systemd-networkd.service" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "mvm-network-config" ''
        CMDLINE=$(cat /proc/cmdline)
        IP=$(echo "$CMDLINE" | ${pkgs.gnugrep}/bin/grep -oP 'mvm\.ip=\K[^ ]+')
        GW=$(echo "$CMDLINE" | ${pkgs.gnugrep}/bin/grep -oP 'mvm\.gw=\K[^ ]+')
        if [ -n "$IP" ] && [ -n "$GW" ]; then
          mkdir -p /run/systemd/network
          cat > /run/systemd/network/10-eth0.network << EOF
        [Match]
        Name=eth0

        [Network]
        Address=$IP
        Gateway=$GW
        DNS=$GW
        EOF
        fi
      '';
    };
  };

  # --- mvm config drive (read-only, per-instance metadata) ---
  # Contains config.json with instance_id, tenant_id, pool_id, guest_ip, etc.
  # Also used by roles to read app-specific config (gateway.toml, worker.toml).
  fileSystems."/mnt/config" = {
    device = "/dev/disk/by-label/mvm-config";
    fsType = "ext4";
    options = [ "ro" "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=1s" ];
    neededForBoot = false;
  };

  # --- mvm secrets drive (read-only, ephemeral tenant secrets) ---
  # Contains secrets.json with tenant API keys, certs, etc.
  # Recreated on every instance start/wake; never persisted to disk on host.
  fileSystems."/mnt/secrets" = {
    device = "/dev/disk/by-label/mvm-secrets";
    fsType = "ext4";
    options = [ "ro" "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=1s" ];
    neededForBoot = false;
  };

  # --- mvm data drive (read-write, persistent per-instance storage) ---
  # Optional — only present when pool spec has data_disk_mib > 0.
  fileSystems."/mnt/data" = {
    device = "/dev/disk/by-label/mvm-data";
    fsType = "ext4";
    options = [ "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=1s" ];
    neededForBoot = false;
  };

  # --- Minimal packages ---
  environment.systemPackages = with pkgs; [
    curl
    jq
  ];

  # --- Security hardening ---
  # microVMs are headless workloads — no SSH, no interactive login.
  # Communication is via Firecracker vsock only.
  security.sudo.enable = false;
  users.mutableUsers = false;
  users.allowNoPasswordLogin = true;
}
