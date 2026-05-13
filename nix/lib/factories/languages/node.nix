# Node.js entry in the function-service language registry.
#
# Plan 60 Phase 5 Slice E1. Same shape as `python.nix`. `.mjs`
# extensions are deliberate — they make Node load the wrappers as ESM
# without needing `--input-type=module` on argv (the agent execs
# `node /usr/lib/mvm/wrappers/runner` and Node infers ESM from the
# extension).

{ pkgs, concurrency }:

let
  runnerSource =
    if concurrency == null
    then ../../../wrappers/node/oneshot.mjs
    else ../../../wrappers/node/longrunning.mjs;
in
{
  language = "node";
  runnerScript = builtins.readFile runnerSource;
  servicePackages = [ pkgs.nodejs ];
}
