{
  description = "mvm — Firecracker microVM guest images and builder";

  # =========================================================================
  # User-provided flakes
  # =========================================================================
  #
  # Users can specify their own flake via `mvm pool create ... --flake <ref>`.
  # The flake_ref can be any valid Nix flake reference:
  #
  #   --flake .                              (local directory)
  #   --flake /path/to/my-flake             (absolute path)
  #   --flake github:myorg/my-vm            (GitHub)
  #   --flake github:myorg/my-vm?rev=abc123 (pinned revision)
  #
  # User flakes MUST expose a microvm.nix-based NixOS configuration.
  # The build system evaluates:
  #
  #   nix build <flake_ref>#packages.<system>.tenant-<profile>
  #
  # The output attribute set must contain:
  #   - kernel   : path to vmlinux
  #   - rootfs   : path to rootfs.ext4 or squashfs image
  #   - toplevel : NixOS system closure
  #
  # This flake (nix/) provides built-in profiles as a reference and for
  # quick-start usage. The builder VM definition is always mvm-internal.
  # =========================================================================

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    microvm = {
      url = "github:astro/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, microvm }:
    let
      # Supported systems — Lima provides aarch64-linux on Apple Silicon,
      # native x86_64-linux on Intel/AMD hosts.
      supportedSystems = [ "aarch64-linux" "x86_64-linux" ];

      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems f;

      # Build a guest NixOS configuration and extract Firecracker artifacts.
      # User flakes should expose the same output shape.
      mkGuest = { system, profile, guestModules }:
        let
          guestConfig = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              microvm.nixosModules.microvm
              ./guests/baseline.nix
            ] ++ guestModules;
          };
          config = guestConfig.config;
        in {
          # microvm.nix exposes these as config attributes
          kernel = config.microvm.kernel;
          rootfs = config.microvm.volumes.root or config.system.build.squashfs;
          toplevel = config.system.build.toplevel;
        };

      # Build the Nix builder VM configuration (mvm-internal, not user-facing).
      mkBuilder = { system }:
        let
          builderConfig = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              microvm.nixosModules.microvm
              ./builders/nix-builder.nix
            ];
          };
          config = builderConfig.config;
        in {
          kernel = config.microvm.kernel;
          rootfs = config.microvm.volumes.root or config.system.build.squashfs;
          toplevel = config.system.build.toplevel;
        };

    in {
      # Per-profile guest packages (built-in profiles):
      #
      #   nix build .#tenant-minimal
      #   nix build .#tenant-python
      #   nix build .#packages.aarch64-linux.tenant-minimal
      #
      # User flakes should follow the same naming convention:
      #   packages.<system>.tenant-<profile>
      packages = forAllSystems (system: {
        tenant-minimal = mkGuest {
          inherit system;
          profile = "minimal";
          guestModules = [ ./guests/profiles/minimal.nix ];
        };

        tenant-python = mkGuest {
          inherit system;
          profile = "python";
          guestModules = [ ./guests/profiles/python.nix ];
        };

        # Builder VM — used internally by `mvm pool build`, not user-facing
        nix-builder = mkBuilder { inherit system; };
      });

      # Expose baseline and microvm module for user flakes to import
      nixosModules = {
        mvm-baseline = ./guests/baseline.nix;
        mvm-microvm = microvm.nixosModules.microvm;
      };
    };
}
