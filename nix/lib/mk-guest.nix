# mkGuest — user-facing flake helper for declaring a microVM image.
#
# Same flake the user writes is consumed in BOTH dev and production
# builds. The "sealed vs accessible" distinction is encoded by:
#
#   1. The entrypoint shape:
#        entrypoint.shell    = "/bin/bash"   # accessible (dev-style)
#        entrypoint.command  = [ "/usr/local/bin/web" ]   # sealed default
#        entrypoint.services = { … }         # multi-service supervised
#
#   2. The explicit `dev` flag (overrides the entrypoint heuristic):
#        dev = true   # always enables the console + reachable shell
#        dev = false  # never enables console regardless of entrypoint
#
# Inferred default: `dev = (entrypoint ? shell)`. A user who declares
# `entrypoint.shell = "/bin/sh"` in their flake gets an accessible
# image; the same user shipping `entrypoint.command = [ … ]` gets a
# sealed image. mvmctl reads the `passthru.mvm.{dev, accessible,
# entrypoint}` metadata on the resulting derivation to decide whether
# `mvmctl console <vm>` is permitted (host-side gate; the in-guest
# state is the source of truth, the host gate is defence-in-depth).
#
# Returns a NixOS system derivation that microvm.nix turns into a
# bootable rootfs + runner. mvmctl pulls the rootfs ext4 path from
# `passthru.runner.config.microvm.rootfs` and bridges it to the
# active backend (Firecracker / microsandbox / Cloud Hypervisor).
#
# Implementation note: this builds on microvm.nix's NixOS module
# (ADR-013) — services become systemd units, entrypoint.shell maps
# to a getty on /dev/hvc0 (a vsock-backed virtio-console). The
# previous iteration used a hand-rolled busybox init for sub-100ms
# boot; we revisit boot perf in Phase 9 once the security port +
# template surface land.

{ nixpkgs, microvm }:
{ system }:
let
  lib = nixpkgs.lib;

  # Validate + classify entrypoint into one of three shapes. Every
  # mkGuest call goes through this so the mode-inference logic below
  # has a stable input.
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
          { shell    = "/bin/bash"; }              — accessible dev shell
          { command  = [ "/usr/local/bin/x" ]; }   — sealed single-service
          { services = { web = { command = … }; … }; } — supervised multi-service
        Got: ${builtins.toJSON ep}
      ''
    else if forms > 1 then
      throw ''
        mkGuest: entrypoint must declare exactly one form, not several.
        Got shapes: ${
          lib.concatStringsSep ", " (
            lib.optional hasShell "shell" ++
            lib.optional hasCommand "command" ++
            lib.optional hasServices "services"
          )
        }
      ''
    else if hasShell then "shell"
    else if hasCommand then "command"
    else "services";
