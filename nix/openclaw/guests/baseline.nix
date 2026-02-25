# Baseline NixOS configuration for mvm Firecracker guests.
#
# This module configures the guest OS for Firecracker:
# - Minimal kernel optimized for VM boot
# - Console on ttyS0 (Firecracker serial)
# - Mount points for mvm drives (config, secrets, data) by filesystem label
# - Automatic init of the NixOS system on boot
#
# mvm's drive model:
#   /dev/vda  = rootfs (ext4, read-only) — always present
#   /dev/vd*  = config drive (ext4, label=mvm-config, read-only) — per-instance metadata
#   /dev/vd*  = data drive (ext4, label=mvm-data, read-write) — optional persistent storage
#   /dev/vd*  = secrets drive (ext4, label=mvm-secrets, read-only) — ephemeral tenant secrets
#
# Drives are mounted by filesystem label (not device path) so the guest
# config is independent of Firecracker drive ordering.

{ lib, pkgs, ... }:
{
  system.stateVersion = "24.11";

  # --- Firecracker / microvm.nix settings ---
  microvm = {
    hypervisor = "firecracker";
    mem = lib.mkDefault 1024;
    vcpu = lib.mkDefault 2;
  };

  # --- Boot ---
  boot.loader.grub.enable = false;
  boot.kernelParams = [
    "console=ttyS0"
    "reboot=k"
    "panic=1"
    "pci=off"
  ];

  # --- Console ---
  systemd.services."serial-getty@ttyS0".enable = true;

  # --- Networking ---
  # IP is configured via Firecracker boot args (ip=<guest>::<gw>:<mask>::eth0:off).
  # The guest does not need DHCP or networkd for the primary interface.
  networking.useDHCP = false;

  # --- mvm config drive (read-only, per-instance metadata) ---
  # Contains config.json with instance_id, tenant_id, pool_id, guest_ip, etc.
  # Also used by roles to read app-specific config (gateway.toml, worker.toml).
  fileSystems."/mnt/config" = {
    device = "/dev/disk/by-label/mvm-config";
    fsType = "ext4";
    options = [ "ro" "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=5s" ];
    neededForBoot = false;
  };

  # --- mvm secrets drive (read-only, ephemeral tenant secrets) ---
  # Contains secrets.json with tenant API keys, certs, etc.
  # Recreated on every instance start/wake; never persisted to disk on host.
  fileSystems."/mnt/secrets" = {
    device = "/dev/disk/by-label/mvm-secrets";
    fsType = "ext4";
    options = [ "ro" "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=5s" ];
    neededForBoot = false;
  };

  # --- mvm data drive (read-write, persistent per-instance storage) ---
  # Optional — only present when pool spec has data_disk_mib > 0.
  fileSystems."/mnt/data" = {
    device = "/dev/disk/by-label/mvm-data";
    fsType = "ext4";
    options = [ "noexec" "nosuid" "nodev" "nofail" "x-systemd.device-timeout=5s" ];
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
