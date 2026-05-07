# mkPythonFunctionService — bake a Python function-call workload.
#
# Plan 0003 phase 4 / ADR-0009. Returns the `extraFiles`,
# `servicePackages`, and `service` triple the generated mvmforge flake
# composes into mvm's `mkGuest`.
#
# Item-6 cleanup (post-Phase-5): the runner is no longer inlined into
# this Nix file as a string literal. The factory now consumes the
# canonical wrapper template at `nix/wrappers/python-runner.py` via
# `pkgs.lib.fileContents`, with the shebang substituted to use the
# Nix-store python path so the rootfs doesn't need `/usr/bin/env`.
# Per-workload runtime config is written to `/etc/mvm/wrapper.json`,
# which the canonical wrapper reads at startup.
#
# Inputs:
#   pkgs        — nixpkgs.legacyPackages.<system>
#   workloadId  — workload id from the IR
#   module      — IR entrypoint.module
#   function    — IR entrypoint.function
#   format      — IR entrypoint.format ("json" | "msgpack")
#   appPkg      — derivation built from the bundled user source
#                 (per ADR-0008)
#   sourcePath  — absolute path inside the rootfs where the user
#                 source tree lives (e.g. "/app")
#   mode        — "prod" (default) or "dev". Dev surfaces the
#                 traceback alongside the envelope on failure;
#                 prod scrubs both. Should never be "dev" for
#                 production workloads.
#
# Outputs (record):
#   extraFiles      — passed straight to mvm's `mkGuest extraFiles`
#   servicePackages — extra packages required in the rootfs
#   service         — `services.<workloadId>` entry for mkGuest

{
  pkgs,
  workloadId,
  module,
  function,
  format,
  appPkg,
  sourcePath ? "/app",
  mode ? "prod",
}:

assert pkgs.lib.assertOneOf "format" format [
  "json"
  "msgpack"
];
assert pkgs.lib.assertOneOf "mode" mode [
  "prod"
  "dev"
];

let
  python = pkgs.python3;

  # Per-workload wrapper config. Schema mirrors what the canonical
  # `nix/wrappers/python-runner.py` reads from `/etc/mvm/wrapper.json`
  # at startup — adding a field here without updating the canonical
  # wrapper is a silent no-op; renaming a field breaks parsing.
  wrapperJson = builtins.toJSON {
    inherit module function format mode;
    working_dir = sourcePath;
    # 1 MiB v1 cap (mvm ADR-007 §M1). The agent enforces a hard cap
    # upstream; this is wrapper-side defense in depth.
    max_input_bytes = 1048576;
    # Read by the agent's `RunCode` vsock verb to pick the right
    # interpreter (`python3 -c`). Stable identifier — mvmforge's
    # SDK and downstream tools may match on it.
    language = "python";
  };

  # Substitute the canonical wrapper's `#!/usr/bin/env python3`
  # shebang for an absolute Nix-store path. Keeps the canonical file
  # portable for ad-hoc testing on hosts with `/usr/bin/env`, while
  # the rootfs gets a self-contained reference.
  rawRunner = pkgs.lib.fileContents ../../wrappers/python-runner.py;
  runnerScript = builtins.replaceStrings
    [ "#!/usr/bin/env python3" ]
    [ "#!${python}/bin/python3" ]
    rawRunner;

in
{
  extraFiles = {
    "/etc/mvm/entrypoint" = {
      content = "/usr/lib/mvm/wrappers/runner";
      mode = "0644";
    };
    "/usr/lib/mvm/wrappers/runner" = {
      content = runnerScript;
      mode = "0755";
    };
    "/etc/mvm/wrapper.json" = {
      content = wrapperJson;
      mode = "0644";
    };
  };

  servicePackages = [ python ];

  # The agent (mvm `RunEntrypoint`) execs the wrapper directly per
  # call; no long-running service is needed. The `service` slot stays
  # populated for `mkGuest`'s shape — it requires every workload-id
  # key to declare a service block — but the command is a no-op
  # idle loop. preStart wires the appPkg symlink as today.
  service = {
    command = pkgs.writeShellScript "${workloadId}-noop" ''
      #!${pkgs.stdenv.shell}
      exec ${pkgs.coreutils}/bin/sleep infinity
    '';
    preStart = pkgs.writeShellScript "${workloadId}-prestart" ''
      set -eu
      mkdir -p "$(dirname ${sourcePath})"
      ln -sfn ${appPkg} ${sourcePath}
    '';
    env = { };
  };
}
