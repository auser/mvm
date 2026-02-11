# capability-imessage.nix — Placeholder role module for iMessage capability.
#
# This role will provide iMessage integration services.
# Currently a placeholder that extends the worker baseline.
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkDefault "mvm-capability-imessage";
}
