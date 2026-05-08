# mkGuest — busybox-as-PID-1 microVM image builder.
#
# Same flake the user writes is consumed in BOTH dev and production
# builds. The "sealed vs accessible" distinction is encoded by:
#
#   1. The entrypoint shape:
#        entrypoint.shell    = "/bin/sh"           # accessible (dev-style)
#        entrypoint.command  = [ "/usr/local/bin/web" ]   # sealed default
#        entrypoint.services = { … }               # multi-service supervised
#
#   2. The explicit `dev` flag (overrides the entrypoint heuristic):
#        dev = true   # always enables the console + reachable shell
#        dev = false  # never enables console regardless of entrypoint
#
# Inferred default: `dev = (entrypoint ? shell)`. mvmctl reads the
# `passthru.mvm.{accessible, sealed, entrypointKind}` metadata to
# gate `mvmctl console <vm>` host-side; the `/etc/mvm/variant` file
# baked into the rootfs is the in-guest cross-check.
#
# ── Why busybox-as-PID-1, not NixOS+systemd ──
#
# ADR-013 §"Boot-time budget" is the source of truth. The short
# version: NixOS+systemd boots in 1-3 s; Alpine+OpenRC in 300-500 ms;
# busybox-as-PID-1 with custom init approaches the upstream Firecracker
# reference of ~125 ms. The 200ms cold-boot target on Firecracker
# requires the busybox path. The previous iteration of mvm shipped
# this exact strategy.
#
# microvm.nix is still pinned as a flake input (per ADR-013) for its
# hypervisor abstractions and kernel-config helpers, but we DO NOT
# use its NixOS module — that's the systemd-heavy path we're
# explicitly avoiding here.

{ nixpkgs, microvm }:
{ system }:
let
  pkgs = import nixpkgs { inherit system; };
  lib  = nixpkgs.lib;

  # Static busybox — single binary, every shell utility as an applet.
  # `pkgsStatic` ensures no glibc dynamic-linker hop at /init time
  # (which alone saves ~10ms vs a glibc-linked init).
  busybox = pkgs.pkgsStatic.busybox;

  classifyEntrypoint = ep:
    let
      hasShell    = ep ? shell;
      hasCommand  = ep ? command;
      hasServices = ep ? services;
      forms       = lib.count (b: b) [ hasShell hasCommand hasServices ];
    in
    if forms == 0 then
      throw ''
        mkGuest: entrypoint must declare exactly one of:
          { shell    = "/bin/sh"; }
          { command  = [ "/usr/local/bin/x" ]; }
          { services = { web = { command = … }; … }; }
        Got: ${builtins.toJSON ep}
      ''
    else if forms > 1 then
      throw "mkGuest: entrypoint must declare exactly one form, not several"
    else if hasShell then "shell"
    else if hasCommand then "command"
    else "services";

  # Render a single command list as a quoted shell command line.
  renderCommand = argv:
    lib.concatStringsSep " " (map lib.escapeShellArg argv);
