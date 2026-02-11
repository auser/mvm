# baseline.nix — Base NixOS configuration for all mvm guest microVMs.
#
# Provides: openssh, static IP networking, fstab mounts, worker lifecycle hooks.
# All profiles import this module and extend it.
{ config, lib, pkgs, ... }:

{
  # --- microvm.nix Firecracker backend ---
  microvm = {
    hypervisor = "firecracker";

    # Resource defaults (overridden by pool spec at runtime via fc_config)
    vcpu = lib.mkDefault 2;
    mem = lib.mkDefault 1024;

    # Volumes: rootfs (read-only squashfs) + data disk (ext4, persistent)
    volumes = [{
      mountPoint = "/";
      image = "rootfs.ext4";
      size = 1024;  # MiB, overridden at build time
    }];

    # Network interface — Firecracker TAP device, configured at instance start
    interfaces = [{
      type = "tap";
      id = "net1";
      mac = "02:00:00:00:00:00";  # placeholder, overridden by fc_config
    }];

    # Share host /nix/store read-only (for Nix-based guests)
    shares = [{
      tag = "store";
      source = "/nix/store";
      mountPoint = "/nix/.ro-store";
      proto = "virtiofs";
    }];
  };

  # --- Networking ---
  # Static IP configuration — actual values injected via kernel boot args
  # or metadata service at runtime. This sets up the interface.
  networking = {
    hostName = lib.mkDefault "mvm-guest";
    useDHCP = false;

    interfaces.eth0 = {
      useDHCP = false;
      # Static IP set via boot args: ip=<guest_ip>::<gateway>::<mask>::<iface>:off
    };

    # Default gateway is the tenant bridge (.1)
    firewall.enable = false;  # Firecracker + bridge isolation handles this
  };

  # Use systemd-networkd for predictable interface management
  systemd.network = {
    enable = true;
    networks."10-eth0" = {
      matchConfig.Name = "eth0";
      # IP is configured via kernel command line (ip= parameter)
      # This just ensures the interface is brought up
      networkConfig = {
        DHCP = "no";
      };
    };
  };

  # --- SSH ---
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
    # Host keys generated on first boot
  };

  # Authorized keys injected from tenant's ssh_key.pub at instance start
  users.users.root = {
    isSystemUser = true;
    openssh.authorizedKeys.keys = [
      # Placeholder — actual key is mounted via secrets disk or metadata
    ];
  };

  # --- Filesystem mounts ---
  fileSystems = {
    # Data disk (persistent across restarts, ext4)
    "/data" = {
      device = "/dev/vdb";
      fsType = "ext4";
      options = [ "defaults" "noatime" ];
      autoFormat = false;
      neededForBoot = false;
    };

    # Secrets disk (recreated each run, ext4, read-only mount)
    "/run/secrets" = {
      device = "/dev/vdc";
      fsType = "ext4";
      options = [ "defaults" "noatime" "ro" ];
      autoFormat = false;
      neededForBoot = false;
    };
  };

  # --- Worker lifecycle hooks ---
  # These files signal the host about guest worker state.
  # The host agent watches for these via the Firecracker API socket or vsock.
  systemd.services."mvm-worker-ready" = {
    description = "Signal mvm host that worker is ready";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "openssh.service" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = "${pkgs.coreutils}/bin/touch /run/mvm/worker-ready";
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /run/mvm";
    };
  };

  # Sleep preparation service — invoked by host before snapshot
  systemd.services."mvm-sleep-prep" = {
    description = "Prepare worker for sleep (drop caches, compact memory)";
    serviceConfig = {
      Type = "oneshot";
      ExecStart = pkgs.writeShellScript "mvm-sleep-prep" ''
        # Drop page cache
        echo 3 > /proc/sys/vm/drop_caches
        # Sync all filesystems
        sync
        # Signal ready for snapshot
        touch /run/mvm/worker-idle
      '';
    };
  };

  # --- System tuning ---
  boot.kernelParams = [
    "console=ttyS0"
    "reboot=k"
    "panic=1"
    "pci=off"
    "nomodules"
    "8250.nr_uarts=0"
    "i8042.noaux"
    "i8042.nomux"
    "i8042.nopnp"
    "i8042.dumbkbd"
  ];

  # Minimal boot — no graphical target
  systemd.targets.multi-user.enable = true;
  services.getty.autologinUser = lib.mkDefault "root";

  # Time sync — important for snapshot resume
  services.chrony.enable = true;

  # Minimal system — no docs, no X
  documentation.enable = false;
  environment.systemPackages = with pkgs; [
    coreutils
    iproute2
    curl
    jq
  ];

  system.stateVersion = "24.11";
}
