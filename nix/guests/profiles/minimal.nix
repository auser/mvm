# minimal.nix — Minimal guest profile.
#
# Extends baseline with no additional packages.
# This is the lightest-weight profile for simple workloads.
{ config, lib, pkgs, ... }:

{
  # Baseline is imported by flake.nix — this module just sets profile-specific overrides.

  networking.hostName = lib.mkForce "mvm-minimal";

  # No additional packages beyond baseline
}
