# mkFunctionService — bake a function-call workload (ADR-0009 / plan
# 0003 phase 4). Generic across languages: the `language` input drives
# a registry lookup (`languages/<lang>.nix`) that contributes the
# interpreter package + wrapper-script source. The factory body itself
# is identical for every language — it composes the `extraFiles` /
# `servicePackages` / `service` triple that mvm's `mkGuest` consumes.
#
# Plan 60 Phase 5 Slice E1. Replaces mvmforge's per-language factories
# (`mkPythonFunctionService.nix`, `mkNodeFunctionService.nix`) with a
# single dispatcher + a language registry. Adding a new language is
# now one file under `languages/`, not a new factory + caller switch.
#
# v1 wrapper hardening lives inside the per-language wrapper sources
# (`nix/wrappers/python/{oneshot,longrunning}.py`, same for `node/*.mjs`),
# which mirror the audited Rust `mvm-runner` crate's semantics. A
# follow-up PR replaces the inlined script with the compiled
# `mvm-runner` binary at `/usr/lib/mvm/wrappers/runner`; until then,
# **changes to mvm-runner's hardening must be mirrored into the
# wrappers.**
#
# Inputs:
#   pkgs        — nixpkgs.legacyPackages.<system>
#   language    — "python" | "node" (look up in `languages/`)
#   workloadId  — workload id from the IR
#   module      — IR entrypoint.module
#   function    — IR entrypoint.function
#   format      — IR entrypoint.format ("json" | "msgpack")
#   appPkg      — derivation built from the bundled user source
#                 (per ADR-0008)
#   sourcePath  — absolute path inside the rootfs where the user
#                 source tree lives (e.g. "/app")
#   concurrency — optional ADR-0011 concurrency block. When non-null,
#                 the registry picks the language's `longrunning`
#                 wrapper instead of `oneshot`, and the agent picks
#                 the value out of `/etc/mvm/runtime.json` to start
#                 the warm-process pool. Null = cold tier (today's
#                 default).
#
# Outputs (record):
#   extraFiles      — passed straight to mvm's `mkGuest extraFiles`
#   servicePackages — extra packages required in the rootfs
#   service         — `services.<workloadId>` entry for mkGuest

{ pkgs
, language
, workloadId
, module
, function
, format
, appPkg
, sourcePath ? "/app"
, concurrency ? null
,
}:

let
  languages = import ./languages { inherit pkgs concurrency; };
  lang =
    languages.${language} or (throw ''
      mkFunctionService: language "${language}" has no entry in the
      language registry. Available: ${builtins.concatStringsSep ", " (builtins.attrNames languages)}.
      Hint: add `nix/lib/factories/languages/${language}.nix` + append
      to `nix/lib/factories/languages/default.nix`, then add the bare
      name to `crates/mvm-ir/data/supported_languages.txt` so the IR
      validator accepts it.
    '');

  # Per-workload runtime config. Mirrors the IR field set on
  # `Entrypoint::Function` (see `crates/mvm-ir/src/workload.rs`) plus
  # the resolved source path. Baked into the rootfs at build time —
  # nothing here is decided at call time.
  #
  # The `concurrency` block (ADR-0011) opts the agent into the
  # warm-process worker pool. Schema mirrors `mvm_guest::
  # runtime_config::RuntimeConfig` exactly so mvm's
  # `serde(deny_unknown_fields)` parse succeeds.
  runtimeJson = builtins.toJSON (
    {
      language = lang.language;
      inherit module function format;
      source_path = sourcePath;
    }
    // (if concurrency == null then { } else { inherit concurrency; })
  );

  # `/etc/mvm/wrapper.json` — config the per-language wrapper reads
  # at startup (separate consumer from `runtime.json`, which is the
  # mvm agent's input). Schema in `nix/wrappers/README.md`.
  wrapperJson = builtins.toJSON {
    inherit module function format;
    working_dir = sourcePath;
    mode = "prod";
  };
in
{
  extraFiles = {
    "/etc/mvm/entrypoint" = {
      content = "/usr/lib/mvm/wrappers/runner";
      mode = "0644";
    };
    "/usr/lib/mvm/wrappers/runner" = {
      content = lang.runnerScript;
      mode = "0755";
    };
    "/etc/mvm/wrapper.json" = {
      content = wrapperJson;
      mode = "0644";
    };
    "/etc/mvm/runtime.json" = {
      content = runtimeJson;
      mode = "0644";
    };
  };

  servicePackages = lang.servicePackages;

  # The agent (mvm `RunEntrypoint`) execs the wrapper directly per
  # call; no long-running service is needed. The `service` slot stays
  # populated for `mkGuest`'s shape — it requires every workload-id
  # key to declare a service block — but the command is a no-op idle
  # loop. preStart wires the appPkg symlink as today.
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
