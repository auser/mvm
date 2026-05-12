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
  # ── Architecture ─────────────────────────────────────────────────────
  #
  # We bring in the parent `mvm` flake as an input. The parent exposes
  # `lib.<system>.mkGuest` (`nix/lib/mk-guest.nix`) which builds a
  # busybox-PID-1 rootfs with the production `mvm-guest-agent` baked
  # in — that binary is load-bearing for `mvmctl console`,
  # `mvmctl dev shell`, and every guest-side hook the host CLI relies
  # on. Replicating that wiring outside `mkGuest` would mean re-doing
  # the vsock + setpriv + uid-drop dance that ADR-002 §W2-§W4
  # specifies, so this flake leans on `mkGuest` instead.
  #
  # `mkGuest` returns an ext4 image derivation; we pair it with a
  # nixpkgs-built Linux kernel and `runCommand`-wrap both into a
  # single output directory.

  inputs = {
    # Point at the parent flake. `path:../..` resolves relative to
    # this flake's directory at evaluation time. Falls back through
    # the standard flake resolution if invoked with an explicit
    # `--override-input mvm <abs-path>`.
    mvm.url = "path:../..";
    # Pin nixpkgs through the parent so version drift between flakes
    # doesn't surface as "two store paths for the same library."
    nixpkgs.follows = "mvm/nixpkgs";
  };

  outputs =
    { self, mvm, nixpkgs, ... }:
    let
      # Only Linux targets — the dev VM is a Linux microVM regardless
      # of host. `mvmctl` consumes `aarch64` on Apple Silicon and on
      # aarch64-linux, `x86_64` everywhere else; the flake exposes
      # both so the matrix in `release.yml` and the xtask can pick
      # either by system identifier.
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # The set of packages baked into the rootfs. Keep this list
      # tight — every entry expands the closure and the rootfs size,
      # and the rootfs travels through every `dev up` cache layer.
      # The contents mirror what `nix/images/builder/flake.nix` at
      # commit 20f776e (the historical version, retired with v1)
      # carried — same shape, but pruned to what the current
      # mvmctl + microsandbox path actually needs.
      builderPackages = pkgs: with pkgs; [
        # Core shell + textutils so `mvmctl dev shell` is usable.
        bashInteractive
        coreutils
        gnugrep
        gnused
        gawk
        findutils
        which
        less

        # `nix` itself — the whole point of the builder VM is running
        # `nix build` against user flakes.
        nix
        git

        # Common build deps a user flake might shell out to.
        gnumake
        curl
        jq

        # Networking helpers for the bridge_ensure path in
        # `mvm-runtime/src/vm/network.rs`. iptables stays here even
        # though we're not doing host-side networking on macOS —
        # when this image is consumed by an mvmd worker on Linux,
        # iptables is what gates the per-tenant bridge.
        iproute2
        iptables

        # Filesystem + process tools used by both nix-collect-garbage
        # and by `mvmctl dev shell` interactive sessions.
        e2fsprogs
        util-linux
        procps
      ];

      # Build the kernel + rootfs pair for a given system. Wrapped in
      # a `runCommand` so the output is a directory `mvmctl` can scan
      # rather than an arbitrarily-named .img file.
      mkBuilderImage = system:
        let
          pkgs = import nixpkgs { inherit system; };

          # mkGuest with the dev variant: `entrypoint.shell` triggers
          # the dev-shell feature on the guest agent (the `do_exec`
          # vsock handler that `mvmctl exec`/`console` rely on) and
          # makes the rootfs `passthru.mvm.accessible = true`, so
          # `mvmctl dev shell` actually attaches.
          rootfs = mvm.lib.${system}.mkGuest {
            name = "mvm-dev";
            entrypoint.shell = "/bin/sh";
            packages = builderPackages pkgs;
          };

          # nixpkgs's default Linux kernel for the target system.
          # `linuxPackages.kernel` is the stable channel selection;
          # bumping to a newer LTS is a future config knob.
          # `kernelFile` resolves to "Image" on aarch64 and "bzImage"
          # on x86_64 — both Apple Container and Firecracker accept
          # the raw kernel image at those names.
          kernelPkg = pkgs.linuxPackages.kernel;
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64
            then "Image"
            else "bzImage";
        in
        pkgs.runCommand "mvm-dev-image-${system}"
          {
            # Surface the inputs so a debugger / `nix eval` can pick
            # them apart without re-running the build.
            passthru = {
              inherit rootfs;
              kernel = kernelPkg;
              inherit (rootfs.passthru) mvm;
            };
          }
          ''
            mkdir -p $out

            # Kernel image → $out/vmlinux. Try the arch-appropriate
            # filename first, then fall back to either common name
            # so a kernel package built with a non-default config
            # still resolves.
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

            # Rootfs → $out/rootfs.ext4. mkGuest's output is an
            # ext4 image; nixpkgs `make-ext4-fs.nix` emits the .img
            # file inside the derivation `$out` directory. Handle
            # both the "single file is $out" and "$out is a dir
            # containing the .img" cases — exact shape varies by
            # nixpkgs version.
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
      # `packages.<sys>.default` is the contract the xtask + the
      # release workflow share. Keep the attribute name stable;
      # additional variants (e.g., a future sealed builder image)
      # land as sibling attributes, not replacements.
      packages = forAllSystems (system: {
        default = mkBuilderImage system;
      });
    };
}
