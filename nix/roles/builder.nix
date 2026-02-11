# builder.nix — NixOS role module for builder instances.
#
# Builders have Nix installed and can build guest images.
# They get larger resource allocations and no secrets drive.
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkDefault "mvm-builder";

  # Nix daemon for builds
  nix = {
    settings = {
      experimental-features = [ "nix-command" "flakes" ];
      sandbox = true;
      max-jobs = "auto";
      cores = 0;
    };
  };

  environment.systemPackages = with pkgs; [
    git
    nix
  ];
}
