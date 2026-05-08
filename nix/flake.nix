{
  description = "mvm — microVM images built on microvm.nix (plan 60).";

  # ── microvm.nix as the foundation (ADR-013) ──────────────────────────
  #
  # This flake replaces the previous iteration's hand-rolled NixOS-module
  # tree (~5K LOC of mkGuest scaffolding) with a thin layer on top of
  # microvm.nix. microvm.nix already abstracts Firecracker, Cloud
  # Hypervisor, QEMU, crosvm, kvmtool, and stratovirt as a NixOS module,
  # so adding a hypervisor is a config change here, not a kernel rewrite.
  #
  # Profiles under `./profiles/` compose microvm.nix's `microvm`
  # NixOS module with our security overlay (per-service uids, seccomp
  # tier, dm-verity, read-only `/etc` — these land in subsequent waves
  # as Phase 6 ports the security model from `../mvm/crates/mvm-security`).
  #
  # Build (on a host with a Linux builder):
  #
  #   nix build .#nixosConfigurations.minimal.config.microvm.declaredRunner
  #
  # That produces a runner script + a rootfs ext4 image plus kernel + initrd.
  # `mvmctl` consumes the ext4 path through the .ext4→.raw alias bridge
  # (`mvm-runtime/src/vm/microsandbox.rs::ensure_microsandbox_rootfs_alias`).
  #
  # Fallback (named in ADR-013): if a per-bump audit of microvm.nix
  # surfaces a security regression we can't accept, revert to the
  # previous iteration's hand-rolled `nix/` tree at `../mvm/nix/`.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    # microvm.nix — the foundation. MIT-licensed; pinned by hash in
    # flake.lock; CI re-audits on every bump (xtask audit-flake).
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , microvm
    , ...
    }:
    let
      systems = [
        # microVM images build only on Linux — no macOS/Darwin output.
        # Cross-builds from macOS need a Linux builder (linux-builder
        # via nix-darwin, or remote nix-daemon). Documented in
        # `specs/runbooks/cross-platform-install.md` (Phase 5).
        "x86_64-linux"
        "aarch64-linux"
      ];

      # Helper: construct a NixOS configuration for the named profile.
      mkProfile = system: profileName: nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          microvm.nixosModules.microvm
          (./profiles + "/${profileName}.nix")
        ];
      };

      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      # Per-system NixOS configurations, one per profile. Profile files
      # live under `./profiles/`. Phase 1 W4 ships `minimal`; Phase 1
      # W5 (and onward) adds `worker`, `builder`, `ai-sandbox`, etc.
      nixosConfigurations = nixpkgs.lib.foldl' nixpkgs.lib.recursiveUpdate { } (
        map
          (system: {
            "${system}.minimal" = mkProfile system "minimal";
          })
          systems
      ) // {
        # Default to x86_64-linux for the bare profile names — the
        # systems-prefixed forms above are the explicit cross-arch
        # path. Kept separate from the system-prefixed map so
        # `nix build .#nixosConfigurations.minimal` works on any
        # x86_64-Linux builder.
        minimal = mkProfile "x86_64-linux" "minimal";
      };

      # Top-level package outputs surface the runner script. Consumers
      # who don't need the full NixOS interface can just
      # `nix build .#minimal-runner` to get a shell script that boots
      # the configured hypervisor with the right artifacts.
      packages = forAllSystems (system: {
        minimal-runner =
          (mkProfile system "minimal").config.microvm.declaredRunner;
      });
    };
}
