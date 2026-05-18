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
  # It carries no Nix store of its own — `nix-portable` is the
  # self-extracting Nix it uses to evaluate + build the builder-VM
  # flake. virtio-fs shares carry the workspace in and the resulting
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
  # | Component               | Source                              |
  # | ----------------------- | ----------------------------------- |
  # | `mvm-builder-init.musl` | this workspace, pkgsStatic build    |
  # | `busybox-static`        | `pkgs.pkgsStatic.busybox`           |
  # | `nix-portable`          | pinned upstream release + sha256    |
  # | kernel                  | libkrunfw-bundled (host-side)       |
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

      # nix-portable pin. Bump in lockstep with pins.json.
      nixPortablePin = {
        version = "v012";
        urls = {
          # Upstream releases are arch-suffixed binaries. We pin both
          # so a single tarball build can produce either-arch output
          # from either-arch CI runner.
          "aarch64-linux" = {
            url = "https://github.com/DavHau/nix-portable/releases/download/v012/nix-portable-aarch64";
            sha256 = "af41d8defdb9fa17ee361220ee05a0c758d3e6231384a3f969a314f9133744ea";
          };
          "x86_64-linux" = {
            url = "https://github.com/DavHau/nix-portable/releases/download/v012/nix-portable-x86_64";
            sha256 = "b409c55904c909ac3aeda3fb1253319f86a89ddd1ba31a5dec33d4a06414c72a";
          };
        };
      };

      nixPortableFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          spec = nixPortablePin.urls.${system};
        in
        # Flat-hash fetch (file content sha256). `executable = true`
        # would switch to NAR-based hashing — we want the pin to match
        # the bytes one would get from `curl + sha256sum`, so the chmod
        # happens in the buildCommand below instead.
        pkgs.fetchurl {
          url = spec.url;
          sha256 = spec.sha256;
        };

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
          nixPortable = nixPortableFor system;
          urSeedInit = urSeedInitFor system;

          # Stock nixpkgs kernel — matches the dev image flake's kernel
          # (`nix/images/builder/flake.nix:150`) so an ur-seed booted
          # with the dev image's vmlinux finds compatible
          # `/lib/modules/<kver>/` entries. The TSI-patched builder VM
          # kernel (`nix/images/builder-vm/kernel/`) doesn't change
          # the version, only the patches — same module tree applies.
          kernelPkg = pkgs.linuxPackages.kernel;
          kernelModules =
            if kernelPkg ? modules then kernelPkg.modules else kernelPkg;

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
            ur_seed_version = "0.1.0";
            nix_portable_pin = nixPortablePin.version;
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

            # nix-portable + a `nix` wrapper so the existing Plan 77
            # Stage 0 cmd.sh ("nix build path:/work#...") finds an
            # invokable `nix` on PATH. nix-portable dispatches based
            # on argv[0] when symlinked, but explicit wrappers are
            # cheaper to reason about than version-dependent behavior.
            cp ${nixPortable} $staging/usr/local/bin/nix-portable
            chmod +x $staging/usr/local/bin/nix-portable

            cat > $staging/usr/local/bin/nix <<'WRAPPER'
            #!/bin/sh
            exec /usr/local/bin/nix-portable nix "$@"
            WRAPPER
            chmod +x $staging/usr/local/bin/nix

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

            # rootfs.ext4 sized at 768 MiB — fits the kernel module
            # tree (~200 MiB), nix-portable (~80 MiB), busybox (~1 MiB),
            # plus working room for nix-portable's HOME extraction.
            truncate -s 768M $out/rootfs.ext4
            mkfs.ext4 -F -d $staging -L mvm-ur-seed $out/rootfs.ext4

            # Out-of-band sidecars the host needs.
            cp ${manifestJson}      $out/manifest.json
            cp ${urSeedCmdlineFile} $out/cmdline.txt

            # Pack the artifact set into a single tarball for
            # `mvmctl dev fetch-ur-seed` / `import-ur-seed`.
            mkdir $out/tarball
            cp $out/rootfs.ext4    $out/tarball/
            cp $out/manifest.json  $out/tarball/
            cp $out/cmdline.txt    $out/tarball/
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
