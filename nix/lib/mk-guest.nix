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
# ── kernel + matching modules ────────────────────────────────────────
#
# When set, mkGuest installs the kernel's `/lib/modules/<modDirVersion>/`
# tree into the rootfs and adds `pkgs.kmod` to the packages closure so
# /init can `modprobe` modules at boot. The motivating use case is
# vsock: the rootfs is busybox-flat, so `vsock=m` modules can't load
# unless we ship the matching modules tree. Earlier iterations
# `.override`-d the kernel to compile vsock built-in — that busts
# `cache.nixos.org` (custom .config = unique derivation hash) and
# added ~22 min to every cold dev-image build. Shipping modules
# instead keeps the stock binary-cached kernel.
#
# **Match exactly.** The modules tree is version-pinned to its
# kernel; mixing kernel + modules from different builds is undefined
# behaviour. Callers pass the kernel derivation; mkGuest pulls
# `kernel.modDirVersion` to name the install directory.
#
# Rootfs growth: ~50 MB on aarch64 nixpkgs (full modules tree). An
# opportunistic prune to just the vsock + virtio module subtree is
# a follow-up — at that point modules.dep needs re-running through
# `depmod -b $out`, which adds complexity disproportionate to the
# size win for a dev image.
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
  # that previously lived inline here. The W7.x.2 microsandbox
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

  # Kernel-modules support: when a kernel is wired in, pull `pkgs.kmod`
  # so /usr/local/bin/modprobe (and its depmod-built index) is available
  # in the guest. busybox's modprobe applet works for trivial cases but
  # drops corner cases (compressed-module variants, alias lookups) that
  # `kmod`'s modprobe handles properly. Without modprobe the modules
  # tree we ship is just dead weight on disk.
  hasKernel = kernel != null;
  effectivePackages = packages ++ pkgs.lib.optional hasKernel pkgs.kmod;

  # Wrap a command-line in `setpriv` when the target uid is non-zero.
  # Uses util-linux's `setpriv` (symlinked into /usr/local/bin via the
  # packages closure), not busybox's stripped applet — busybox doesn't
  # recognise the `--reuid` / `--regid` long options and silently fails
  # the launch with "setpriv: unrecognized option: reuid=…", which
  # then breaks the workload AND any agent we wrap in setpriv. The
  # flags here match ADR-002 W2.3 (--reuid + --regid + --no-new-privs)
  # so the privilege drop is consistent with the planned Phase 6
  # hardening. uid==0 short-circuits to the bare command — no point
  # setpriv-ing to root.
  setprivWrap = uid: cmd:
    if uid == 0 then cmd
    else
      "exec /usr/local/bin/setpriv --reuid=${toString uid} --regid=${toString uid} "
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
    # can read /proc/self or write to /dev/console. `mountpoint -q`
    # short-circuits when the kernel already mounted devtmpfs (the
    # default with CONFIG_DEVTMPFS_MOUNT) — otherwise the explicit
    # mount fails "Resource busy" and clutters the console log.
    /bin/busybox mount -t proc  proc  /proc
    /bin/busybox mount -t sysfs sysfs /sys
    /bin/busybox mountpoint -q /dev || /bin/busybox mount -t devtmpfs devtmpfs /dev

    # Stage 2 — runtime tmpfs. /tmp + /run are RAM so the rootfs
    # stays read-only-leaning; volumes (Phase 2) attach to fixed
    # mountpoints instead.
    /bin/busybox mount -t tmpfs -o mode=1777,nosuid,nodev tmpfs /tmp
    /bin/busybox mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run

    # Stage 2.4 — kernel modules (when a kernel was wired into the
    # rootfs at build time). Most importantly the vsock chain: the
    # agent in Stage 2.5 opens AF_VSOCK, and if the vsock module
    # family isn't loaded that `socket()` returns EAFNOSUPPORT,
    # taking the whole host↔guest agent surface (`mvmctl console`,
    # exec, lifecycle) dark.
    #
    # Defensive on three axes:
    #   - `[ -x modprobe ]`: modules-less images (kernel = null at
    #     mkGuest time) skip the loop silently.
    #   - `modprobe -q`: silent on already-loaded / built-in modules.
    #   - `|| true`: module names drift across kernel versions
    #     (vmw_vsock_virtio_transport vs. vsock_virtio_transport).
    #     Try the canonical names; the first that loads wins. As
    #     long as `vsock` itself loads, AF_VSOCK is registered.
    if [ -x /usr/local/bin/modprobe ] && [ -d /lib/modules ]; then
      for mod in vsock vmw_vsock_virtio_transport vsock_loopback; do
        /usr/local/bin/modprobe -q "$mod" 2>/dev/null || true
      done
    fi

    # Stage 2.5 — guest agent supervisor. Fork the agent into
    # the background under its own uid before we drop to the
    # entrypoint. The agent is responsible for vsock RPC (host
    # tool calls, lifecycle hooks); without it, the host can boot
    # us but can't talk to us. We never block on it — if the agent
    # fails to start, the entrypoint still runs and the lack of
    # agent shows up in `mvmctl status`.
    # Same util-linux-vs-busybox setpriv distinction as in
    # `setprivWrap` above — busybox's applet doesn't grok `--reuid`,
    # so use `/usr/local/bin/setpriv` (util-linux, symlinked from the
    # packages closure). Without this the agent never starts and the
    # whole vsock surface (`mvmctl console`, exec, lifecycle hooks)
    # is dark.
    if [ -x /usr/local/bin/mvm-guest-agent ]; then
      /bin/busybox setsid /usr/local/bin/setpriv \
        --reuid=${toString agentUid} --regid=${toString agentUid} \
        --clear-groups --no-new-privs \
        -- /usr/local/bin/mvm-guest-agent &
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
  # was being staged; the W7.x.2 microsandbox builder VM made the
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

    # mvm-guest-agent — installed under /usr/local/bin so /init can
    # exec it. Mode 0555 so the agent can't rewrite itself; ownership
    # is the build-time user (Nix sandbox has no root) — Phase 6 W2.2
    # binds /etc + /usr read-only at boot to make this load-bearing.
    # `mkdir -p` is load-bearing too: the FHS-skeleton mkdir at the
    # top of this script doesn't include `usr/local/bin` (the later
    # extra-packages section creates it, but that runs *after* this
    # cp). Without this line the build dies "cp: cannot create
    # regular file ... usr/local/bin/mvm-guest-agent: No such file
    # or directory" — the parent dir hasn't been made yet.
    mkdir -p "$out/usr/local/bin"
    cp ${agentBinary} "$out/usr/local/bin/mvm-guest-agent"
    chmod 0555 "$out/usr/local/bin/mvm-guest-agent"

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
      effectivePackages}

    # Kernel modules tree, when a matching kernel was supplied. We
    # copy (not symlink) so the rootfs is self-contained — the host's
    # /nix/store isn't visible to the guest at boot. /lib/modules is
    # made read-only so a compromised entrypoint can't replace a
    # module with one of its own choosing.
    ${pkgs.lib.optionalString hasKernel ''
      mkdir -p "$out/lib/modules"
      cp -R ${kernel}/lib/modules/${kernel.modDirVersion} \
        "$out/lib/modules/${kernel.modDirVersion}"
      chmod -R a-w "$out/lib/modules/${kernel.modDirVersion}"
    ''}
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
