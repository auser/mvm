# mkNodeFunctionService — bake a Node.js function-call workload.
#
# Plan 0003 phase 4 / ADR-0009. Mirror of mkPythonFunctionService.nix
# for the Node runtime. See that file for the design notes; this one
# only covers the Node-specific wrinkles.
#
# Item-6 cleanup: same shape as the Python factory — the runner now
# comes from `nix/wrappers/node-runner.mjs` via `pkgs.lib.fileContents`,
# with the shebang substituted to the Nix-store node path. Per-workload
# config is written to `/etc/mvm/wrapper.json`.

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
  node = pkgs.nodejs_22;

  wrapperJson = builtins.toJSON {
    inherit module function format mode;
    working_dir = sourcePath;
    max_input_bytes = 1048576;
  };

  rawRunner = pkgs.lib.fileContents ../../wrappers/node-runner.mjs;
  runnerScript = builtins.replaceStrings
    [ "#!/usr/bin/env node" ]
    [ "#!${node}/bin/node" ]
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

  servicePackages = [ node ];

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
