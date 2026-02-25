# Worker profile — NixOS guest configuration for the worker role.
#
# Configures hostname, firewall ports, and a tmpfs workspace.
# Actual tenant-specific config comes from mvm's config/secrets drives at runtime.

{ ... }:
{
  networking.hostName = "openclaw-worker";
  networking.firewall.enable = true;
  networking.firewall.allowedTCPPorts = [ 18790 ];

  # Role-specific workspace (tmpfs, not persisted across reboots).
  # Persistent data should use /mnt/data (mvm data drive).
  fileSystems."/var/lib/openclaw" = {
    fsType = "tmpfs";
    device = "tmpfs";
    options = [ "mode=0755" "size=1024m" ];
  };
}
