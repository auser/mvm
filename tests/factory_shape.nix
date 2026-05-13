# Offline smoke test for the function-service factory contract.
# Asserts that `mvm.lib.<system>.mkFunctionService` evaluates and
# returns the `{ extraFiles, servicePackages, service }` triple that
# `mkGuest`'s composition layer consumes.
#
# Originally written as plan-48 spec (one factory per language); now
# exercises the unified `mkFunctionService` introduced by plan 60
# Phase 5 Slice E1, parameterized by `language`. Adding a language to
# the registry is verified by appending its name to `results` below.
#
# Run via:
#
#   nix eval --no-warn-dirty --impure --raw --expr '
#     let flake = builtins.getFlake "path:/Users/auser/work/tinylabs/mvmco/mvm/nix";
#     in import /Users/auser/work/tinylabs/mvmco/mvm/tests/factory_shape.nix { flake = flake; }'
#
# Asserts (per language):
#   1. `extraFiles` is an attrset containing `/etc/mvm/entrypoint` and
#      the wrapper at `/usr/lib/mvm/wrappers/runner`.
#   2. `servicePackages` is a list.
#   3. `service` is an attrset with at least `command` and `env`.

{ flake }:

let
  system = "aarch64-linux";
  pkgs = import flake.inputs.nixpkgs { inherit system; };
  lib = pkgs.lib;
  appPkg = pkgs.writeText "stub-app" "stub";
  mkFunctionService = flake.lib.${system}.mkFunctionService;

  testLanguage =
    language:
    let
      out = mkFunctionService {
        inherit pkgs appPkg language;
        workloadId = "test-${language}";
        module = "main";
        function = "handler";
        format = "json";
      };
      ok =
        out ? extraFiles
        && lib.isAttrs out.extraFiles
        && (out.extraFiles ? "/etc/mvm/entrypoint")
        && (out.extraFiles ? "/usr/lib/mvm/wrappers/runner")
        && out ? servicePackages
        && lib.isList out.servicePackages
        && out ? service
        && lib.isAttrs out.service
        && out.service ? command
        && out.service ? env;
    in
    if ok then "${language}: ok" else "${language}: FAIL ${builtins.toJSON (builtins.attrNames out)}";

  # Each language entry in the registry should evaluate via the
  # unified factory. Adding a language is appending its name here.
  results = map testLanguage [ "python" "node" ];

  allOk = builtins.all (r: lib.hasInfix ": ok" r) results;
in
if allOk then
  "factory_shape: ${toString (builtins.length results)}/${toString (builtins.length results)} passed (${lib.concatStringsSep ", " results})"
else
  "factory_shape: FAIL — ${lib.concatStringsSep "; " results}"
