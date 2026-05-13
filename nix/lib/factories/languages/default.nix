# Function-service language registry.
#
# Plan 60 Phase 5 Slice E1. `mkFunctionService` looks up a language
# entry here to learn:
# - which interpreter / runtime package to bake into the rootfs
#   (`servicePackages`)
# - which wrapper-script source to inline at
#   `/usr/lib/mvm/wrappers/runner` (`runnerScript`)
# - the canonical language string emitted into `/etc/mvm/runtime.json`
#   (`language`) — must match the value `mvm-ir`'s validator allowlists
#   in `crates/mvm-ir/data/supported_languages.txt`
#
# Adding a language is a single-file change: drop `<name>.nix` next to
# `python.nix` exporting the same triple, append it to the attrset
# below, append the bare name to mvm-ir's `supported_languages.txt`,
# done. No factory-dispatcher edit, no caller-side switch statement.
#
# Wasm intentionally lives outside the registry today because its
# inputs differ (the user's `.wasm` module IS the wrapper; no
# interpreter package is baked). When wasm lands it will either
# extend the registry shape (with optional `wrapperKind` + per-kind
# baking logic) or stay sibling-factory; the call hasn't been made.

{ pkgs, concurrency }:

{
  python = import ./python.nix { inherit pkgs concurrency; };
  node = import ./node.nix { inherit pkgs concurrency; };
}