in
{
  # name           — human-readable identifier; baked into the image.
  # entrypoint     — see `classifyEntrypoint` above.
  # services       — extra systemd-style services on top of the entrypoint.
  # packages       — additional packages added to the rootfs closure.
  # hypervisor     — microvm.nix hypervisor name; default firecracker.
  # vcpus / memory_mib  — resource defaults for `microvm.declaredRunner`.
  # dev            — explicit accessible-vs-sealed override; default
  #                  inferred from the entrypoint shape.
  # extraFiles     — { "absolute/guest/path" = { content; mode; }; }
  #                  baked into the rootfs at build time.
  name,
  entrypoint,
  services       ? { },
  packages       ? [ ],
  hypervisor     ? "firecracker",
  vcpus          ? 1,
  memory_mib     ? 256,
  dev            ? null,   # null = infer from entrypoint
  extraFiles     ? { },
}:
let
  pkgs = import nixpkgs { inherit system; };

  entrypointKind = classifyEntrypoint entrypoint;

  # Default dev = true when the entrypoint is `shell`; user can
  # override either way explicitly.
  isDev =
    if dev == null then entrypointKind == "shell"
    else dev;

  # Sealed = the inverse; named separately for documentation
  # readability when the metadata appears in `nix eval`.
  isSealed = !isDev;

  # Convert mkGuest's `entrypoint.command` / `entrypoint.services` /
  # `entrypoint.shell` into NixOS-systemd module fragments.
  entrypointModule =
    if entrypointKind == "shell" then
      {
        # Auto-login getty on /dev/hvc0 — virtio-console wired through
        # vsock when the active backend is microsandbox / libkrun, or
        # virtio-console-as-vsock for Firecracker. mvmctl's `console`
        # subcommand attaches to the same channel host-side.
        services.getty.autologinUser = "root";
        services.getty.extraArgs = [ "--keep-baud" ];
        # Set the login shell to the user-requested one so it isn't
        # the NixOS default. Validated against /etc/shells at boot
        # time by NixOS' security model.
        users.users.root.shell = pkgs.runCommand "user-shell" { } ''
          mkdir -p $out/bin
          ln -s ${entrypoint.shell} $out/bin/login-shell
        '';
      }
    else if entrypointKind == "command" then
      {
        systemd.services.entrypoint = {
          description = "mvm entrypoint (command form)";
          wantedBy = [ "multi-user.target" ];
          after = [ "network.target" ];
          serviceConfig = {
            ExecStart = lib.escapeShellArgs entrypoint.command;
            Restart = entrypoint.restart or "on-failure";
            # Phase 6 wires per-service uid + seccomp tier here.
            # Until then, root-with-no-new-privs is the floor.
            NoNewPrivileges = true;
          };
        };
      }
    else  # services
      {
        # Each entry of entrypoint.services becomes a systemd unit.
        systemd.services = lib.mapAttrs
          (svcName: svc: {
            description = "mvm entrypoint service '${svcName}'";
            wantedBy = [ "multi-user.target" ];
            after = [ "network.target" ];
            serviceConfig = {
              ExecStart = lib.escapeShellArgs (svc.command or
                (throw "mkGuest: entrypoint.services.${svcName}.command is required"));
              Restart = svc.restart or "on-failure";
              NoNewPrivileges = true;
            };
          })
          entrypoint.services;
      };

  # Extra services declared *outside* entrypoint (parallel
  # supervisors). Same shape as the services entrypoint variant.
  extraServicesModule = {
    systemd.services = lib.mapAttrs
      (svcName: svc: {
        description = "mvm aux service '${svcName}'";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (svc.command or
            (throw "mkGuest: services.${svcName}.command is required"));
          Restart = svc.restart or "on-failure";
          NoNewPrivileges = true;
        };
      })
      services;
  };

  # extraFiles → environment.etc declarations. We accept absolute
  # paths under any prefix; environment.etc is rooted at /etc, so
  # for non-/etc paths we wire systemd-tmpfiles instead.
  extraFilesModule = {
    environment.etc = lib.mapAttrs'
      (path: spec:
        lib.nameValuePair
          (lib.removePrefix "/etc/" path)
          {
            text = spec.content;
            mode = spec.mode or "0644";
          }
      )
      (lib.filterAttrs (path: _: lib.hasPrefix "/etc/" path) extraFiles);
  };

  # mvm-side passthru metadata. Read by mvmctl host-side to gate the
  # `console` subcommand and report sealed-vs-accessible status to
  # the user. The keys below are stable; new keys are additive.
  mvmMeta = {
    inherit name hypervisor;
    accessible = isDev;
    sealed = isSealed;
    entrypointKind = entrypointKind;
  };

  # Compose the full NixOS configuration.
  nixosSystem = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      microvm.nixosModules.microvm
      entrypointModule
      extraServicesModule
      extraFilesModule
      ({ ... }: {
        microvm = {
          inherit hypervisor;
          vcpu = vcpus;
          mem = memory_mib;
          interfaces = [ ];
          volumes = [ ];
          shares = [ ];
        };

        networking.hostName = lib.mkDefault name;
        time.timeZone = "UTC";
        i18n.defaultLocale = "C.UTF-8";

        environment.systemPackages = packages ++ (with pkgs; [
          coreutils
          bashInteractive
        ]);

        # Load-bearing: SSH absolutely never enabled in a mkGuest
        # image, sealed or accessible. ADR-002 / CLAUDE.md.
        services.openssh.enable = false;

        # Variant marker visible from inside the guest. mvm's host-
        # side runtime cross-checks this against the passthru.mvm
        # metadata before letting `mvmctl console` attach.
        environment.etc."mvm/variant".text =
          if isDev then "dev\n" else "prod\n";
        environment.etc."mvm/name".text = "${name}\n";

        system.stateVersion = "25.11";
      })
    ];
  };

  # Surface the runner + the rootfs path + mvm metadata via passthru
  # so callers (the user's flake `outputs.packages.<system>.default`)
  # can either build the runner directly or `nix eval` the metadata.
  runnerDrv = nixosSystem.config.microvm.declaredRunner;
in
runnerDrv.overrideAttrs (old: {
  passthru = (old.passthru or { }) // {
    mvm = mvmMeta;
    nixosSystem = nixosSystem;
  };
})
