{
  description = "mvm builder VM image — Linux kernel + ext4 rootfs that runs `nix build` on behalf of mvmctl";

  # ── What this flake produces ────────────────────────────────────────
  #
  # `packages.<system>.default` is a derivation whose `$out` contains:
  #
  #   $out/vmlinux       — Linux kernel (ELF)
  #   $out/rootfs.ext4   — ext4 filesystem, mvm-builder-init at /sbin
  #   $out/manifest.json — { sha256 + size for both, kernelCmdline, isStub }
  #
  # This is the **Layer 1** image from ADR-046 / Plan 72: the VM that
  # runs `nix build` to produce **Layer 2** (user-facing dev/workload
  # images). It is NOT the dev VM users boot into — that lives in
  # `nix/images/builder/` (to be renamed `nix/images/dev-shell/` in
  # plan 72 W5).
  #
  # ── Why not mkGuest ─────────────────────────────────────────────────
  #
  # `nix/lib/mk-guest.nix` is purpose-built for workloads: it bakes a
  # /init that forks the guest agent under uid 990, sets up /etc/mvm/
  # {name,variant,entrypoint}, and execs the user's entrypoint under
  # setpriv at uid 1000 by default. The builder VM has none of those
  # needs — no vsock RPC, no privilege drop, single-shot lifecycle.
  # Forcing mkGuest's contract on the builder VM means baking ~30 MiB
  # of mvm-guest-agent into a rootfs that never talks to vsock. We
  # assemble the rootfs directly here and keep the image at the W2
  # size budget (≤ 300 MiB compressed, ≤ 1.2 GiB uncompressed).
  #
  # ── Where the content lives ─────────────────────────────────────────
  #
  # No inline heredocs, no embedded shell scripts. Every templated /
  # scripted artifact is its own checked-in file:
  #
  #   ./files/etc/{passwd,group,nsswitch.conf,profile}
  #     Static rootfs system files. Copied byte-for-byte.
  #
  #   ./files/manifest.template.json
  #     Placeholder-substituted at build time with sha256s + sizes.
  #
  #   ./files/assemble-rootfs.sh
  #     Rootfs tree assembly. Reads paths via env vars set in the
  #     runCommand's attrset.
  #
  #   ./files/assemble-image.sh
  #     Final image packaging (kernel copy + rootfs.ext4 copy +
  #     manifest.json substitution + size-budget enforcement).
  #
  # The init script itself ships under `nix/packages/mvm-builder-init.sh`
  # (W2 stub) / `crates/mvm-builder-init/` (W3 Rust binary) consumed
  # via `nix/packages/mvm-builder-init.nix`. One contract surface
  # (`$out/sbin/mvm-builder-init`); the W3 Rust binary swaps in without
  # touching this flake.

  inputs = {
    # Pin matches nix/images/builder/flake.nix so Layer 1 and Layer 2
    # share a nixpkgs snapshot. Bumping is a single coordinated change
    # to both flakes.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # Recommended kernel cmdline. Burned into manifest.json + exposed
      # at `passthru.kernelCmdline` so `LibkrunBuilderVm` reads it from
      # a single source of truth. See plan 72 W2 §"Kernel cmdline".
      #
      #   console=hvc0       → libkrun's virtio-console writes to the
      #                        host stdout; matches plan 57 W3.4.
      #   root=/dev/vda ro   → rootfs is virtio-blk-0, mounted RO.
      #   rootfstype=ext4    → skip kernel autodetection, faster boot.
      #   init=/sbin/...     → bypass any /init or /sbin/init in the
      #                        rootfs and exec our PID 1 directly.
      kernelCmdline = "console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/mvm-builder-init";

      mkBuilderVmImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          lib = nixpkgs.lib;

          # ── Slim package set per Plan 72 W2 §Inputs ────────────────
          #
          # bash + GNU coreutils + grep/sed/awk/find + nix + git +
          # gnumake + curl + jq + e2fsprogs + util-linux. Explicitly
          # NOT included: bashInteractive, less, which, iptables,
          # iproute2, procps — the builder VM runs non-interactively
          # and busybox covers the omitted utilities adequately for
          # nix's needs.
          builderPackages = with pkgs; [
            bash
            coreutils
            gnugrep
            gnused
            gawk
            findutils
            nix
            git
            gnumake
            curl
            jq
            e2fsprogs
            util-linux
          ];

          # PID 1. Plan 72 W2 ships the stub shell variant; W3 swaps
          # it for the Rust crate behind the same output contract.
          mvmBuilderInit = pkgs.callPackage ../../packages/mvm-builder-init.nix { };

          # Static busybox — single binary covers `mount`, `udhcpc`,
          # `mkdir`, `ln`, every shell utility mvm-builder-init.sh
          # invokes via `BB=/bin/busybox`.
          busybox = pkgs.pkgsStatic.busybox;

          # Rootfs tree — FHS skeleton + busybox + init + packages +
          # static /etc files. Script body lives in
          # ./files/assemble-rootfs.sh; this runCommand just hands it
          # all required paths via env vars.
          rootfsTree = pkgs.runCommand "mvm-builder-vm-rootfs-tree-${system}"
            {
              # Path-valued attrs become env vars in the build env.
              # `assemble-rootfs.sh` reads each one by name.
              inherit busybox mvmBuilderInit;
              passwdFile   = ./files/etc/passwd;
              groupFile    = ./files/etc/group;
              nsswitchFile = ./files/etc/nsswitch.conf;
              profileFile  = ./files/etc/profile;
              # Newline-separated list of store paths. The script
              # iterates with `while read`.
              builderPackagePaths = lib.concatStringsSep "\n" (map (p: "${p}") builderPackages);
            }
            "bash ${./files/assemble-rootfs.sh}";

          # ext4 packaging. `make-ext4-fs.nix` from nixpkgs handles
          # the mkfs + populate dance. Reference via `${nixpkgs}/...`
          # (not the angle-bracket form) — the latter trips flake
          # pure evaluation.
          rootfsImage = pkgs.callPackage "${nixpkgs}/nixos/lib/make-ext4-fs.nix" {
            storePaths = [ rootfsTree ];
            volumeLabel = "mvm-builder-vm";
            # Script body lives in ./files/populate-image.sh; passing
            # `rootfsTree` as an env var keeps the substitution out of
            # the .nix file.
            populateImageCommands = "rootfsTree=${rootfsTree} bash ${./files/populate-image.sh}";
          };

          kernelPkg = pkgs.linuxPackages.kernel;
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64
            then "Image"
            else "bzImage";
        in
        pkgs.runCommand "mvm-builder-vm-image-${system}"
          {
            passthru = {
              inherit rootfsTree kernelCmdline;
              inherit (mvmBuilderInit.passthru) isStub;
              kernel = kernelPkg;
              rootfs = rootfsImage;
            };
            # Inputs to ./files/assemble-image.sh. Each becomes an
            # env var of the same name in the build env.
            kernelPkg = kernelPkg;
            inherit kernelFile rootfsImage;
            coreutils        = pkgs.coreutils;
            gnused           = pkgs.gnused;
            gnugrep          = pkgs.gnugrep;
            manifestTemplate = ./files/manifest.template.json;
            templateSystem        = system;
            templateKernelCmdline = kernelCmdline;
            templateInitIsStub    = if mvmBuilderInit.passthru.isStub then "true" else "false";
          }
          "bash ${./files/assemble-image.sh}";
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderVmImage system;
      });
    };
}
