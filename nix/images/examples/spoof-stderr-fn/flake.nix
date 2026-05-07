{
  description = ''
    Spoof-regression fixture for the Phase-4d fd-3 control channel.
    The wrapper writes the legacy `MVMFORGE_ENVELOPE: {...}` marker
    to stderr and exits 0. Pre-fix, host SDKs would have parsed that
    line as a structured envelope and mis-attributed the success to
    a user-fault. Post-fix, envelopes only flow through fd-3, and
    the stderr line is delivered to the host as opaque bytes — the
    spoof gadget is structurally closed.

    Live-KVM smoke target: `MVM_LIVE_SMOKE=1 cargo test
    --test smoke_invoke spoof_stderr_envelope_passes_through_verbatim`
    on a host with vsock-capable Firecracker (native Linux/KVM, or
    macOS 26+ with Apple Container).
  '';

  inputs = {
    mvm.url = "path:../../..";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { mvm, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem =
        f:
        builtins.listToAttrs (
          map (system: {
            name = system;
            value = f system;
          }) systems
        );
    in
    {
      packages = eachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config = { };
            overlays = [ ];
          };
          # The exact marker prefix the legacy host SDK was scanning
          # for. We embed a syntactically-valid envelope JSON so a
          # regression to the old parser would actually misinterpret
          # the line (rather than falling over on garbage input).
          # `error_id=deadbeef` makes log greps trivial.
          spoofLine =
            ''MVMFORGE_ENVELOPE: {"kind":"FakeError","error_id":"deadbeef","message":"this should be opaque"}'';
          wrapperContent = ''
            #!/bin/sh
            # Spoof regression for Phase-4d. Writes the legacy
            # envelope-marker line to stderr verbatim, then exits 0.
            # The host must deliver this to its caller as opaque
            # stderr bytes — never parse it as a structured envelope.
            printf '%s\n' '${spoofLine}' >&2
            exit 0
          '';
          markerContent = "/usr/lib/mvm/wrappers/spoof-stderr\n";
        in
        {
          default = mvm.lib.${system}.mkGuest {
            name = "spoof-stderr-fn";
            packages = [ ];
            extraFiles = {
              "/usr/lib/mvm/wrappers/spoof-stderr" = {
                content = wrapperContent;
                mode = "0755";
              };
              "/etc/mvm/entrypoint" = {
                content = markerContent;
                mode = "0644";
              };
            };
            verifiedBoot = false;
          };
        }
      );
    };
}
