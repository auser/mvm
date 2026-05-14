{
  description = "mvm builder VM image — Linux kernel + ext4 rootfs running mvm-builder-init as PID 1";

  # ── What this flake is ────────────────────────────────────────────
  #
  # Plan 72 W2 / ADR-046. The "builder VM" is the Layer-1 artifact a
  # contributor (or end user) boots to run `nix build` against the
  # in-repo Layer-2 flakes (the dev shell, user `--flake` images, etc.).
  # Boots under libkrun on macOS and Firecracker on Linux. The host
  # attaches three virtio-fs shares (the workspace at /work, a writable
  # /out, and a job dir with cmd.sh + result), one virtio-blk holding
  # the persistent /nix store, and a virtio-net link. mvm-builder-init
  # mounts everything, runs `/job/cmd.sh`, writes the exit code to
  # `/job/result`, and powers the VM off.
  #
  # ── How it differs from the existing nix/images/builder/ flake ────
  #
  # The existing `nix/images/builder/` is the Layer-2 image users boot
  # (and W5 of plan 72 renames it to `nix/images/dev-shell/` so the
  # distinction is grep-obvious). This `nix/images/builder-vm/` flake
  # is Layer 1: it's purpose-built to run `nix build`, never run by an
  # end user directly.
  #
  # Differences:
  #
  #   • Slim package set — no iptables, no procps, no `less`. Just the
  #     tools `mvm-builder-init` shells out to (busybox applets) plus
  #     what `nix build` invokes (nix, git, curl, jq, gnumake, etc.).
  #   • mvm-builder-init at /usr/local/bin/mvm-builder-init (via
  #     mkGuest's package symlink hook). Kernel cmdline `init=`
  #     points at that path; mkGuest's own /init script never runs.
  #   • Sealed (`dev = false`) so mvm-guest-agent strips the do_exec
  #     handler — the builder VM has no need for interactive RPCs.
  #   • Emits `$out/cmdline` + `$out/manifest.json` alongside vmlinux
  #     + rootfs.ext4 so the launcher reads the cmdline string from a
  #     file instead of hard-coding it host-side.
  #
  # ── Acquisition rule (ADR-046 §"Two artifact layers") ────────────
  #
  # In a source checkout, `mvmctl dev up` resolves this flake locally
  # via `find_builder_vm_flake()` (plan 72 W5) and builds it from
  # source. Outside a source checkout, `mvmctl` downloads the matching
  # release artifacts (vmlinux, rootfs.ext4, cmdline, manifest.json) +
  # SHA-256 verifies via the manifest. The release workflow
  # (`.github/workflows/release.yml`'s `builder-vm-image` job) is the
  # source of those artifacts.
  #
  # ── Workspace staging (mirrors nix/images/builder/flake.nix) ──────
  #
  # Skip the `mvm.url = "path:../.."` flake input because path inputs
  # can't be locked by content hash and the lock can't be rewritten on
  # a read-only sandbox mount. Stage the workspace once via
  # `builtins.path` (filtered to exclude target/, .git/, etc.) and
  # `import` `nix/lib/default.nix` directly. Same shape `lib.<system>`
  # would produce, no flake input → no lock validation failure.

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

      # Kernel cmdline — emitted as `$out/cmdline` and consumed by the
      # launcher (plan 72 W1). Centralising the string here means a
      # cmdline tweak (e.g. enabling additional kernel debug output)
      # is a one-file change rather than a host/guest contract update.
      #
      # `init=/usr/local/bin/mvm-builder-init` is where mkGuest puts
      # `packages` binaries via its symlink hook. The plan-72 W2 spec
      # text named `/sbin/mvm-builder-init`; using the existing
      # mkGuest install path avoids extending mkGuest with a separate
      # sbin-install branch just for this image.
      kernelCmdline = "console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/usr/local/bin/mvm-builder-init";

      # See `nix/images/builder/flake.nix` for the rationale for env-var
      # override; the same logic applies here — when the host's
      # `LibkrunBuilderVm` runs `nix build` inside an in-flight builder
      # VM (the bootstrap case), the workspace is bind-mounted at /work
      # and `MVM_WORKSPACE_PATH=/work` redirects `builtins.path`. The
      # path filter is the same conservative exclude list — none of
      # those dirs are needed to build mvm-builder-init or the rootfs.
      workspaceRoot =
        let
          envPath = builtins.getEnv "MVM_WORKSPACE_PATH";
        in
        if envPath != "" then /. + envPath else ../../..;

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

      libFor = import (workspace + "/nix/lib") {
        inherit nixpkgs microvm;
        mvmSrc = workspace;
      };

      # Slim builder-VM package set — only what `nix build` and
      # `mvm-builder-init` actually exec. Compare with the dev image's
      # `builderPackages` in `nix/images/builder/flake.nix` which adds
      # `bashInteractive`, `which`, `less`, `iproute2`, `iptables`, and
      # `procps` for interactive use. None of those are needed for an
      # ephemeral build appliance — `iproute2` is replaced by busybox's
      # `ip` applet (mvm-builder-init invokes `/bin/ip` directly).
      builderVmPackages = pkgs: with pkgs; [
        coreutils gnugrep gnused gawk findutils
        nix git gnumake curl jq e2fsprogs util-linux
      ];

      mkBuilderVmImage = system:
        let
          pkgs = import nixpkgs { inherit system; };

          # mvm-builder-init Rust binary, built from the workspace at
          # `mvmSrc` via `nix/packages/mvm-builder-init.nix`. The
          # closure lands in the rootfs's `/nix/store`; mkGuest's
          # `packages` symlink loop puts the bin at
          # `/usr/local/bin/mvm-builder-init` which the kernel
          # cmdline's `init=` resolves to.
          builderInitPkg = pkgs.callPackage ../../packages/mvm-builder-init.nix {
            mvmSrc = workspace;
          };

          rootfs = (libFor { inherit system; }).mkGuest {
            name = "mvm-builder";
            # `command` entrypoint = sealed mode → mvm-guest-agent
            # strips the do_exec handler. The entrypoint itself is dead
            # code because the kernel cmdline's `init=` bypasses
            # mkGuest's /init script; pointing it at the same binary
            # is just defensive (a future caller booting without the
            # cmdline override would still end up running the right
            # thing).
            entrypoint.command = [ "/usr/local/bin/mvm-builder-init" ];
            packages = (builderVmPackages pkgs) ++ [ builderInitPkg ];
            # The builder VM gets generous resources — `nix build` is
            # CPU + memory heavy. The launcher (`LibkrunBuilderVm`)
            # can override these via its own KrunContext shape.
            vcpus = 4;
            memory_mib = 4096;
          };

          kernelPkg = pkgs.linuxPackages.kernel;
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64
            then "Image"
            else "bzImage";
        in
        pkgs.runCommand "mvm-builder-vm-${system}"
          {
            passthru = {
              inherit rootfs;
              kernel = kernelPkg;
              builderInit = builderInitPkg;
              inherit (rootfs.passthru) mvm;
              cmdline = kernelCmdline;
            };
            # Hard fail in CI if the rootfs balloons past plan 72 W2's
            # 1.2 GiB uncompressed budget. Caught here so the release
            # workflow never uploads an over-budget artifact.
            rootfsMaxBytes = 1258291200; # 1.2 GiB
          }
          ''
            mkdir -p $out

            # Kernel — copy whichever Image/bzImage the arch produces
            # to a stable `$out/vmlinux` name. The launcher consumes
            # `<image>/vmlinux` regardless of arch.
            if [ -f ${kernelPkg}/${kernelFile} ]; then
              cp ${kernelPkg}/${kernelFile} $out/vmlinux
            elif [ -f ${kernelPkg}/Image ]; then
              cp ${kernelPkg}/Image $out/vmlinux
            elif [ -f ${kernelPkg}/bzImage ]; then
              cp ${kernelPkg}/bzImage $out/vmlinux
            else
              echo "kernel package ${kernelPkg} produced no Image/bzImage" >&2
              ls -la ${kernelPkg} >&2
              exit 1
            fi

            # Rootfs — mkGuest emits either a file directly or a
            # directory containing *.img / *.ext4. Same handling as the
            # dev image flake.
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

            # Size budget check — fail the build before the artifact
            # gets uploaded if we've blown past the 1.2 GiB target.
            actual=$(${pkgs.coreutils}/bin/stat -c %s $out/rootfs.ext4)
            if [ "$actual" -gt "$rootfsMaxBytes" ]; then
              echo "FATAL: rootfs.ext4 size $actual bytes exceeds budget $rootfsMaxBytes bytes (plan 72 W2)" >&2
              echo "       trim builderVmPackages or audit the closure to fit." >&2
              exit 1
            fi

            # Kernel cmdline — emitted as a file so the launcher reads
            # the string from a known location instead of hard-coding it.
            printf '%s\n' '${kernelCmdline}' > $out/cmdline

            # Manifest — sha256 + size for each artifact, plus the
            # cmdline string for cross-checking. Read by
            # `download_builder_vm_image` (plan 72 W5) to verify a
            # release-downloaded image matches the manifest checksum.
            kernel_sha=$(${pkgs.coreutils}/bin/sha256sum $out/vmlinux | ${pkgs.coreutils}/bin/cut -d' ' -f1)
            rootfs_sha=$(${pkgs.coreutils}/bin/sha256sum $out/rootfs.ext4 | ${pkgs.coreutils}/bin/cut -d' ' -f1)
            cmdline_sha=$(${pkgs.coreutils}/bin/sha256sum $out/cmdline | ${pkgs.coreutils}/bin/cut -d' ' -f1)
            kernel_size=$(${pkgs.coreutils}/bin/stat -c %s $out/vmlinux)
            rootfs_size=$(${pkgs.coreutils}/bin/stat -c %s $out/rootfs.ext4)
            cmdline_size=$(${pkgs.coreutils}/bin/stat -c %s $out/cmdline)
            cat > $out/manifest.json <<MANIFEST
            {
              "schema_version": 1,
              "artifact": "mvm-builder-vm",
              "system": "${system}",
              "cmdline": "${kernelCmdline}",
              "files": {
                "vmlinux":     { "sha256": "$kernel_sha",  "size": $kernel_size },
                "rootfs.ext4": { "sha256": "$rootfs_sha",  "size": $rootfs_size },
                "cmdline":     { "sha256": "$cmdline_sha", "size": $cmdline_size }
              }
            }
            MANIFEST
            chmod 0644 $out/cmdline $out/manifest.json
          '';
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderVmImage system;
      });
    };
}
