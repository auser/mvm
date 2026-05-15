# Internal test fixture — NOT a user-facing template.
#
# This profile exists so mvm's own Nix flake structure tests have a
# minimal internal fixture (`tests/nix_flake_structure.rs`).
# It is **not** a starter for user projects. User flakes use
# `mvm.lib.<system>.mkGuest { … }` to declare a microVM image —
# see `public/src/content/docs/guides/building-microvm-images.md`
# for the user-facing surface.
#
# The flake exposes this configuration under
# `nixosConfigurations.internal-minimal-<system>` so the namespace
# encodes the boundary: anything prefixed `internal-` is mvm-private
# tooling, not part of the public API.
#
# Security overlay placeholders documented inline — they land in
# Phase 6 as the security port from `../mvm/crates/mvm-security` /
# `../mvm/specs/plans/25-microvm-hardening.md` completes:
#
#   W2.1  per-service uid              (TODO Phase 6)
#   W2.2  read-only /etc bind mount    (TODO Phase 6)
#   W2.3  setpriv launch line          (TODO Phase 6)
#   W2.4  per-service seccomp tier     (TODO Phase 6)
#   W3    dm-verity rootfs             (TODO Phase 6 — Firecracker only;
#                                       microsandbox uses image-hash + HMAC)
#
# The profile is deliberately scheme-agnostic about the hypervisor:
# microvm.nix selects the right runner from `microvm.hypervisor` and
# builds artifacts our backend dispatch (Firecracker / microsandbox /
# Cloud Hypervisor) can consume.

{ config, lib, pkgs, ... }:

{
  microvm = {
    # Default hypervisor for `microvm.declaredRunner`. Production
    # Linux paths point at Firecracker via the mvm runtime backend
    # (ADR-013) — this default is for `nix build .#minimal-runner`
    # convenience and gets overridden when consumers want a different
    # runner.
    hypervisor = "firecracker";

    # Resource defaults — sized so the image boots cleanly on a
    # constrained CI runner. Tenant policy (mvm-policy crate) bumps
    # these when a workload demands it.
    vcpu = 1;
    mem = 256;

    # Minimal device set — vsock for the guest agent + a single
    # rootfs share. No TAP, no extra disks. Phase 3 (network
    # isolation) extends this with the L4/L7-mediated path.
    interfaces = [ ];
    volumes = [ ];
    shares = [ ];
  };

  # Hostname is informational; the per-instance name comes from the
  # caller (mvmctl writes it on boot).
  networking.hostName = lib.mkDefault "mvm-minimal";

  # Locale + timezone defaults — chosen for size (no extra locale
  # data) rather than user-friendliness. Profiles that surface to
  # end users override these.
  time.timeZone = "UTC";
  i18n.defaultLocale = "C.UTF-8";

  # Bare-bones package set: a shell + coreutils. The guest agent
  # (`mvm-guest-agent`, ported from `../mvm/crates/mvm-guest`) lands
  # in Phase 1 W5 as the next wave; it brings its own systemd
  # service unit when it ships.
  environment.systemPackages = with pkgs; [
    coreutils
    bashInteractive
  ];

  # No services exposed by default. Phase 1 W5 adds the guest-agent
  # service unit + the entrypoint supervisor.

  # SSH explicitly disabled. The guest agent communicates over vsock
  # only (CLAUDE.md / ADR-002 §"No SSH in microVMs, ever"). Setting
  # `services.openssh.enable = false` is redundant with NixOS's
  # default but stated here as a load-bearing invariant.
  services.openssh.enable = false;

  # Stable system version pinned. Bumps land alongside nixpkgs flake
  # input bumps in CI.
  system.stateVersion = "25.11";
}
