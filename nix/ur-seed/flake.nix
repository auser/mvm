{
  description = "mvm ur-seed — Stage -1 bootstrap rootfs (Plan 86)";

  # ── Why this flake exists ────────────────────────────────────────────
  #
  # Plan 77 W5 introduced a seed contract for Stage 0
  # (`bootstrap_builder_vm_image_via_dev_image_stage0` requires
  # `/sbin/mvm-builder-init` in the seed dev image). Contributor hosts
  # whose dev image pre-dates W5 hit a catch-22: they need a builder
  # VM to build a dev image, a dev image to bootstrap a builder VM,
  # and neither path is auto-downloadable per the
  # `feedback_no_prebuilt_builder_vm_artifact.md` memory.
  #
  # The ur-seed is Stage -1: a minimal aarch64-linux rootfs that exists
  # only to run `nix build` against `nix/images/builder-vm/flake.nix`.
  # It ships the same runtime package closure as the steady-state
  # builder VM (real `nix`, `bash`, `iptables`, …) plus a
  # TSI-patched kernel, so `mvm-builder-init`'s in-VM dispatch
  # (`/job/cmd.sh` → `nix build path:/work#…`) runs unchanged.
  # virtio-fs shares carry the workspace in and the resulting
  # vmlinux/rootfs out.
  #
  # ── Acquisition model (Plan 86 Shape C) ──────────────────────────────
  #
  # The ur-seed tarball is built by this flake ONLY:
  #   - At release time on CI (publishes to GitHub releases).
  #   - Manually by a contributor with access to a Linux+Nix env, for
  #     local bootstrap when no release is available yet.
  #
  # The tarball is fetched by `mvmctl dev fetch-ur-seed` (explicit,
  # opt-in) or installed by `mvmctl dev import-ur-seed` (air-gapped).
  # `mvmctl dev up` NEVER auto-fetches.
  #
  # ── ADR-046 invariance ───────────────────────────────────────────────
  #
  # Edits to `nix/images/builder-vm/flake.nix` invalidate the builder
  # VM cache but do NOT invalidate the ur-seed — the ur-seed runs the
  # current flake from the workspace each invocation. Edits to
  # `crates/mvm-builder-init/` invalidate the builder VM image (where
  # it's rebuilt) but do NOT invalidate the ur-seed's bundled
  # `mvm-builder-init`, which is release-frozen. This trade-off is
  # documented in ADR-054.
  #
  # ── Components ───────────────────────────────────────────────────────
  #
  # | Component               | Source                                |
  # | ----------------------- | ------------------------------------- |
  # | `mvm-builder-init.musl` | this workspace, pkgsStatic build      |
  # | `busybox-static`        | `pkgs.pkgsStatic.busybox`             |
  # | runtime closure         | mirrors `nix/images/builder-vm`'s pkgs|
  # | kernel + modules        | `nix/images/builder-vm/kernel` (TSI)  |
  #
  # The rootfs is produced as ext4 (not initramfs) to reuse the
  # existing Stage 0 boot path in `mvm-build/src/libkrun_builder.rs`
  # unchanged.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      workspaceRoot =
        let
          envPath = builtins.getEnv "MVM_WORKSPACE_PATH";
        in
        if envPath != "" then /. + envPath else ../..;

      workspace = builtins.path {
        path = workspaceRoot;
        name = "mvm-workspace";
        filter =
          path: _type:
          let
            base = baseNameOf path;
          in
          !(builtins.elem base [
            "target"
            ".git"
            "result"
            "node_modules"
            ".direnv"
            ".cargo"
            ".claude"
            ".worktrees"
          ])
          && !(nixpkgs.lib.hasPrefix "result-" base);
      };

      # Builder-VM runtime packages — mirrors the curated set in
      # `nix/images/builder-vm/flake.nix` so the ur-seed has the same
      # runtime shape as the steady-state builder VM. Includes real
      # `nix` (not nix-portable), so cmd.sh's `nix build` works
      # unchanged. The closure is large (~400 MiB rootfs) but
      # produces a single-bootstrap path that doesn't need to grow
      # piecemeal as new in-VM tools surface (bash, env, proot, …).
      urSeedPackages =
        pkgs: with pkgs; [
          bashInteractive
          coreutils
          gnugrep
          gnused
          gawk
          findutils
          which
          nix
          git
          gnumake
          curl
          jq
          iproute2
          # iptables-legacy, not the nft-backed default. The
          # libkrunfw-bundled kernel ships without CONFIG_NF_TABLES, so
          # `iptables-nft` bails with "Failed to initialize nft: Protocol
          # not supported" at the first `iptables -A` call. Legacy
          # iptables works against the older kernel netfilter ABI which
          # the bundled kernel DOES carry.
          iptables-legacy
          e2fsprogs
          util-linux
        ];

      # mvm-builder-init, statically linked against musl. Embedded
      # in the ur-seed at /sbin/mvm-builder-init as PID 1.
      mvmBuilderInitStaticFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.pkgsStatic.rustPlatform.buildRustPackage {
          pname = "mvm-builder-init-ur-seed";
          version = "0.14.0";
          src = workspace;
          cargoLock = {
            lockFile = workspace + "/Cargo.lock";
          };
          buildAndTestSubdir = "crates/mvm-builder-init";
          doCheck = false;
          meta = {
            description = "PID-1 for the ur-seed bootstrap rootfs (Plan 86)";
            mainProgram = "mvm-builder-init";
          };
        };

      # Minimal ur-seed init. POSIX shell, busybox applets only.
      # Stages /proc, /sys, /dev, /tmp, /run; mounts virtio-fs shares
      # at /work (workspace) and /out (artifact output); execs
      # nix-portable to build the builder-vm flake; copies the result
      # to /out; halts cleanly.
      #
      # The "real" mvm-builder-init binary lives at
      # /sbin/mvm-builder-init for the Plan 77 W5 seed contract check
      # to find it, but in the ur-seed path the kernel cmdline pins
      # `init=/sbin/ur-seed-init` so this script is what actually runs.
      # (Stage 0 in `libkrun_builder.rs` will set the right cmdline
      # when booting an ur-seed.)
      urSeedInitFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.writeScript "ur-seed-init" ''
          #!/bin/sh
          # mvm ur-seed init (Plan 86).
          set -e

          /bin/busybox mount -t proc     proc     /proc
          /bin/busybox mount -t sysfs    sysfs    /sys
          /bin/busybox mount -t devtmpfs devtmpfs /dev || true
          /bin/busybox mount -t tmpfs -o mode=1777 tmpfs /tmp
          /bin/busybox mount -t tmpfs -o mode=0755 tmpfs /run

          /bin/busybox mkdir -p /work /out

          # virtio-fs shares mounted by mvm-libkrun before init runs:
          #   work_share -> /work   (workspace, ro)
          #   out_share  -> /out    (artifact output, rw)
          # If they aren't mounted, fail loudly so the host log surfaces it.
          if ! /bin/busybox mountpoint -q /work; then
            echo "ur-seed-init: /work is not a mountpoint; aborting." >&2
            exit 64
          fi
          if ! /bin/busybox mountpoint -q /out; then
            echo "ur-seed-init: /out is not a mountpoint; aborting." >&2
            exit 65
          fi

          # nix-portable needs a writable HOME for its self-extraction
          # cache. We give it /tmp/np-home; the cache is discarded with
          # the VM.
          export HOME=/tmp/np-home
          /bin/busybox mkdir -p "$HOME"

          # The ur-seed's sole job: run `nix build path:/work#default`
          # against the builder-vm flake. The flake path inside the
          # workspace is fixed.
          BUILDER_VM_FLAKE_PATH="/work/nix/images/builder-vm"
          if [ ! -f "$BUILDER_VM_FLAKE_PATH/flake.nix" ]; then
            echo "ur-seed-init: $BUILDER_VM_FLAKE_PATH/flake.nix not found" >&2
            exit 66
          fi

          echo "ur-seed-init: invoking nix-portable build of builder-vm flake" >&2
          /usr/local/bin/nix-portable nix \
            --extra-experimental-features "nix-command flakes" \
            build \
            "path:$BUILDER_VM_FLAKE_PATH#packages.$(uname -m)-linux.default" \
            --out-link /tmp/result \
            --print-build-logs

          # Copy artifacts to /out so the host can promote them.
          /bin/busybox cp -L /tmp/result/vmlinux        /out/vmlinux
          /bin/busybox cp -L /tmp/result/rootfs.ext4    /out/rootfs.ext4
          if [ -f /tmp/result/cmdline.txt ]; then
            /bin/busybox cp -L /tmp/result/cmdline.txt /out/cmdline.txt
          fi
          if [ -f /tmp/result/manifest.json ]; then
            /bin/busybox cp -L /tmp/result/manifest.json /out/manifest.json
          fi

          /bin/busybox sync
          echo "ur-seed-init: build complete, halting." >&2
          /bin/busybox poweroff -f
        '';

      # Plan 77 W5 seed-contract sentinel: this exists so the byte-scan
      # check (`file_contains_bytes(rootfs, b"/sbin/mvm-builder-init")`)
      # passes against ur-seed rootfs.ext4 the same way it does against
      # a dev image. The ur-seed's actual PID 1 is /sbin/ur-seed-init
      # (set via kernel cmdline by Stage 0); /sbin/mvm-builder-init is
      # present as a symlink for the contract check + as a viable PID 1
      # if Stage 0 doesn't override cmdline.
      mkUrSeedRootfs =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          builderInit = mvmBuilderInitStaticFor system;
          busybox = pkgs.pkgsStatic.busybox;
          urSeedInit = urSeedInitFor system;

          # TSI-patched kernel from nix/images/builder-vm/kernel/.
          # libkrun routes AF_INET sockets through TSI (Transparent
          # Socket Impersonation), which requires guest-side kernel
          # patches that aren't upstream. The stock nixpkgs kernel —
          # what the dev image flake ships today — makes `nix build`
          # fail with "Could not resolve host" inside the guest
          # (Plan 72 W5.D bullet 10).
          #
          # The ur-seed ships its own TSI kernel + matching module
          # tree alongside the rootfs so Stage 0 has a self-contained
          # bootable environment, independent of whatever kernel the
          # contributor's dev image happens to ship.
          kernelPkg = import (workspace + "/nix/images/builder-vm/kernel") { inherit pkgs; };
          kernelModules =
            if kernelPkg ? modules then kernelPkg.modules else kernelPkg;

          # Full closure of runtime packages mirroring the steady-state
          # builder VM. The closure is symlinked into /usr/local/bin
          # and /sbin (Plan 72 W5.D bullets 4 + 5) so absolute-path
          # call sites + PATH lookups both resolve.
          packages = urSeedPackages pkgs;

          # closureInfo materialises the complete /nix/store closure
          # for the runtime packages as a derivation containing a
          # `store-paths` file the build script can iterate over.
          # This is the Nix-sandbox-friendly way to enumerate the
          # transitive closure (the `nix-store --query --requisites`
          # path needs daemon access which the sandbox forbids).
          packagesClosure = pkgs.closureInfo {
            rootPaths = packages;
          };

          # Manifest mirrors the dev-image manifest shape so the
          # existing seed-contract validator can read either source
          # without dispatch logic. schema_version + contract_version
          # are pinned to the values Plan 77 W5 validates against.
          # Manifest matches the Plan 77 W5 seed-contract schema. The
          # ur-seed-specific fields (`origin`, `ur_seed_version`,
          # `nix_portable_pin`) are additive — the seed-contract validator
          # ignores unknown fields and only enforces schema_version,
          # contract_version, image_kind, and init_paths.
          manifestJson = pkgs.writeText "manifest.json" (builtins.toJSON {
            schema_version = 1;
            contract_version = 2;
            image_kind = "ur-seed";
            init_paths = [ "/sbin/mvm-builder-init" "/sbin/ur-seed-init" ];
            origin = "ur-seed";
            ur_seed_version = "0.2.0";
            system = system;
          });

          passwdFile = pkgs.writeText "passwd" ''
            root:x:0:0:root:/root:/bin/sh
            nobody:x:65534:65534:nobody:/nonexistent:/bin/false
          '';
          groupFile = pkgs.writeText "group" ''
            root:x:0:
            nobody:x:65534:
          '';
          nsswitchFile = pkgs.writeText "nsswitch.conf" ''
            passwd: files
            group: files
            shadow: files
            hosts: files dns
          '';
          resolvFile = pkgs.writeText "resolv.conf" ''
            nameserver 1.1.1.1
            nameserver 8.8.8.8
          '';
          urSeedCmdlineFile = pkgs.writeText "cmdline.txt"
            "console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/ur-seed-init";
        in
        pkgs.runCommand "mvm-ur-seed-${system}"
          {
            nativeBuildInputs = with pkgs; [ e2fsprogs gnutar coreutils ];
          }
          ''
            mkdir -p $out
            staging=$(mktemp -d)

            # Filesystem skeleton. /nix-store, /job, /work, /out exist
            # as pre-created directories so mvm-builder-init's bind +
            # mkdir calls don't fail on the RO rootfs (per Plan 72 W5.D
            # bullet 3 — same fix the dev image flake got).
            mkdir -p $staging/bin $staging/sbin $staging/etc $staging/proc \
                     $staging/sys $staging/dev $staging/tmp $staging/run \
                     $staging/work $staging/out $staging/job $staging/nix-store \
                     $staging/nix $staging/etc/mvm $staging/usr/local/bin \
                     $staging/lib/modules

            # busybox + applets. Symlink into BOTH /bin AND /sbin per
            # Plan 72 W5.D bullets 4 + 5: mvm-builder-init invokes
            # `/sbin/mkfs.ext4`, `/sbin/udhcpc`, etc. as well as /bin tools.
            cp ${busybox}/bin/busybox $staging/bin/busybox
            chmod +x $staging/bin/busybox
            for applet in sh mount umount cp mkdir mountpoint poweroff sync \
                          uname cat ls echo modprobe ifconfig ip route \
                          udhcpc hostname true false stat readlink ln rm \
                          chmod chown find xargs grep awk sed cut head tail \
                          wc sleep kill ps env tar gunzip gzip; do
              ln -s busybox $staging/bin/$applet
              ln -s ../bin/busybox $staging/sbin/$applet
            done

            # mvm-builder-init (Plan 77 W5 seed contract) — present as
            # /sbin/mvm-builder-init so the contract check passes; the
            # ur-seed path overrides cmdline to use /sbin/ur-seed-init
            # instead, but the binary is still callable.
            cp ${builderInit}/bin/mvm-builder-init $staging/sbin/mvm-builder-init
            chmod +x $staging/sbin/mvm-builder-init

            # Plan 86 ur-seed PID 1.
            cp ${urSeedInit} $staging/sbin/ur-seed-init
            chmod +x $staging/sbin/ur-seed-init

            # Stage the full Nix closure for the runtime packages
            # under /nix/store so dynamically-linked binaries resolve
            # their dependencies. closureInfo's `store-paths` file
            # enumerates the transitive closure inside the sandbox.
            mkdir -p $staging/nix/store
            while read -r dep; do
              if [ ! -e "$staging$dep" ]; then
                mkdir -p "$staging$(dirname "$dep")"
                cp -aL "$dep" "$staging$dep"
              fi
            done < ${packagesClosure}/store-paths

            # Plan 72 W5.D bullet 4 + 5: symlink every root package's
            # bin/* and sbin/* into BOTH /usr/local/bin and /sbin so
            # both absolute-path call sites (`/sbin/mkfs.ext4`) and
            # PATH lookups (`Command::new("iptables")`) resolve.
            for pkg in ${pkgs.lib.concatStringsSep " " (map (p: ''"${p}"'') packages)}; do
              for subdir in bin sbin; do
                if [ -d "$pkg/$subdir" ]; then
                  for bin in "$pkg/$subdir"/*; do
                    [ -e "$bin" ] || continue
                    name=$(basename "$bin")
                    # Don't overwrite earlier package's binary if it
                    # already exists — first-package-wins keeps the
                    # symlink target stable.
                    [ -e "$staging/usr/local/bin/$name" ] || \
                      ln -sf "$bin" "$staging/usr/local/bin/$name"
                    [ -e "$staging/sbin/$name" ] || \
                      ln -sf "$bin" "$staging/sbin/$name"
                  done
                fi
              done
            done

            # /usr/bin/env — many scripts (including nix's wrappers
            # and shebang-driven helpers) use `#!/usr/bin/env <prog>`.
            mkdir -p $staging/usr/bin
            ln -sf /bin/busybox $staging/usr/bin/env

            # /etc minimal config (libc resolvers + nix-portable's
            # internal getpwuid work even before any real /etc setup).
            cp ${passwdFile}   $staging/etc/passwd
            cp ${groupFile}    $staging/etc/group
            cp ${nsswitchFile} $staging/etc/nsswitch.conf
            cp ${resolvFile}   $staging/etc/resolv.conf
            cp ${manifestJson} $staging/etc/mvm-ur-seed.json

            # Kernel modules. mvm-builder-init runs
            # `modprobe virtiofs fuse` before mounting the host shares;
            # without the module tree those modprobe calls no-op and
            # the mount returns ENODEV (Plan 72 W5.D bullet 8). The
            # stock nixpkgs kernel ships virtio_fs + fuse as `=m`.
            for src in ${kernelModules}/lib/modules/*; do
              if [ -d "$src" ]; then
                kver=$(basename "$src")
                mkdir -p "$staging/lib/modules/$kver"
                cp -aL "$src/." "$staging/lib/modules/$kver/"
                # Drop build-machine source/build symlinks pointing
                # into store dev outputs the guest can't resolve.
                rm -f "$staging/lib/modules/$kver/source" \
                      "$staging/lib/modules/$kver/build" || true
              fi
            done

            # rootfs.ext4 sized at 2 GiB — fits the runtime package
            # closure (nix + bash + e2fsprogs + iptables + glibc =
            # ~400 MiB), the kernel module tree (~200 MiB), busybox
            # (~1 MiB), plus working room. Sparse-allocated, so the
            # tarball stays compressed-small.
            truncate -s 2G $out/rootfs.ext4
            mkfs.ext4 -F -d $staging -L mvm-ur-seed $out/rootfs.ext4

            # Out-of-band sidecars the host needs.
            cp ${manifestJson}      $out/manifest.json
            cp ${urSeedCmdlineFile} $out/cmdline.txt

            # TSI-patched kernel — ship alongside the rootfs so Stage 0
            # has a self-contained, libkrun-compatible boot pair.
            kernelFile=
            for cand in Image bzImage; do
              if [ -f "${kernelPkg}/$cand" ]; then
                kernelFile="${kernelPkg}/$cand"
                break
              fi
            done
            if [ -z "$kernelFile" ]; then
              echo "kernel package ${kernelPkg} did not produce Image or bzImage" >&2
              exit 1
            fi
            cp "$kernelFile" $out/vmlinux

            # Pack the artifact set into a single tarball for
            # `mvmctl dev fetch-ur-seed` / `import-ur-seed`.
            mkdir $out/tarball
            cp $out/rootfs.ext4    $out/tarball/
            cp $out/manifest.json  $out/tarball/
            cp $out/cmdline.txt    $out/tarball/
            cp $out/vmlinux        $out/tarball/
            tar -C $out/tarball -czf $out/ur-seed-${system}.tar.gz .

            # Emit a sha256 sidecar — fetch-ur-seed verifies against it.
            sha256sum $out/ur-seed-${system}.tar.gz | awk '{print $1}' \
              > $out/ur-seed-${system}.tar.gz.sha256
          '';
    in
    {
      packages = forAllSystems (system: {
        default = mkUrSeedRootfs system;
        ur-seed = mkUrSeedRootfs system;
      });
    };
}
