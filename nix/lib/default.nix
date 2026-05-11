# Library entry point — exposed as `mvm.lib.<system>` from the root
# flake. Functions here are user-facing and stable; their shapes are
# part of mvm's public API. New helpers go in their own files under
# `nix/lib/` and get re-exported here.

{ nixpkgs, microvm, mvmSrc }:
let
  mkGuestImpl = import ./mk-guest.nix { inherit nixpkgs microvm mvmSrc; };
in
{ system }:
{
  # Full mkGuest implementation. Documented at
  # `public/src/content/docs/guides/building-microvm-images.md`.
  mkGuest = args: (mkGuestImpl { inherit system; }) args;
}
