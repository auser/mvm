{
  description = "mvm builder VM image — kernel + rootfs.ext4 with Nix + tools + mvm-builder-init (Plan 72 W2)";

  # ── Why this flake exists ────────────────────────────────────────────
  #
  # Plan 72 (ADR-046) replaces the libkrun-backed builder VM
  # (`nix/images/builder/`, which is actually the dev-shell image
  # despite the name) with a libkrun-direct launcher
  # (`LibkrunBuilderVm`, Plan 72 W1). This flake is the artifact
  # `LibkrunBuilderVm` boots into: a small Linux kernel + ext4
  # rootfs containing Nix + a curated build-tools subset +
  # `mvm-builder-init` (Plan 72 W3) at `/sbin/mvm-builder-init`.
  #
  # `packages.<system>.default` produces `$out/{vmlinux,rootfs.ext4,
  # cmdline.txt,manifest.json}`. CI (Plan 72 W2 release-workflow
  # follow-up) uploads these as `builder-vmlinux-<arch>` and
  # `builder-rootfs-<arch>.ext4` alongside the existing dev-image
  # outputs.
  #
  # Distinct from `nix/images/builder/flake.nix` which produces the
  # dev-shell image (`mvm-dev`) — the rootfs a user `dev shell`s
  # into. The names will reshuffle in Plan 72 W6 hygiene; for now
  # the two flakes coexist and `mvmctl dev up` will pick the right
  # one via `find_builder_vm_flake` / `find_dev_image_flake`.
  #
  # ── Architecture / workspace staging ──────────────────────────────
  #
  # Identical pattern to `nix/images/builder/flake.nix`:
  #
  # - Stage the workspace via `builtins.path` (filter out `target/`,
  #   `.git/`, etc.) so the flake works both on a host running
  #   `nix build` directly and inside the libkrun builder VM's
  #   `path:` URL fetch (W4).
  # - `MVM_WORKSPACE_PATH` env var override for the sandbox case
  #   (avoids the `../../..` resolution-against-store-copy trap
  #   that bit `nix/images/builder/flake.nix` in Plan 72 W0).
  # - Import the parent flake's `nix/lib/` directly (skip flake-
  #   input chain → no path-input lock validation issue).
  #
  # ── Builder VM package set ────────────────────────────────────────
  #
  # Per Plan 72 §W2, narrower than the dev-shell image:
  #
  # - Static busybox (provides `/bin/sh`, `udhcpc`, `sync`, basic
  #   POSIX utilities — small footprint).
  # - Nix (the whole point of the VM).
  # - Bash + coreutils + gnugrep / gnused / gawk / findutils / which
  #   (user's `cmd.sh` is shell, not necessarily POSIX-only).
  # - Git + gnumake + curl + jq (Nix flakes pull from git, builds
  #   often run make / curl, `cmd.sh` may format JSON).
  # - e2fsprogs (`mkfs.ext4` for the first-boot format of the
  #   persistent `/nix` store) + util-linux (`mount`, `umount`,
  #   `losetup`).
  # - iproute2 (used by `udhcpc` and friends; small).
  # - iptables — Plan 73 Followup B.2.y / ADR-047 defense-in-
  #   depth: `mvm-builder-init` installs an OUTPUT-chain
  #   default-deny + uid-owner ACCEPT for `mvm-egress-proxy`
  #   (uid 1801), so a build step that ignores `HTTP_PROXY`
  #   cannot reach upstream. See
  #   `crates/mvm-builder-init/src/network.rs`.
  # - **No** `procps`-interactive / `less` per the plan's
  #   slimming directive.
  # - `mvm-builder-init` mounted at `/sbin/mvm-builder-init` via
  #   `extraFiles`. The kernel cmdline (`cmdline.txt` output)
  #   sets `init=/sbin/mvm-builder-init` so this becomes PID 1.

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

      workspaceRoot =
        let
          envPath = builtins.getEnv "MVM_WORKSPACE_PATH";
        in
        if envPath != "" then /. + envPath else ../../..;

      # Filter list lives at nix/lib/workspace-filter.nix so the three
      # flakes that ingest the host workspace (this one, builder/,
      # runtime-overlay/) stay aligned with .gitignore in one place.
      workspace =
        (import (workspaceRoot + "/nix/lib/workspace-filter.nix") {
          inherit (nixpkgs) lib;
        })
        { inherit workspaceRoot; };

      libFor = import (workspace + "/nix/lib") {
        inherit nixpkgs microvm;
        mvmSrc = workspace;
      };

      # ADR-050 / issue #223 — veritysetup sidecar bytes must not drift
      # when nixpkgs revs. The OCI-pull path runs `veritysetup format`
      # inside this builder VM, while the Nix-built baseline runs it in
      # `nix/images/runtime-overlay/flake.nix`. Both flakes intentionally
      # pin the same cryptsetup release + tarball hash, and both must be
      # reviewed together on bump.
      pinnedCryptsetupVersion = "2.8.6";
      pinnedCryptsetupSrcHash = "sha256-gAQmX9mTiF0I97Yz2+BWhR3hohAwdhOk693HQ/zO/lo=";
      pinnedCryptsetupFor = pkgs:
        pkgs.cryptsetup.overrideAttrs (_old: {
          version = pinnedCryptsetupVersion;
          src = pkgs.fetchurl {
            url =
              "mirror://kernel/linux/utils/cryptsetup/v${pkgs.lib.versions.majorMinor pinnedCryptsetupVersion}/"
              + "cryptsetup-${pinnedCryptsetupVersion}.tar.xz";
            hash = pinnedCryptsetupSrcHash;
          };
        });

      # Per Plan 72 §W2 — narrower than the dev-shell image.
      # See module-level docs above for the rationale on each.
      #
      # Plan 73 Followup B.2 adds the application-dependency
      # install pipeline (ADR-047): `uv` / `pnpm` drive the
      # installer, `cyclonedx-py` / `pnpm sbom` emit SBOMs, and
      # `pip-audit` / `pnpm audit` run the CVE scan. Each is
      # currently a soft gate — `mvm-builder-init::install`
      # emits a CycloneDX-1.5 empty stub when the SBOM / CVE
      # tool isn't on PATH and logs a warning. B.2.x will
      # hard-gate the SBOM + CVE side once the egress allowlist
      # lands.
      #
      # Plan 73 Followup B.2.x closed the egress side: the
      # builder VM now runs `mvm-egress-proxy` (built via
      # `mvmEgressProxyFor system`, installed at
      # `/sbin/mvm-egress-proxy` via `extraFiles` below).
      # `mvm-builder-init::install::run_install` spawns it
      # before the installer + injects `HTTPS_PROXY` /
      # `HTTP_PROXY` on `uv` / `pnpm`'s env. The proxy refuses
      # anything outside ADR-047's four hostnames:
      # `pypi.org`, `files.pythonhosted.org`,
      # `registry.npmjs.org`, `objects.githubusercontent.com`.
      # Complementary iptables drop-rule defense-in-depth is
      # B.2.y (not in this slice).
      builderPackages = pkgs: with pkgs; [
        bashInteractive
        coreutils
        # `pkgsStatic.busybox` for the lightweight utilities that
        # mvm-builder-init spawns by absolute path — chiefly
        # `/sbin/udhcpc` (busybox applet) for DHCP on the builder
        # VM's eth0. Without busybox in `packages`, mkGuest's
        # symlink loop (nix/lib/mk-guest.nix:770-788) skips it
        # and `/sbin/udhcpc` doesn't exist → setup_network bails.
        pkgsStatic.busybox
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
        # iptables — installed at boot by mvm-builder-init's
        # network::install_egress_lockdown (Plan 73 Followup
        # B.2.y / ADR-047). FATAL if absent.
        iptables
        e2fsprogs
        util-linux
        (pinnedCryptsetupFor pkgs) # provides pinned veritysetup
        # Plan 73 Followup B.2 — app-deps install pipeline.
        uv
        pnpm
        # NOTE (Plan 72 W5.D unblock): `python3Packages.cyclonedx-bom`
        # and `python3Packages.pip-audit` are referenced here but not
        # present in nixpkgs-25.11 under those exact attribute names;
        # the Stage 0 nix eval bails with "attribute 'cyclonedx-bom'
        # missing". Commented out until the right attribute name (or
        # a newer nixpkgs pin that has them) lands. Plan 73's
        # deps-volume audit pipeline still works at runtime via the
        # `mvm-egress-proxy` allowlist; the SBOM/CVE tools were a
        # nice-to-have inside the builder VM, not a load-bearing
        # blocker for `mvmctl dev up`.
        # python3Packages.cyclonedx-bom
        # python3Packages.pip-audit
      ];

      # Build `mvm-builder-init` (Plan 72 W3) for the target system.
      # `rustPlatform.buildRustPackage` consumes the workspace's
      # `Cargo.lock` so the dependency closure matches the rest of
      # the workspace — same `nix` crate version we cargo-check on
      # macOS, same nothing-else.
      #
      # `doCheck = false` because the unit tests under
      # `mvm-builder-init::linux::tests` already run in the
      # workspace's `cargo test` CI lane; running them again here
      # would double-pay the closure compute for no extra signal.
      mvmBuilderInitFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "mvm-builder-init";
          version = "0.14.0";
          src = workspace;
          cargoLock = {
            lockFile = workspace + "/Cargo.lock";
          };
          buildAndTestSubdir = "crates/mvm-builder-init";
          doCheck = false;
          meta = {
            description = "PID-1 for the libkrun builder VM (Plan 72 W3)";
            mainProgram = "mvm-builder-init";
          };
        };

      # Build `mvm-egress-proxy` (Plan 73 Followup B.2.x) for the
      # target system. Same `rustPlatform.buildRustPackage` shape
      # as `mvm-builder-init` — the workspace's `Cargo.lock`
      # drives the closure so we don't fork dep versions across
      # the two builder-VM binaries. Tests run in CI's
      # `cargo test --workspace` lane; `doCheck = false` here for
      # the same reason.
      mvmEgressProxyFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "mvm-egress-proxy";
          version = "0.14.0";
          src = workspace;
          cargoLock = {
            lockFile = workspace + "/Cargo.lock";
          };
          buildAndTestSubdir = "crates/mvm-egress-proxy";
          doCheck = false;
          meta = {
            description = "Builder-VM egress allowlist proxy (Plan 73 Followup B.2.x, ADR-047)";
            mainProgram = "mvm-egress-proxy";
          };
        };

      # Canonical kernel cmdline for the builder VM. `LibkrunBuilderVm`
      # (Plan 72 W4) reads this from the cmdline.txt output and
      # passes it to `mvm_libkrun::KrunContext.kernel_cmdline`.
      #
      # - `console=hvc0` — libkrun's virtio-console (no serial).
      # - `root=/dev/vda` — rootfs.ext4 attached as virtio-blk.
      # - `ro` — root is read-only; writes go to the persistent
      #   /nix-store virtio-blk at /dev/vdb (Plan 72 W4 wires it).
      # - `rootfstype=ext4` — skip filesystem auto-detection.
      # - `init=/sbin/mvm-builder-init` — Plan 72 W3 binary as PID 1.
      builderCmdline = "console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/mvm-builder-init";

      # Rootfs builder. The custom kernel under `./kernel` is built
      # `CONFIG_MODULES=n` (everything in-tree is `=y`), so the
      # rootfs ships no `/lib/modules/<kver>/` tree and mkGuest's
      # kernel arg goes unused — same rootfs whether we're producing
      # the full builder-VM image or the Stage 0 seed.
      # Plan 92 — `specs/plans/92-minimal-builder-vm-kernel.md`.
      mkBuilderVmRootfs =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          builderInit = mvmBuilderInitFor system;
          egressProxy = mvmEgressProxyFor system;
        in
        (libFor { inherit system; }).mkGuest {
          name = "mvm-builder-vm";
          # Skip the addon-dns bake. The builder VM's PID 1 is
          # `mvm-builder-init` (set via `extraFiles` + the
          # `init=/sbin/mvm-builder-init` kernel cmdline), so
          # mkGuest's initScript-side addon-dns activation block
          # never runs and the binary would just sit unused at
          # /usr/local/bin/mvm-addon-dns. The win is in Stage 0:
          # not building `mvm-addon-dns` removes a parallel rustc
          # run that competed with the kernel compile and pushed
          # the tmpfs-bound build into OOM territory.
          bakeAddonDns = false;
          # mkGuest requires an entrypoint declaration. At runtime
          # the kernel cmdline sets `init=/sbin/mvm-builder-init`,
          # so mkGuest's entrypoint is vestigial — but we still
          # need to declare one to satisfy the type contract.
          entrypoint.shell = "/bin/sh";
          packages = builderPackages pkgs;
          extraFiles = {
            "/sbin/mvm-builder-init" =
              "${builderInit}/bin/mvm-builder-init";
            # Plan 73 Followup B.2.x — egress allowlist proxy.
            # `mvm-builder-init::install::run_install` spawns
            # this from PATH (kernel default PATH includes
            # `/sbin`) before invoking `uv` / `pnpm`. The
            # binary embeds the ADR-047 four-hostname list at
            # compile time; no env-var override path on the
            # production build.
            "/sbin/mvm-egress-proxy" =
              "${egressProxy}/bin/mvm-egress-proxy";
          };
        };

      mkBuilderVmImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          builderInit = mvmBuilderInitFor system;
          egressProxy = mvmEgressProxyFor system;
          # Slim custom kernel — see `./kernel/default.nix`.
          # `pkgs.linuxManualConfig` over `make tinyconfig` + a
          # narrow `enables` list. `CONFIG_MODULES=n` so the kernel
          # has only what `mvm-builder-init` actually uses built-in
          # — no driver modules tree to ship.
          # Plan 92 — `specs/plans/92-minimal-builder-vm-kernel.md`.
          kernelPkg = import ./kernel { inherit pkgs; };
          rootfs = mkBuilderVmRootfs system;
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64 then "Image" else "bzImage";
        in
        pkgs.runCommand "mvm-builder-vm-image-${system}"
          {
            passthru = {
              inherit rootfs builderInit egressProxy;
              kernel = kernelPkg;
              cmdline = builderCmdline;
            };
          }
          ''
            mkdir -p $out

            # Kernel.
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

            # Rootfs.
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

            # Canonical kernel cmdline — Plan 72 W4's
            # `LibkrunBuilderVm` reads this and threads it into
            # `mvm_libkrun::KrunContext.kernel_cmdline`. Living next
            # to the kernel makes the binding atomic with the image.
            echo "${builderCmdline}" > $out/cmdline.txt

            # SHA-256 + size manifest, sister to the dev-image's
            # release-artifact pattern. `download_builder_vm_image`
            # (Plan 72 W5) verifies these against the release
            # manifest before extracting.
            kernel_sha=$(sha256sum $out/vmlinux | cut -d' ' -f1)
            rootfs_sha=$(sha256sum $out/rootfs.ext4 | cut -d' ' -f1)
            kernel_size=$(stat -c%s $out/vmlinux)
            rootfs_size=$(stat -c%s $out/rootfs.ext4)
            cat > $out/manifest.json <<MANIFEST
            {
              "name": "mvm-builder-vm",
              "system": "${system}",
              "vmlinux":      { "sha256": "$kernel_sha", "size": $kernel_size },
              "rootfs_ext4":  { "sha256": "$rootfs_sha", "size": $rootfs_size },
              "cmdline": "${builderCmdline}"
            }
            MANIFEST
          '';

      mkBuilderVmStage0Rootfs = system:
        let
          pkgs = import nixpkgs { inherit system; };
          # Stage 0 boots under a different kernel than what nixpkgs
          # ships, so omit the kernel + module tree to avoid
          # misleading modprobe with a foreign kver.
          rootfs = mkBuilderVmRootfs system;
        in
        pkgs.runCommand "mvm-builder-vm-stage0-rootfs-${system}" { } ''
          mkdir -p $out

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

          chmod 0644 $out/rootfs.ext4
          echo "${builderCmdline}" > $out/cmdline.txt

          rootfs_sha=$(sha256sum $out/rootfs.ext4 | cut -d' ' -f1)
          rootfs_size=$(stat -c%s $out/rootfs.ext4)
          cat > $out/manifest.json <<MANIFEST
          {
            "name": "mvm-builder-vm-stage0-rootfs",
            "system": "${system}",
            "rootfs_ext4": { "sha256": "$rootfs_sha", "size": $rootfs_size },
            "cmdline": "${builderCmdline}",
            "stage0_rootfs_only": true
          }
          MANIFEST
        '';
      # Plan 95 W2 — expose the generated kernel `.config` as a
      # standalone flake output so contributors can audit what
      # `make defconfig + enables/disables + olddefconfig` actually
      # produced without temporarily editing this flake. Build with:
      #
      #   nix build .#kernel-configfile -o /tmp/kconfig
      #   grep '=y$' /tmp/kconfig | sort > /tmp/kconfig.y.txt
      #
      # The file is a regular `.config` text file — diffable across
      # `disables` edits to confirm SoC platform clusters are gone.
      mkKernelConfigfile = system:
        let pkgs = import nixpkgs { inherit system; };
        in (import ./kernel { inherit pkgs; }).passthru.configfile;
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderVmImage system;
        stage0-rootfs = mkBuilderVmStage0Rootfs system;
        kernel-configfile = mkKernelConfigfile system;
      });
    };
}
