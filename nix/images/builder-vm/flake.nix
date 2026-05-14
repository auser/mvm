{
  description = "mvm builder VM image — kernel + rootfs.ext4 with Nix + tools + mvm-builder-init (Plan 72 W2)";

  # ── Why this flake exists ────────────────────────────────────────────
  #
  # Plan 72 (ADR-046) replaces the microsandbox-backed builder VM
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
  # - **No** `iptables` / `procps`-interactive / `less` per the
  #   plan's slimming directive.
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
      # TODO(B.2.x): pin a network egress allowlist
      # (`pypi.org`, `files.pythonhosted.org`, `registry.npmjs.org`,
      # `objects.githubusercontent.com`) per ADR-047 §"Lifecycle
      # gates" — today's builder VM has open egress.
      builderPackages = pkgs: with pkgs; [
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
        e2fsprogs
        util-linux
        # Plan 73 Followup B.2 — app-deps install pipeline.
        uv
        pnpm
        # `cyclonedx-py` ships under `python3Packages` in
        # nixpkgs (the upstream package name; `pkgs.cyclonedx-py`
        # is the entry point binary). Pulls a Python interpreter
        # closure but the same one `pip-audit` needs, so the
        # marginal cost is minor.
        python3Packages.cyclonedx-bom
        python3Packages.pip-audit
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

      mkBuilderVmImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          builderInit = mvmBuilderInitFor system;
          rootfs = (libFor { inherit system; }).mkGuest {
            name = "mvm-builder-vm";
            # mkGuest requires an entrypoint declaration. At runtime
            # the kernel cmdline sets `init=/sbin/mvm-builder-init`,
            # so mkGuest's entrypoint is vestigial — but we still
            # need to declare one to satisfy the type contract.
            entrypoint.shell = "/bin/sh";
            packages = builderPackages pkgs;
            extraFiles = {
              "/sbin/mvm-builder-init" =
                "${builderInit}/bin/mvm-builder-init";
            };
          };
          kernelPkg = pkgs.linuxPackages.kernel;
          kernelFile =
            if pkgs.stdenv.hostPlatform.isAarch64 then "Image" else "bzImage";
        in
        pkgs.runCommand "mvm-builder-vm-image-${system}"
          {
            passthru = {
              inherit rootfs builderInit;
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
    in
    {
      packages = forAllSystems (system: {
        default = mkBuilderVmImage system;
      });
    };
}
