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
  #
  # ── Trust boundary (cache + attestation) ────────────────────────────
  #
  # This flake is built inside a microsandbox VM. The resulting
  # `/nix/store` closure is persisted on the host at
  # `~/.cache/mvm/builder-store/` (`mvm_build::builder_vm::builder_store_dir`),
  # bind-mounted into the sandbox at `/scratch-nix`, and reused across
  # builds. That dir is mvm-owned (mode 0700, NEVER the host's actual
  # `/nix/store`).
  #
  # Why the cache is trustworthy: nix store paths are content-addressed
  # by input hash, so a poisoned cache entry would land at a different
  # path and could not satisfy a future build's input. `run_build_async`
  # additionally runs `nix-store --verify --check-contents` on builder
  # startup when a "dirty marker" indicates the previous run crashed,
  # so NAR-hash divergence is caught before the cache is reused.
  #
  # The artifact pair that ends up at
  # `nix/images/dev-prebuilt/<arch>/{vmlinux, rootfs.ext4}` ships
  # alongside `checksums-sha256.txt` written by
  # `xtask/src/build_dev_image.rs`. `mvmctl dev up` re-hashes the
  # vendored slot on every boot via
  # `apple_container::verify_vendored_checksums`; a mismatch hard-fails
  # the boot, surfacing tamper / partial-copy issues immediately.
  #
  # Kernel override (further down): a custom kernel is a deliberate
  # cache miss against `cache.nixos.org` because the rootfs has no
  # `/lib/modules/` and vsock therefore has to be built in. First
  # build of this flake on a host is slow (~20-25 min on aarch64);
  # subsequent builds reuse the cached kernel closure from
  # `~/.cache/mvm/builder-store/` and complete in ~30 s.
  #
  # macOS xattr caveat — bind-mount disabled by default on macOS.
  # libkrun's virtio-fs proxy strips `setxattr` from bind-mounted
  # APFS, which nix needs to mark chroot-store paths; `nix build`
  # fails on the first store-path write with EIO. `run_build_async`
  # detects macOS and force-fallbacks to an in-guest tmpfs (no
  # persistent cache, cold ~25 min every run). Linux hosts get the
  # bind-mount + warm cache. The block-device workaround
  # (sparse ext4 file attached as a raw disk, mkfs'd by the guest)
  # is the planned macOS fix — see Sprint 50 follow-up. Opt into
  # the bind-mount today with `MVM_BUILDER_USE_HOST_STORE=1` only
  # if you're working on that follow-up.

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
          # Override the stock kernel to compile vsock support in
          # (rather than as modules). The rootfs we ship is a flat
          # busybox tree without `/lib/modules/`, so `=m` modules can
          # never load — `mvm-guest-agent` then fails `socket(AF_VSOCK,
          # …)` because the address family isn't registered, and
          # every host-side surface that talks to the agent
          # (`mvmctl console`, `dev shell`, `build`) goes dark.
          #
          # Built-in:
          #   - VSOCKETS:               AF_VSOCK address family
          #   - VHOST_VSOCK:            host-side vhost driver path
          #   - VIRTIO_VSOCKETS_COMMON: shared virtio-vsock plumbing
          #   - VIRTIO_VSOCKETS:        guest-side virtio transport
          #   - VHOST + VHOST_NET:      dependencies the above pull in
          devKernel = pkgs.linuxPackages.kernel.override {
            structuredExtraConfig = with pkgs.lib.kernel; {
              VSOCKETS = yes;
              VHOST = yes;
              VHOST_VSOCK = yes;
              VIRTIO_VSOCKETS_COMMON = yes;
              VIRTIO_VSOCKETS = yes;
            };
            ignoreConfigErrors = true;
          };
          kernelPkg = devKernel;
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
