{
  description = "mvm — microVM library built on microvm.nix (plan 60).";

  # ── This flake is a LIBRARY, not a project ───────────────────────────
  #
  # User projects have their OWN `flake.nix` + `mvm.toml`. Their flake
  # imports `mvm` as an input and consumes the library helpers we
  # expose under `lib.<system>.mkGuest` to declare a microVM image.
  # `mvmctl build` reads the user's `mvm.toml`, follows the `flake`
  # field to their flake.nix, and runs `nix build` against it. Users
  # never edit anything inside this repository.
  #
  # User-side example (lives in *their* project):
  #
  #   # my-app/flake.nix
  #   {
  #     inputs.mvm.url = "github:auser/mvm";
  #     outputs = { self, mvm, ... }: {
  #       packages.x86_64-linux.default = mvm.lib.x86_64-linux.mkGuest {
  #         name = "my-app";
  #         services.web = {
  #           command = [ "/usr/local/bin/web" ];
  #         };
  #       };
  #     };
  #   }
  #
  #   # my-app/mvm.toml
  #   flake = "."
  #   profile = "default"
  #   vcpus = 1
  #   memory_mib = 256
  #
  # The `mkGuest` library is being ported from the previous iteration
  # in a follow-up wave (Phase 1 W5+); it composes microvm.nix's
  # NixOS module with mvm's security overlay (per-service uids,
  # seccomp tier, dm-verity, read-only `/etc`). The `lib` attribute
  # below is the placeholder that future user flakes will consume.
  #
  # ── Why microvm.nix (ADR-013) ────────────────────────────────────────
  #
  # microvm.nix abstracts Firecracker, Cloud Hypervisor, QEMU, crosvm,
  # kvmtool, and stratovirt as a NixOS module — so adding a hypervisor
  # is a config change here, not a kernel rewrite. Pinned by hash in
  # flake.lock; CI re-audits on every bump (`xtask audit-flake`).
  #
  # Fallback (named in ADR-013): if a per-bump audit of microvm.nix
  # surfaces a security regression we can't accept, revert to the
  # previous iteration's hand-rolled `nix/` tree at `../mvm/nix/`.
  #
  # ── nixosConfigurations.minimal ──────────────────────────────────────
  #
  # The `minimal` configuration below is **internal** — it's the test
  # fixture our Rust smoke tests use to exercise the build/exec path
  # (`tests/nix_flake_structure.rs`, `tests/smoke_microsandbox.rs`).
  # It is NOT a starter template for user projects. Users should
  # write their own flake using `lib.mkGuest`; the minimal profile
  # exists so we can boot something in CI without depending on a
  # user-side fixture.

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

      # ── Library output (USER-FACING) ─────────────────────────────
      #
      # This is what user flakes consume. Right now it exposes one
      # symbol — `mkGuest` — as a placeholder pointing at where the
      # full library port from the previous iteration will land
      # (Phase 1 W5+). User flakes can already import the surface
      # so their flake.nix's don't need to change when the
      # implementation fills in.
      mkLib = system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          # mkGuest { name, services?, packages?, hypervisor?, … }
          #   → a derivation producing a microVM rootfs + runner.
          #
          # Phase 1 W5+ ports the real implementation from
          # `../mvm/nix/flake.nix::mkGuestFn`. Until then, calling
          # mkGuest emits a clear error pointing users at the
          # microvm.nix path so they aren't blocked.
          mkGuest = _args:
            throw ''
              mvm.lib.${system}.mkGuest is not yet implemented in this
              iteration of mvm. Until Phase 1 W5 lands the port from
              the previous iteration's nix/lib/factories/, write your
              flake using the microvm.nix NixOS module directly:

                {
                  inputs.microvm.url = "github:microvm-nix/microvm.nix";
                  outputs = { self, nixpkgs, microvm, ... }: {
                    nixosConfigurations.my-app = nixpkgs.lib.nixosSystem {
                      system = "${system}";
                      modules = [
                        microvm.nixosModules.microvm
                        ({
                          microvm.hypervisor = "firecracker";
                          # … your config …
                        })
                      ];
                    };
                  };
                }

              `mvmctl build` reads your project's `mvm.toml` to find
              the flake reference and runs `nix build` against it.
              See: https://gomicrovm.com/guides/building-microvm-images/

              Tracked in plan 60 / specs/SPRINT.md Sprint 50 W5.
            '';
        };
    in
    {
      # ── User-facing: lib.<system>.mkGuest ────────────────────────
      #
      # User flakes import this as `inputs.mvm.lib.<system>.mkGuest`
      # to declare a microVM image. The shape is intentionally stable
      # so user flakes don't churn when the implementation evolves.
      lib = forAllSystems mkLib;

      # ── Internal: nixosConfigurations.minimal ────────────────────
      #
      # Test fixture for our smoke tests (`tests/smoke_microsandbox.rs`,
      # `tests/nix_flake_structure.rs`). NOT a starter template —
      # users write their own flake. The `internal` namespace makes
      # the boundary unambiguous so CI lints can grep for it.
      nixosConfigurations.internal-minimal-x86_64-linux =
        mkProfile "x86_64-linux" "minimal";
      nixosConfigurations.internal-minimal-aarch64-linux =
        mkProfile "aarch64-linux" "minimal";

      # Top-level package output mirroring the internal fixture so
      # `nix build .#internal-minimal-runner` works on Linux CI
      # runners. Same INTERNAL boundary — not consumed by user
      # flakes; if you find yourself running this command, you're
      # working on mvm itself, not a user project.
      packages = forAllSystems (system: {
        internal-minimal-runner =
          (mkProfile system "minimal").config.microvm.declaredRunner;
      });
    };
}
