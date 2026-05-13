# Python entry in the function-service language registry.
#
# Plan 60 Phase 5 Slice E1. `mkFunctionService` looks up
# `languages.python` to know which interpreter package to bake into
# the rootfs and which wrapper-script source to inline at
# `/usr/lib/mvm/wrappers/runner`.
#
# Wrapper variants:
# - cold tier (concurrency == null): single-call `oneshot.py`.
# - warm-process tier: long-running `longrunning.py` speaking the
#   framed multi-call protocol (`mvm_guest::worker_protocol`).
# Same install path either way; the agent dispatches the same way and
# the wrapper itself decides whether to loop. ADR-0011 W-WRAPPER.

{ pkgs, concurrency }:

let
  runnerSource =
    if concurrency == null
    then ../../../wrappers/python/oneshot.py
    else ../../../wrappers/python/longrunning.py;
in
{
  language = "python";
  runnerScript = builtins.readFile runnerSource;
  servicePackages = [ pkgs.python3 ];
}
