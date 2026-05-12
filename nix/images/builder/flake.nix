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
  # microsandbox sandbox where `/work` is a read-only bind-mount of
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
      workspace = builtins.path {
        path = ../../..;
        name = "mvm-workspace";
      };

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

      mkBuilderImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          rootfs = (libFor { inherit system; }).mkGuest {
            name = "mvm-dev";
            entrypoint.shell = "/bin/sh";
            packages = builderPackages pkgs;
          };
          kernelPkg = pkgs.linuxPackages.kernel;
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
          '';
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderImage system;
      });
    };
}
