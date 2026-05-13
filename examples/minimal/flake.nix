{
  description = "examples/minimal — smallest-thing-that-boots image for Plan 57 W3 libkrun smoke test.";

  # ── Why this flake is separate from nix/images/builder/ ──────────────
  #
  # The dev-image flake at nix/images/builder/ composes microvm.nix's
  # NixOS module + mvm's mkGuest helper, which bakes the Rust guest agent
  # (~480-crate cargo closure) into the rootfs. That closure exceeds the
  # microsandbox builder VM's 4 GiB overlay during nix-build evaluation,
  # so it can't be used as the substrate for the libkrun spike — the
  # build itself runs out of room before producing artifacts.
  #
  # This flake takes the opposite tack: zero Rust, zero microvm.nix,
  # just `pkgs.linuxPackages.kernel` for the kernel and a hand-rolled
  # busybox rootfs + a single static C binary (`vsock_ok.c`) for the
  # guest payload. The resulting closure is small enough to build
  # under the existing microsandbox path while we wait on Plan 72 W1
  # to replace it with the libkrun-driven builder VM.
  #
  # ── Contract with examples/libkrun-smoke.rs ──────────────────────────
  #
  # `nix build .#default` produces `$out` with these two files:
  #
  #   vmlinux       — Linux kernel image (Image format on aarch64-linux,
  #                   bzImage format on x86_64-linux). KRUN_KERNEL_FORMAT_RAW
  #                   accepts both.
  #   rootfs.ext4   — ext4 root filesystem image with /init at the root.
  #                   /init mounts /proc /sys /dev, runs /bin/vsock_ok
  #                   (which connects vsock CID_HOST:1234 and writes
  #                   "ok\n"), then powers off via sysrq.
  #
  # The smoke test (host side) constructs a KrunContext pointing at
  # those two artifacts, calls `krun_add_vsock_port(1234, <socketpath>)`,
  # listens on the Unix socket, and asserts it reads "ok\n" before the
  # guest exits. Both ends agree on port 1234 by literal constant.
  #
  # ── Out of scope ────────────────────────────────────────────────────
  #
  # No verified boot (claim 3) — Plan 57 W3.4 explicitly exempts the
  # spike; Plan 25 §W6.4 wires dm-verity through libkrun later.
  # No guest agent — the smoke test exercises libkrun's boot/vsock/
  # exit path, not the mvm-guest protocol.
  # No network — the smoke test doesn't touch networking; libkrun's
  # default TSI vsock is the only host↔guest channel.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # Build the static guest payload: a single C source file
      # producing a self-contained musl-static ELF. `pkgsStatic` is
      # the nixpkgs convention for "everything in this package set
      # links statically" — perfect for a /init payload that has no
      # runtime linker available.
      mkVsockOk = pkgs:
        pkgs.pkgsStatic.stdenv.mkDerivation {
          pname = "vsock-ok";
          version = "0.1.0";
          src = ./.;
          dontConfigure = true;
          buildPhase = ''
            $CC -static -O2 -o vsock_ok vsock_ok.c
          '';
          installPhase = ''
            install -Dm0755 vsock_ok $out/bin/vsock_ok
          '';
        };

      # Pick the kernel image filename the libkrun caller expects.
      # libkrun accepts KRUN_KERNEL_FORMAT_RAW for both arm64 `Image`
      # and x86_64 `bzImage`; we land both at `$out/vmlinux` so the
      # smoke test doesn't need arch-conditional paths.
      kernelFileFor = pkgs:
        if pkgs.stdenv.hostPlatform.isAarch64 then "Image" else "bzImage";

      mkImage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          vsockOk = mkVsockOk pkgs;
          # Static busybox: a single binary with every shell utility
          # as an applet, no glibc dynamic linker required.
          busybox = pkgs.pkgsStatic.busybox;
          kernel = pkgs.linuxPackages.kernel;
          kernelFile = kernelFileFor pkgs;
        in
        pkgs.runCommand "mvm-libkrun-smoke-${system}"
          {
            nativeBuildInputs = [ pkgs.e2fsprogs pkgs.cpio ];
            passthru = { inherit kernel vsockOk busybox; };
          }
          ''
            set -euo pipefail

            mkdir -p $out

            # ── Kernel ──
            if [ -f ${kernel}/${kernelFile} ]; then
              cp ${kernel}/${kernelFile} $out/vmlinux
            else
              echo "kernel package ${kernel} did not produce ${kernelFile}" >&2
              ls -la ${kernel} >&2
              exit 1
            fi
            chmod 0644 $out/vmlinux

            # ── Rootfs staging ──
            rootdir=$(mktemp -d)
            mkdir -p "$rootdir"/{bin,dev,proc,sys,etc,root}

            # busybox: single static binary, applets via /bin/busybox $name.
            install -Dm0755 ${busybox}/bin/busybox "$rootdir/bin/busybox"

            # Common applet names linked back to /bin/busybox so /init's
            # `mount`, `sync`, `cat`, `poweroff`, `sleep` resolve without
            # the explicit `/bin/busybox <applet>` form.
            for applet in sh mount umount sync cat echo poweroff reboot sleep ls cp; do
              ln -s busybox "$rootdir/bin/$applet"
            done

            # Static guest payload — written by the C derivation above.
            install -Dm0755 ${vsockOk}/bin/vsock_ok "$rootdir/bin/vsock_ok"

            # /init is the kernel's entrypoint. Must be at the root of the
            # filesystem and executable. We don't use a separate initramfs
            # — the kernel mounts rootfs.ext4 as `/` directly and execve's
            # /init from there.
            install -Dm0755 ${./init.sh} "$rootdir/init"

            # ── Build rootfs.ext4 ──
            # mke2fs -d <dir> stages the directory at filesystem creation
            # time; -t ext4 selects the right superblock; the size
            # (32 MiB) is bigger than the actual content for slack but
            # tiny by VM-image standards.
            #
            # mke2fs writes timestamps from the host clock by default,
            # which breaks reproducibility under Nix. SOURCE_DATE_EPOCH
            # is honored when set by Nix's stdenv; the explicit env-line
            # is harmless if already set.
            : ''${SOURCE_DATE_EPOCH:=1}
            export SOURCE_DATE_EPOCH

            mke2fs \
              -t ext4 \
              -d "$rootdir" \
              -L mvm-smoke \
              -U random \
              -E no_copy_xattrs \
              -b 4096 \
              $out/rootfs.ext4 \
              32M
            chmod 0644 $out/rootfs.ext4
          '';
    in
    {
      packages = forAllSystems (system: {
        default = mkImage system;
      });
    };
}
