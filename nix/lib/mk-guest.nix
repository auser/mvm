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

{ nixpkgs, microvm, mvmSrc }:
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
, uids           ? null   # { agent = <int>; entrypoint = <int>; } — see below
, extraFiles     ? { }
# Optional kernel package. When set, mkGuest copies its module
# tree (`/lib/modules/<kver>/`) into the rootfs and `/init` runs
# `modprobe vmw_vsock_virtio_transport` before forking the agent.
# Required when the kernel ships AF_VSOCK as a module (the default
# nixpkgs `linuxPackages.kernel` config). Without it,
# `mvm-guest-agent`'s `socket(AF_VSOCK, …)` returns EAFNOSUPPORT and
# every host-side surface (`mvmctl console`, `dev shell`, `build`)
# goes dark on a guest booted from that kernel.
, kernel         ? null
}:
let
  entrypointKind = classifyEntrypoint entrypoint;
  isDev =
    if dev == null then entrypointKind == "shell"
    else dev;
  isSealed = !isDev;

  # ── Guest agent build (W6.1.2) ─────────────────────────────────
  #
  # Real Rust binary built from the workspace at `mvmSrc` via
  # `nix/packages/mvm-guest-agent.nix`. Replaces the W6.1.1 sh-stub
  # that previously lived inline here. The W7.x.2 libkrun
  # builder VM is what makes this buildable on hosts without native
  # Linux Nix.
  #
  # The `dev-shell` Cargo feature gates the `do_exec` RPC handler
  # (ADR-002 §W4.3 / `prod-agent-no-exec` CI gate). We tie it to
  # `isDev` here so the same `mkGuest` call:
  #
  #   - Dev image (`entrypoint.shell = ...`, or `dev = true`)
  #     → `do_exec` compiled in → `mvmctl exec`/`mvmctl console` work
  #   - Prod/sealed image (`entrypoint.command`/`services`, or
  #     `dev = false`)
  #     → `do_exec` stripped → CI's symbol-absence gate passes
  #
  # Either way the binary is the production Rust build, not a stub.
  guestAgentPkg = pkgs.callPackage ../packages/mvm-guest-agent.nix {
    inherit mvmSrc;
    withDevShell = isDev;
  };

  # ── Privilege model (uids) ─────────────────────────────────────
  #
  # PID 1 must be uid 0 (kernel requirement); everything we can
  # drop is dropped via `setpriv` before exec. Two configurable
  # uids:
  #
  #   agent       — the host-mediated tool agent (vsock RPC handler).
  #                 Always non-root; never needs privilege.
  #
  #   entrypoint  — the workload the user declared. Defaults differ
  #                 by mode:
  #                   dev = true  → uid 0 (root shell;
  #                                  apt install / mount work)
  #                   dev = false → uid 1000 (rootless workload;
  #                                  defense in depth — ADR-002 W2.1)
  #
  # Override either via `uids = { agent = N; entrypoint = M; }` —
  # e.g. `entrypoint = 1000` forces a rootless dev shell, or
  # `entrypoint = 0` forces a rootful prod workload (rare; usually
  # a misconfiguration).
  defaultEntrypointUid = if isDev then 0 else 1000;
  resolvedUids = {
    agent = if uids != null && uids ? agent then uids.agent else 990;
    entrypoint =
      if uids != null && uids ? entrypoint
      then uids.entrypoint
      else defaultEntrypointUid;
  };

  # GID == UID by convention. /etc/group entries below mirror this.
  # Phase 6 (W2.1) introduces per-service derived gids; for the
  # Phase 1 W6.1 slice we keep it simple.
  agentUid = resolvedUids.agent;
  entrypointUid = resolvedUids.entrypoint;

  # Wrap a command-line in `setpriv` when the target uid is non-zero.
  # busybox provides setpriv from util-linux applets; the flags here
  # match ADR-002 W2.3 (--reuid + --regid + --no-new-privs) so the
  # privilege drop is consistent with the planned Phase 6 hardening.
  # uid==0 short-circuits to the bare command — no point setpriv-ing
  # to root.
  setprivWrap = uid: cmd:
    if uid == 0 then cmd
    else
      "exec /bin/busybox setpriv --reuid=${toString uid} --regid=${toString uid} "
      + "--clear-groups --no-new-privs -- ${cmd}";

  rawEntrypointCmd =
    if entrypointKind == "shell" then
      "${lib.escapeShellArg entrypoint.shell} -i"
    else if entrypointKind == "command" then
      renderCommand entrypoint.command
    else
      "/bin/sh -i";  # services fallthrough; W5.2 wires the supervisor

  # The full /etc/mvm/entrypoint body. For shell + command forms,
  # setpriv-wrap as appropriate. For services (still stubbed),
  # bail out with a clear note + recovery shell.
  entrypointCmd =
    if entrypointKind == "services" then
      ''
        echo "mkGuest: entrypoint.services is not yet wired in this iteration"
        echo "  (W5.2 ports the multi-service supervisor binary)"
        echo "  Falling through to a recovery shell for triage."
        ${setprivWrap entrypointUid "/bin/sh -i"}
      ''
    else
      "${setprivWrap entrypointUid rawEntrypointCmd}";

  # /init — our PID 1. Pure POSIX shell; busybox provides every
  # utility used here. Boot-time-critical path so kept terse and
  # readable. No bashisms, no externalities beyond busybox applets.
  #
  # Supervision pattern (W6.1):
  #   1. Stage filesystem (proc/sys/dev + tmpfs).
  #   2. Fork the guest agent in background under setpriv→agent uid.
  #   3. Re-attach stdio (dev variant).
  #   4. setpriv→entrypoint uid + exec the workload.
  #
  # PID 1 stays uid 0 (kernel mandate); both children run rootless
  # by default in production (see uids resolution above).
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

    # Stage 2.25 — vsock kernel modules. Stock nixpkgs kernel ships
    # AF_VSOCK as `=m`; without modprobe the agent's
    # `socket(AF_VSOCK, …)` returns EAFNOSUPPORT. modprobe-ing
    # `vmw_vsock_virtio_transport` pulls in `vsock` +
    # `vmw_vsock_virtio_transport_common` via modules.dep. Silently
    # skipped when `/lib/modules` is absent — e.g. on a future kernel
    # that ships VSOCK=y, or when `mkGuest` was called without the
    # `kernel` argument.
    if [ -d /lib/modules ]; then
      /bin/busybox modprobe vmw_vsock_virtio_transport 2>/dev/null || true
    fi

    # Stage 2.5 — guest agent supervisor. Fork the agent into
    # the background under its own uid before we drop to the
    # entrypoint. The agent is responsible for vsock RPC (host
    # tool calls, lifecycle hooks); without it, the host can boot
    # us but can't talk to us. We never block on it — if the agent
    # fails to start, the entrypoint still runs and the lack of
    # agent shows up in `mvmctl status`.
    #
    # Plan 74 W1.4b (ADR-051) — when the mvm runtime overlay is
    # attached, `mvm-verity-init` bind-mounts it at /mvm/runtime
    # before switch_root, so /mvm/runtime/agent is the canonical
    # binary location. Prefer it over the baked-in copy at
    # /usr/local/bin/mvm-guest-agent (which a future PR drops
    # entirely once every backend attaches the overlay). Both
    # paths are exec-tested so a half-attached overlay (directory
    # present, agent missing) still falls through to the baked-in
    # path rather than booting agent-less.
    MVM_AGENT_BIN=
    if [ -x /mvm/runtime/agent ]; then
      MVM_AGENT_BIN=/mvm/runtime/agent
    elif [ -x /usr/local/bin/mvm-guest-agent ]; then
      MVM_AGENT_BIN=/usr/local/bin/mvm-guest-agent
    fi
    if [ -n "$MVM_AGENT_BIN" ]; then
      /bin/busybox setsid /bin/busybox setpriv \
        --reuid=${toString agentUid} --regid=${toString agentUid} \
        --clear-groups --no-new-privs \
        -- "$MVM_AGENT_BIN" &
    fi

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

    # Stage 4.5 — Plan 74 W1.4b (ADR-051) — mvm runtime overlay env.
    # When the overlay is mounted (verity boot path), surface its
    # presence + SDK-library paths to the entrypoint via env
    # variables. Per-language path vars (PYTHONPATH, NODE_PATH)
    # are prepended so they take precedence over a user's existing
    # value; an empty existing value leaves no trailing colon.
    # Setting these unconditionally on the overlay-mounted path
    # gives a stable contract for SDK addons (ADR-049 vsock hooks)
    # without per-image opt-in. The dev/legacy path (no overlay)
    # leaves the env untouched so existing flakes keep their
    # current behaviour.
    if [ -d /mvm/runtime ] && [ -e /mvm/runtime/VERSION ]; then
      export MVM_RUNTIME_OVERLAY=1
      if [ -d /mvm/runtime/sdk-py ]; then
        export PYTHONPATH="/mvm/runtime/sdk-py''${PYTHONPATH:+:''${PYTHONPATH}}"
      fi
      if [ -d /mvm/runtime/sdk-ts ]; then
        export NODE_PATH="/mvm/runtime/sdk-ts''${NODE_PATH:+:''${NODE_PATH}}"
      fi
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

  # ── mvm-guest-agent — production Rust binary (W6.1.2)
  #
  # Built by `nix/packages/mvm-guest-agent.nix` from the workspace
  # source at `mvmSrc`. The W6.1.1 sh-stub that previously lived here
  # was a placeholder used while the cross-compile infrastructure
  # was being staged; the W7.x.2 libkrun builder VM made the
  # real build host-Nix-free, which unblocked this swap.
  #
  # The binary is the same one the workspace's
  # `crates/mvm-guest/src/bin/mvm-guest-agent.rs` Cargo target builds
  # — vsock RPC handler, worker-pool dispatcher, integration manifest
  # consumer, system metrics surface. ADR-002 §W4 documents the
  # attack surface; ADR-002 §W4.3 documents the `dev-shell` feature
  # gate that toggles `do_exec` between dev and prod images.
  agentBinary = "${guestAgentPkg}/bin/mvm-guest-agent";

  # `mvm-seccomp-apply` ships alongside the agent (same Cargo
  # workspace member, same derivation). The per-service launch line
  # in `mkServiceBlock` execs it via setpriv to apply the tier's
  # seccomp filter before handing control to the workload.
  seccompApplyBinary = "${guestAgentPkg}/bin/mvm-seccomp-apply";

  # `mvm-verity-init` is the PID 1 of the verity initramfs (ADR-002
  # §W3). Baked into the verity-initrd cpio.gz, not into the rootfs
  # directly — wired here as a passthru export so the initramfs
  # builder can reach it without duplicating the agent derivation.
  verityInitBinary = "${guestAgentPkg}/bin/mvm-verity-init";

  # extraFiles — three accepted spec shapes per target path:
  #
  #   { "absolute/path" = { content = "..."; mode? = "0644"; }; }
  #     → write text content via `pkgs.writeText`. Default mode 0644.
  #
  #   { "absolute/path" = { source = "/nix/store/.../bin/foo"; mode? = "0755"; }; }
  #     → copy an existing file (typically a built binary) from the
  #       given store path. Default mode 0755 (executables dominate).
  #
  #   { "absolute/path" = "/nix/store/.../bin/foo"; }
  #     → shorthand for `{ source = <that string>; }`.
  #
  # Binary-source variants exist so Plan 72's builder-vm flake can
  # install `mvm-builder-init` at `/sbin/mvm-builder-init` without
  # inlining its bytes as a string (`writeText` is text-only).
  extraFilePopulation = lib.concatMapStringsSep "\n"
    (path:
      let
        rawSpec = extraFiles.${path};
        spec =
          if builtins.isString rawSpec then { source = rawSpec; }
          else rawSpec;
        hasContent = spec ? content;
        hasSource = spec ? source;
        mode =
          if spec ? mode then spec.mode
          else if hasSource then "0755"
          else "0644";
        src =
          if hasContent then
            pkgs.writeText "extra-${builtins.hashString "sha256" path}" spec.content
          else if hasSource then
            spec.source
          else
            throw "mkGuest: extraFiles[${path}] must set either `content` (text) or `source` (file path)";
      in
      # Path arrives from Nix-interpolated keys (no shell escaping
      # needed); inline via `"$out${path}"` rather than via
      # `lib.escapeShellArg` so the shell expands `$out` instead of
      # treating it as a literal in single quotes.
      ''
        mkdir -p "$out$(dirname ${lib.escapeShellArg path})"
        ${pkgs.coreutils}/bin/install -m ${mode} \
          ${src} \
          "$out${path}"
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

    # Standard FHS dirs the kernel + init expect. `/nix-store`,
    # `/job`, `/out`, `/work` are mount points the libkrun builder
    # VM (Plan 72 W3) needs pre-created — rootfs boots `ro` so
    # `mvm-builder-init` can't `mkdir` them at runtime.
    mkdir -p "$out"/{bin,sbin,etc,proc,sys,dev,tmp,run,var,root,home,nix/store,nix-store,etc/mvm,job,out,work}
    chmod 1777 "$out/tmp"
    chmod 0755 "$out/run"

    # Plan 74 W1.4b — the mvm runtime overlay (ADR-051) is
    # bind-mounted at /mvm/runtime by `mvm-verity-init` before
    # switch_root. The directory must exist in the rootfs so the
    # bind-mount has a target. Mode 0755 (owner root); the overlay
    # itself is mounted read-only over it, so contents can't be
    # written by the guest regardless. Outside the verity-boot
    # path (dev-mode VMs that don't run `mvm-verity-init`) the
    # directory is empty — /init below falls back to the baked-in
    # agent. `/mvm/` is reserved (admission-time check in W1.4b.3d
    # rejects OCI images that carry content under this path).
    mkdir -p "$out/mvm/runtime"
    chmod 0755 "$out/mvm"
    chmod 0755 "$out/mvm/runtime"

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

    # /etc/passwd + /etc/group provision root (mandatory for PID 1)
    # plus the agent + entrypoint uids resolved at build time.
    # Per ADR-002 W2.2 (Phase 6) these become read-only via bind-
    # mount once the security overlay lands; for W6.1 they're
    # plain mode 0644.
    #
    # When entrypoint uid happens to be 0 (dev-mode default), the
    # entry collapses to the root row — guarded against the
    # duplicate by skipping the second cat. Same for the agent
    # uid in the unlikely override case.
    cat > "$out/etc/passwd" <<EOF
    root:x:0:0:root:/root:/bin/sh
    EOF
    if [ "${toString agentUid}" != "0" ]; then
      printf 'mvm-agent:x:${toString agentUid}:${toString agentUid}:mvm guest agent:/var/empty:/bin/false\n' >> "$out/etc/passwd"
    fi
    if [ "${toString entrypointUid}" != "0" ] && [ "${toString entrypointUid}" != "${toString agentUid}" ]; then
      printf 'mvm-worker:x:${toString entrypointUid}:${toString entrypointUid}:mvm workload:/home/mvm-worker:/bin/sh\n' >> "$out/etc/passwd"
      mkdir -p "$out/home/mvm-worker"
      chmod 0755 "$out/home/mvm-worker"
    fi
    chmod 0644 "$out/etc/passwd"

    cat > "$out/etc/group" <<EOF
    root:x:0:
    EOF
    if [ "${toString agentUid}" != "0" ]; then
      printf 'mvm-agent:x:${toString agentUid}:\n' >> "$out/etc/group"
    fi
    if [ "${toString entrypointUid}" != "0" ] && [ "${toString entrypointUid}" != "${toString agentUid}" ]; then
      printf 'mvm-worker:x:${toString entrypointUid}:\n' >> "$out/etc/group"
    fi
    chmod 0644 "$out/etc/group"

    # Default /etc/resolv.conf and CA cert bundle — needed for any
    # guest that talks to the network over TLS (most Nix flake
    # fetches reach cache.nixos.org / api.github.com). Cloudflare +
    # Google as the canonical no-infra-of-my-own DNS defaults; the
    # cert bundle is the standard Mozilla one from `pkgs.cacert`.
    cat > "$out/etc/resolv.conf" <<EOF
    nameserver 1.1.1.1
    nameserver 8.8.8.8
    EOF
    chmod 0644 "$out/etc/resolv.conf"

    mkdir -p "$out/etc/ssl/certs"
    cp ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt "$out/etc/ssl/certs/ca-bundle.crt"
    ln -sf /etc/ssl/certs/ca-bundle.crt "$out/etc/ssl/certs/ca-certificates.crt"
    chmod 0644 "$out/etc/ssl/certs/ca-bundle.crt"

    # mvm-guest-agent — installed under /usr/local/bin so /init can
    # exec it. Mode 0555 so the agent can't rewrite itself; ownership
    # is the build-time user (Nix sandbox has no root) — Phase 6 W2.2
    # binds /etc + /usr read-only at boot to make this load-bearing.
    #
    # mkdir before the cp: when `packages = []` the directory-creation
    # block below is a no-op, so /usr/local/bin doesn't exist yet and
    # the cp fails with "No such file or directory". That was the
    # latent bug that broke release.yml's dev-image lane and the
    # ch-linux bootcheck before this fix.
    mkdir -p "$out/usr/local/bin"
    cp ${agentBinary} "$out/usr/local/bin/mvm-guest-agent"
    chmod 0555 "$out/usr/local/bin/mvm-guest-agent"

    # Kernel modules. `/init` `modprobe`s vsock before forking the
    # agent (default nixpkgs kernel ships AF_VSOCK as `=m`); without
    # `/lib/modules/<kver>/` in the rootfs, modprobe has nothing to
    # load and the agent fails to open AF_VSOCK.
    #
    # nixpkgs splits the aarch64-linux kernel into two derivations:
    # `kernel` ships `Image` + `System.map` + `dtbs/` (no modules),
    # while `kernel.modules` owns the `lib/modules/<kver>/` tree
    # (built with `INSTALL_MOD_PATH=$out`). Probe `kernel.modules`
    # first (modern nixpkgs), fall back to `kernel`'s own `$out` for
    # single-output kernel packages microvm.nix wraps.
    ${lib.optionalString (kernel != null) (
      let
        candidates =
          (if kernel ? modules then [ kernel.modules ] else [ ])
          ++ [ kernel ];
        candidateRefs = lib.concatMapStringsSep " " (c: ''"${c}"'') candidates;
      in ''
        for cand in ${candidateRefs}; do
          if [ -d "$cand/lib/modules" ]; then
            shopt -s nullglob
            kmod_dirs=("$cand"/lib/modules/*)
            shopt -u nullglob
            for src in "''${kmod_dirs[@]}"; do
              kver=$(${pkgs.coreutils}/bin/basename "$src")
              mkdir -p "$out/lib/modules/$kver"
              cp -a --reflink=auto "$src/." "$out/lib/modules/$kver/"
              # Drop build-machine `source`/`build` symlinks that
              # point into host nix-store dev outputs the guest
              # can't resolve. modprobe warns on every invocation
              # otherwise; the guest never reads kernel headers.
              rm -f "$out/lib/modules/$kver/source" \
                    "$out/lib/modules/$kver/build"
            done
            break
          fi
        done
      ''
    )}

    # Extra user-supplied files.
    ${extraFilePopulation}

    # Closure of additional packages — symlink each binary into
    # `/usr/local/bin` AND `/sbin` so the standard system-binary
    # paths (`/sbin/mkfs.ext4`, `/sbin/udhcpc`, etc.) resolve.
    # `mvm-builder-init` uses those paths verbatim and would
    # ENOENT-fail without them (e.g. e2fsprogs ships mkfs.ext4 in
    # the package's sbin subdir, not bin).
    mkdir -p "$out/usr/local/bin"
    ${lib.concatMapStringsSep "\n"
      (pkg: ''
        for srcdir in bin sbin; do
          if [ -d "${pkg}/$srcdir" ]; then
            for binpath in "${pkg}/$srcdir"/*; do
              [ -e "$binpath" ] || continue
              name=$(basename "$binpath")
              ln -sf "$binpath" "$out/usr/local/bin/$name"
              ln -sf "$binpath" "$out/sbin/$name"
            done
          fi
        done
      '')
      packages}
  '';

  # Package the tree as an ext4 image. nixpkgs ships a make-ext4-fs
  # derivation that handles the mkfs + populate dance correctly.
  # All arguments arrive in a single set via callPackage's auto-arg
  # injection. Reference make-ext4-fs.nix via the flake input
  # (`${nixpkgs}/...`) rather than the angle-bracket form (`<nixpkgs/...>`)
  # — the latter trips flake pure evaluation ("cannot look up
  # '<nixpkgs/...>' in pure evaluation mode").
  rootfsImage = pkgs.callPackage "${nixpkgs}/nixos/lib/make-ext4-fs.nix" {
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
    # ADR-013 §"Per-backend boot budgets" — single 300ms floor across
    # every backend. Custom /init + trimmed kernel + direct vmlinux
    # boot are the levers that keep us under it. A backend that can't
    # hit the floor is a backend we drop.
    expectedBootMs = 300;
    # Privilege model — the resolved uids `setpriv` drops to before
    # exec. PID 1 is uid 0 (kernel requirement); these are the
    # workload + agent uids. Surfaces here so mvmctl status can
    # verify the actual /proc/<pid>/Uid against the declared
    # intent.
    uids = {
      agent = agentUid;
      entrypoint = entrypointUid;
    };
    rootlessEntrypoint = entrypointUid != 0;
    # Agent binary kind: "real" since W6.1.2 swapped in the cross-
    # compiled Rust binary. The previous "stub" value flagged the
    # W6.1.1 placeholder sh script. `mvmctl status` reads this;
    # production deployments should refuse to boot a "stub" image.
    agentBinary = "real";
    # Plan 74 W1.4b (ADR-051) — the rootfs carries a `/mvm/runtime`
    # bind-mount target and the /init script prefers the overlay
    # agent at `/mvm/runtime/agent` over the baked-in
    # `/usr/local/bin/mvm-guest-agent`. Admission-time gates can
    # refuse to boot a workload whose rootfs is not overlay-aware
    # (e.g. an old cached template predating W1.4b.3c).
    overlayAware = true;
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
    # W6.1.2: expose the side-binaries from the guest-agent build so
    # downstream derivations (verity-initrd, per-service launch line
    # in `mkServiceBlock`) can reach `mvm-seccomp-apply` and
    # `mvm-verity-init` without re-running the cargo build.
    inherit guestAgentPkg seccompApplyBinary verityInitBinary;
  };
})
