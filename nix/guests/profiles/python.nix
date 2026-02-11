# python.nix — Python guest profile.
#
# Extends baseline with Python 3, pip, and common data science packages.
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkForce "mvm-python";

  environment.systemPackages = with pkgs; [
    python3
    python3Packages.pip
    python3Packages.virtualenv
    git
  ];
}
