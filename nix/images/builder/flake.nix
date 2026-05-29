{
  description = "mvm dev VM image — Linux kernel + ext4 rootfs with Nix + build tools";

  # ── Why this flake exists ────────────────────────────────────────────
  #
  # `cargo xtask build-dev-image` runs `nix build` against
  # `packages.<system>.default` here, expects a derivation whose `$out`
  # directory contains `vmlinux` and `rootfs.ext4`, and drops those into
  # the vendored slot at `nix/images/dev-prebuilt/<arch>/` where
  # `mvm_cli::commands::env::apple_container::find_vendored_dev_image`
  # picks them up at highest precedence. The same shape is what
  # `.github/workflows/release.yml`'s `dev-image` job uploads to the
  # release page; the source-checkout and release-page paths share one
  # contract.
  #
  # ── Architecture / why we bypass the parent flake input ─────────────
  #
  # Earlier iterations of this flake had `mvm.url = "path:../.."` to
  # consume the parent `mvm` flake's `lib.<system>.mkGuest`. That
  # works fine on the host, but the xtask runs `nix build` inside a
  # libkrun sandbox where `/work` is a read-only bind-mount of
  # the host workspace. In that setup the path-input chain
  # (`mvm = path:../..`, parent's `mvm-workspace = path:..`) trips
  # nix's strict lock validation with "lock file contains unlocked
  # input '{"path":"..","type":"path"}'" — path inputs can't be
  # locked by content hash, and the lock file can't be rewritten on
  # the read-only mount.
  #
  # Workaround: skip the parent flake input entirely. Stage the
  # workspace once via `builtins.path`, then `import` the parent's
  # `nix/lib/default.nix` directly with our own `mvmSrc` pointing at
  # the staged tree. No flake input → no lock → no validation
  # failure. The shape mkGuest produces is identical to consuming it
  # through the flake's `lib` output.
  #
  # mvmSrc points at the workspace root because
  # `nix/packages/mvm-guest-agent.nix` reads `${mvmSrc}/Cargo.lock`
  # to vendor the cargo closure — the lockfile lives at the
  # workspace root, not under `nix/`.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, microvm, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # Stage the workspace root (`<repo>/`, three levels up from
      # this flake's `nix/images/builder/`) into the store. The
      # `name` argument is what shows up in derivation logs.
      #
      # The filter excludes large directories that are never inputs to
      # the dev-image build: `target/` (cargo build output, multi-GB),
      # `.git/` (history), `result*` (nix build symlinks), and a few
      # developer-environment dirs. Without it, `builtins.path` hashes
      # and store-copies the whole workspace on every evaluation —
      # slow on cold caches and wasteful even when warm.
      #
      # When running inside the libkrun builder VM, the flake is
      # fetched via `path:` URL which store-copies just the flake
      # subdir; `../../..` from that store location resolves outside
      # the workspace and would trip over sandbox-internal files like
      # `/.msb/agent.sock`. The builder passes `MVM_WORKSPACE_PATH`
      # (under `--impure`) pointing at the workspace mount, and we
      # use that absolute path when set. Outside the sandbox (e.g.
      # running `nix build` directly on the host), the env var is
      # empty and `../../..` works as before.
      workspaceRoot =
        let
          envPath = builtins.getEnv "MVM_WORKSPACE_PATH";
        in
        if envPath != "" then /. + envPath else ../../..;
      # Filter list lives at nix/lib/workspace-filter.nix so the three
      # flakes that ingest the host workspace (this one, builder-vm/,
      # runtime-overlay/) stay aligned with .gitignore in one place.
      workspace =
        (import (workspaceRoot + "/nix/lib/workspace-filter.nix") {
          inherit (nixpkgs) lib;
        })
        { inherit workspaceRoot; };

      # Import the parent flake's library code directly. `nix/lib/`
      # lives at `${workspace}/nix/lib`; its `default.nix` returns
      # `{ system }: { mkGuest, ... }`.
      libFor = import (workspace + "/nix/lib") {
        inherit nixpkgs microvm;
        mvmSrc = workspace;
      };

      builderPackages = pkgs: with pkgs; [
        bashInteractive coreutils gnugrep gnused gawk findutils which less
        nix git gnumake curl jq iproute2 iptables e2fsprogs util-linux procps
      ];

      # Stage 0 bootstrap support: source checkouts must not download
      # a published builder-VM image. When the real builder-VM cache is
      # empty, mvmctl can boot an already-cached dev image with
      # `init=/sbin/mvm-host-vm-init` and ask it to build
      # `nix/images/builder-vm`. That requires the dev image to carry
      # the same PID-1 binary, even though normal `mvmctl dev up`
      # boots via `/init`.
      mvmBuilderInitFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "mvm-host-vm-init";
          version = "0.14.0";
          src = workspace;
          cargoLock = {
            lockFile = workspace + "/Cargo.lock";
          };
          buildAndTestSubdir = "crates/mvm-host-vm-init";
          doCheck = false;
          meta = {
            description = "PID-1 for local builder VM bootstrap";
            mainProgram = "mvm-host-vm-init";
          };
        };

      mkBuilderImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          builderInit = mvmBuilderInitFor system;
          # Stock nixpkgs kernel. Pass it to `mkGuest` so the rootfs
          # ships its module tree (`/lib/modules/<kver>/`) and `/init`
          # can `modprobe vmw_vsock_virtio_transport` before the agent
          # forks. Without modules the agent fails to open AF_VSOCK
          # (nixpkgs default config has VSOCK=m) and every host-side
          # surface (`mvmctl console`, `dev shell`, `build`) goes dark.
          kernelPkg = pkgs.linuxPackages.kernel;
          rootfs = (libFor { inherit system; }).mkGuest {
            name = "mvm-dev";
            entrypoint.shell = "/bin/sh";
            packages = builderPackages pkgs;
            kernel = kernelPkg;
            extraFiles = {
              "/sbin/mvm-host-vm-init" =
                "${builderInit}/bin/mvm-host-vm-init";
            };
          };
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64
            then "Image"
            else "bzImage";
        in
        pkgs.runCommand "mvm-dev-image-${system}"
          {
            passthru = {
              inherit rootfs;
              kernel = kernelPkg;
              inherit (rootfs.passthru) mvm;
            };
          }
          ''
            mkdir -p $out

            if [ -f ${kernelPkg}/${kernelFile} ]; then
              cp ${kernelPkg}/${kernelFile} $out/vmlinux
            elif [ -f ${kernelPkg}/Image ]; then
              cp ${kernelPkg}/Image $out/vmlinux
            elif [ -f ${kernelPkg}/bzImage ]; then
              cp ${kernelPkg}/bzImage $out/vmlinux
            else
              echo "kernel package ${kernelPkg} did not produce Image or bzImage" >&2
              ls -la ${kernelPkg} >&2
              exit 1
            fi

            if [ -f ${rootfs} ]; then
              cp ${rootfs} $out/rootfs.ext4
            else
              img=$(find ${rootfs} -maxdepth 1 -name '*.img' -o -name '*.ext4' | head -1)
              if [ -z "$img" ]; then
                echo "mkGuest output at ${rootfs} contains no .img or .ext4 file" >&2
                ls -la ${rootfs} >&2
                exit 1
              fi
              cp "$img" $out/rootfs.ext4
            fi

            chmod 0644 $out/vmlinux $out/rootfs.ext4

            # Plan 77 W5 — host-side preflight Stage 0 seed contract.
            # `validate_stage0_seed_contract` in mvm-cli reads this
            # before booting the dev image as a Stage 0 bootstrap and
            # refuses to launch libkrun if the contract doesn't match.
            # The sidecar is metadata, not a trust anchor — see Plan 77
            # security consideration 13.
            #
            # schema_version: shape of this manifest itself.
            # contract_version: bumped on any backward-incompatible
            #   Stage 0 contract change (init binary moves, kernel
            #   cmdline shape changes, expected mount points shift).
            #   Bump in lockstep with the matching mvmctl-side minimum.
            # init_paths: paths the host expects to find inside the
            #   rootfs. mvmctl validates that this flake's extraFiles
            #   declared each one; it does not peek inside the ext4.
            # image_kind: "dev" disambiguates from the builder-vm
            #   flake's manifest (image_kind would be "builder-vm"
            #   there if it carried this field).
            cat > $out/manifest.json <<MANIFEST
            {
              "schema_version": 1,
              "contract_version": 2,
              "image_kind": "dev",
              "system": "${system}",
              "init_paths": ["/sbin/mvm-host-vm-init"]
            }
            MANIFEST
            chmod 0644 $out/manifest.json
          '';
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderImage system;
      });
    };
}