in
{ name
, entrypoint
, services       ? { }
, packages       ? [ ]
, hypervisor     ? "firecracker"
, vcpus          ? 1
, memory_mib     ? 256
, dev            ? null
, extraFiles     ? { }
}:
let
  entrypointKind = classifyEntrypoint entrypoint;
  isDev =
    if dev == null then entrypointKind == "shell"
    else dev;
  isSealed = !isDev;

  # Rendered entrypoint command-line that /init will exec. Only the
  # shell + command forms are wired in this wave; multi-service
  # supervision lands in W5.2 (needs a small Rust supervisor binary
  # in the rootfs). We surface the "services" form's metadata
  # correctly even though the runtime path errors at boot — that
  # way the host-side passthru is right and only the in-guest
  # supervisor is missing.
  entrypointCmd =
    if entrypointKind == "shell" then
      "exec ${lib.escapeShellArg entrypoint.shell} -i"
    else if entrypointKind == "command" then
      "exec ${renderCommand entrypoint.command}"
    else
      ''
        echo "mkGuest: entrypoint.services is not yet wired in this iteration"
        echo "  (W5.2 ports the multi-service supervisor binary)"
        echo "  Falling through to a recovery shell for triage."
        exec /bin/sh -i
      '';

  # /init — our PID 1. Pure POSIX shell; busybox provides every
  # utility used here. Boot-time-critical path so kept terse and
  # readable. No bashisms, no externalities beyond busybox applets.
  initScript = pkgs.writeScript "mvm-init" ''
    #!/bin/sh
    # mvm /init — busybox PID 1 (plan 60 / ADR-013).

    # Stage 1 — kernel pseudofs. Required before anything else
    # can read /proc/self or write to /dev/console.
    /bin/busybox mount -t proc     proc     /proc
    /bin/busybox mount -t sysfs    sysfs    /sys
    /bin/busybox mount -t devtmpfs devtmpfs /dev

    # Stage 2 — runtime tmpfs. /tmp + /run are RAM so the rootfs
    # stays read-only-leaning; volumes (Phase 2) attach to fixed
    # mountpoints instead.
    /bin/busybox mount -t tmpfs -o mode=1777,nosuid,nodev tmpfs /tmp
    /bin/busybox mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run

    # Stage 3 — hostname + console. /dev/console is what the
    # hypervisor wires our virtio-console to; in dev mode we keep
    # stdio attached to it so `mvmctl console` sees output.
    /bin/busybox hostname "$(/bin/busybox cat /etc/mvm/name 2>/dev/null || echo mvm)"

    # Stage 4 — exec the entrypoint. /etc/mvm/variant (dev|prod) +
    # /etc/mvm/entrypoint are baked at build time. dev variant gets
    # stdio re-attached to /dev/console so the user can interact;
    # prod variant lets the entrypoint inherit whatever the hypervisor
    # provided (typically the same console, but the variant marker
    # is the host-side gate).
    if [ -e /etc/mvm/variant ] && [ "$(/bin/busybox cat /etc/mvm/variant)" = "dev" ]; then
      exec </dev/console >/dev/console 2>&1
    fi

    # Source the entrypoint script. Rendered at build time so the
    # exec line below is final — no shell injection from runtime
    # config. /etc/mvm/entrypoint is mode 0500, owned by root.
    . /etc/mvm/entrypoint

    # If the entrypoint exits or doesn't exec, the kernel panics.
    # The fallthrough echo gives a chance to capture *why* via
    # console before that happens.
    echo "mvm: /etc/mvm/entrypoint returned without exec — kernel will panic"
    /bin/busybox sleep 5
  '';

  # Render the entrypoint as a shell-sourced fragment. /init does
  # `. /etc/mvm/entrypoint`, so this is just a script.
  entrypointFile = pkgs.writeText "mvm-entrypoint" ''
    #!/bin/sh
    # Auto-generated by mkGuest at build time. Do not edit.
    ${entrypointCmd}
  '';

  # Variant marker (dev|prod). In-guest source of truth — paired
  # with passthru.mvm.{accessible,sealed} on the derivation.
  variantFile = pkgs.writeText "mvm-variant" (
    if isDev then "dev\n" else "prod\n"
  );

  nameFile = pkgs.writeText "mvm-name" "${name}\n";

  # extraFiles — { "absolute/path" = { content; mode?; }; }
  extraFilePopulation = lib.concatMapStringsSep "\n"
    (path:
      let
        spec = extraFiles.${path};
        mode = spec.mode or "0644";
        target = "$out${path}";
      in
      ''
        mkdir -p "$(dirname ${lib.escapeShellArg target})"
        ${pkgs.coreutils}/bin/install -m ${mode} \
          ${pkgs.writeText "extra-${builtins.hashString "sha256" path}" spec.content} \
          ${lib.escapeShellArg target}
      ''
    )
    (lib.attrNames extraFiles);

  # ── Rootfs tree population ────────────────────────────────────
  #
  # We construct the rootfs as a real directory tree (not a NixOS
  # closure) so the boot path is a flat ext4. Every binary the
  # /init script touches resolves through /bin/* symlinks pointing
  # at /bin/busybox.
  #
  # Phase 6 layers the security overlay (per-service uids,
  # read-only /etc bind-mount, dm-verity) on top of this base.
  rootfsTree = pkgs.runCommand "mvm-rootfs-tree-${name}" { } ''
    set -e
    mkdir -p "$out"

    # Standard FHS dirs the kernel + init expect.
    mkdir -p "$out"/{bin,sbin,etc,proc,sys,dev,tmp,run,var,root,home,nix/store,etc/mvm}
    chmod 1777 "$out/tmp"
    chmod 0755 "$out/run"

    # busybox + applet symlinks. busybox --install -s would do this
    # at runtime; we pre-bake the links so the rootfs has no first-
    # boot setup step.
    cp ${busybox}/bin/busybox "$out/bin/busybox"
    chmod 0755 "$out/bin/busybox"
    for applet in $(${busybox}/bin/busybox --list); do
      ln -sf /bin/busybox "$out/bin/$applet"
    done
    # /sbin/init is what the kernel actually execs at boot (when
    # there's no init=/init kernel param). We point both at our
    # custom init script so either path works.
    cp ${initScript} "$out/init"
    chmod 0500 "$out/init"
    ln -sf /init "$out/sbin/init"

    # mvm metadata. /etc/mvm/entrypoint is the load-bearing file —
    # /init sources it. Mode 0500 so non-root processes in the guest
    # can't read or replace it (W2.2 makes /etc read-only as well).
    cp ${entrypointFile} "$out/etc/mvm/entrypoint"
    chmod 0500 "$out/etc/mvm/entrypoint"
    cp ${variantFile} "$out/etc/mvm/variant"
    chmod 0444 "$out/etc/mvm/variant"
    cp ${nameFile} "$out/etc/mvm/name"
    chmod 0444 "$out/etc/mvm/name"

    # Minimal /etc/passwd + /etc/group so getuid() lookups don't
    # silently fail. Only root (0) is provisioned at this stage —
    # per-service uids land in Phase 6 (W2.1).
    cat > "$out/etc/passwd" <<EOF
    root:x:0:0:root:/root:/bin/sh
    EOF
    chmod 0644 "$out/etc/passwd"
    cat > "$out/etc/group" <<EOF
    root:x:0:
    EOF
    chmod 0644 "$out/etc/group"

    # Extra user-supplied files.
    ${extraFilePopulation}

    # Closure of additional packages — copy each into /usr/local/bin
    # by symlink so they're on PATH alongside the busybox applets.
    mkdir -p "$out/usr/local/bin"
    ${lib.concatMapStringsSep "\n"
      (pkg: ''
        if [ -d "${pkg}/bin" ]; then
          for bin in "${pkg}"/bin/*; do
            [ -e "$bin" ] || continue
            ln -sf "$bin" "$out/usr/local/bin/$(basename "$bin")"
          done
        fi
      '')
      packages}
  '';

  # Package the tree as an ext4 image. nixpkgs ships a make-ext4-fs
  # derivation that handles the mkfs + populate dance correctly.
  # All arguments arrive in a single set via callPackage's auto-arg
  # injection.
  rootfsImage = pkgs.callPackage <nixpkgs/nixos/lib/make-ext4-fs.nix> {
    storePaths = [ rootfsTree ];
    volumeLabel = "mvm-${name}";
    populateImageCommands = ''
      cp -a --reflink=auto ${rootfsTree}/. ./files/
    '';
  };

  mvmMeta = {
    inherit name hypervisor;
    accessible = isDev;
    sealed = isSealed;
    entrypointKind = entrypointKind;
    initSystem = "busybox";
    expectedBootMs =
      if hypervisor == "firecracker" then 200
      else if hypervisor == "microsandbox" || hypervisor == "libkrun" then 500
      else 1000;
  };
in
rootfsImage.overrideAttrs (old: {
  passthru = (old.passthru or { }) // {
    mvm = mvmMeta;
    inherit rootfsTree;
    # Surface the chosen hypervisor + resource defaults at the top
    # of passthru so `nix eval` is sufficient for mvmctl to drive
    # the runtime — no NixOS evaluation needed.
    inherit hypervisor;
    resources = { inherit vcpus memory_mib; };
  };
})
