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
  # The build system evaluates (with mvm-profiles.toml):
  #
  #   nix build <flake_ref>#packages.<system>.tenant-<role>-<profile>
  #
  # Legacy (without mvm-profiles.toml):
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
      # roleModules is optional — when provided, role-specific NixOS modules
      # are composed with the guest profile modules.
      mkGuest = { system, profile, guestModules, roleModules ? [] }:
        let
          guestConfig = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              microvm.nixosModules.microvm
              ./guests/baseline.nix
            ] ++ roleModules ++ guestModules;
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
      # Role-aware guest packages (with mvm-profiles.toml):
      #
      #   nix build .#tenant-gateway-minimal
      #   nix build .#tenant-worker-python
      #   nix build .#packages.aarch64-linux.tenant-gateway-minimal
      #
      # Legacy outputs (backward compat, no role prefix):
      #
      #   nix build .#tenant-minimal
      #   nix build .#tenant-python
      packages = forAllSystems (system: {
        # --- Role-aware outputs (role+profile combinations) ---

        tenant-gateway-minimal = mkGuest {
          inherit system;
          profile = "minimal";
          roleModules = [ ./roles/gateway.nix ];
          guestModules = [ ./guests/profiles/minimal.nix ];
        };

        tenant-worker-minimal = mkGuest {
          inherit system;
          profile = "minimal";
          roleModules = [ ./roles/worker.nix ];
          guestModules = [ ./guests/profiles/minimal.nix ];
        };

        tenant-builder-minimal = mkGuest {
          inherit system;
          profile = "minimal";
          roleModules = [ ./roles/builder.nix ];
          guestModules = [ ./guests/profiles/minimal.nix ];
        };

        tenant-gateway-python = mkGuest {
          inherit system;
          profile = "python";
          roleModules = [ ./roles/gateway.nix ];
          guestModules = [ ./guests/profiles/python.nix ];
        };

        tenant-worker-python = mkGuest {
          inherit system;
          profile = "python";
          roleModules = [ ./roles/worker.nix ];
          guestModules = [ ./guests/profiles/python.nix ];
        };

        tenant-builder-python = mkGuest {
          inherit system;
          profile = "python";
          roleModules = [ ./roles/builder.nix ];
          guestModules = [ ./guests/profiles/python.nix ];
        };

        # --- Legacy outputs (backward compat, no role) ---

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

      # Expose baseline, role, and microvm modules for user flakes to import
      nixosModules = {
        mvm-baseline = ./guests/baseline.nix;
        mvm-microvm = microvm.nixosModules.microvm;
        mvm-role-gateway = ./roles/gateway.nix;
        mvm-role-worker = ./roles/worker.nix;
        mvm-role-builder = ./roles/builder.nix;
        mvm-role-openclaw = ./roles/openclaw.nix;
      };
    };
}
