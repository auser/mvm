# nix-builder.nix — Ephemeral Nix builder microVM configuration.
#
# This VM is booted by `mvm pool build` to run `nix build` inside Firecracker.
# It has: Nix, git, outbound networking, SSH access, and a large tmpfs for builds.
# It is stateless and destroyed after each build.
{ config, lib, pkgs, ... }:

{
  microvm = {
    hypervisor = "firecracker";

    # Builder gets more resources than typical guests
    vcpu = lib.mkDefault 4;
    mem = lib.mkDefault 4096;

    volumes = [{
      mountPoint = "/";
      image = "rootfs.ext4";
      size = 4096;  # 4 GiB for Nix store during builds
    }];

    interfaces = [{
      type = "tap";
      id = "net1";
      mac = "02:00:00:00:00:00";  # overridden at launch
    }];

    shares = [{
      tag = "store";
      source = "/nix/store";
      mountPoint = "/nix/.ro-store";
      proto = "virtiofs";
    }];
  };

  # --- Networking ---
  # Builder needs outbound access to fetch flake inputs
  networking = {
    hostName = "mvm-builder";
    useDHCP = false;
    firewall.enable = false;
  };

  systemd.network = {
    enable = true;
    networks."10-eth0" = {
      matchConfig.Name = "eth0";
      networkConfig.DHCP = "no";
    };
  };

  # --- SSH (for host to push build commands) ---
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  users.users.root = {
    isSystemUser = true;
    openssh.authorizedKeys.keys = [
      # Injected at boot from tenant's ssh_key.pub
    ];
  };

  # --- Nix ---
  nix = {
    settings = {
      experimental-features = [ "nix-command" "flakes" ];
      # Allow builder to use all cores
      max-jobs = "auto";
      cores = 0;  # use all available
      sandbox = true;
    };
  };

  # --- Large tmpfs for builds ---
  fileSystems."/tmp" = {
    device = "tmpfs";
    fsType = "tmpfs";
    options = [ "size=2G" "mode=1777" ];
  };

  # --- Build tools ---
  environment.systemPackages = with pkgs; [
    nix
    git
    curl
    coreutils
    gnutar
    gzip
    xz
  ];

  # --- Boot tuning ---
  boot.kernelParams = [
    "console=ttyS0"
    "reboot=k"
    "panic=1"
    "pci=off"
    "nomodules"
  ];

  documentation.enable = false;

  # Signal readiness after boot
  systemd.services."mvm-builder-ready" = {
    description = "Signal host that builder VM is ready";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "nix-daemon.service" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = "${pkgs.coreutils}/bin/touch /run/mvm/builder-ready";
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /run/mvm";
    };
  };

  system.stateVersion = "24.11";
}
