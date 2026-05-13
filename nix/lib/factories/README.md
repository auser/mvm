# mvm Nix function-service factory

`mkFunctionService` bakes the wrapper + entrypoint files that mvm's
`RunEntrypoint` verb consumes (mvm ADR-007). Bundled into every
mvm-emitted artifact when the workload declares
`entrypoint.kind = "function"` (plan 0003 / ADR-0009).

## Files

- `mkFunctionService.nix` — single generic factory. Dispatches on the
  `language` input to the registry under `languages/`.
- `languages/default.nix` — language registry. Maps a language string
  to `{ language, runnerScript, servicePackages }`.
- `languages/python.nix` — Python entry. Bakes `pkgs.python3` and the
  Python wrapper from `nix/wrappers/python/`.
- `languages/node.nix` — Node entry. Bakes `pkgs.nodejs` and the Node
  wrapper from `nix/wrappers/node/`.

WASM is not yet in the registry — the user's `.wasm` IS the wrapper
(no interpreter package, different input semantics), so it will land
either as a registry entry with a tagged `wrapperKind` field or as a
sibling `mkWasmFunctionService` factory. Decision pending.

## Adding a language

One file, no dispatcher edit, no caller-side switch:

1. Drop `languages/<name>.nix` next to `python.nix` exporting
   `{ language, runnerScript, servicePackages }`.
2. Append the language to `languages/default.nix`'s attrset.
3. Append the bare name to
   `crates/mvm-ir/data/supported_languages.txt` so the IR validator
   accepts it as an `Entrypoint::Function.language` value.
4. Append the name to the `results = map testLanguage [ ... ]` list
   in `tests/factory_shape.nix`.

## Contract

```nix
mkFunctionService {
  pkgs,         # nixpkgs.legacyPackages.<system>
  language,     # "python" | "node" — registry key
  workloadId,   # workload id from the IR
  module,       # IR entrypoint.module
  function,     # IR entrypoint.function
  format,       # IR entrypoint.format ("json" | "msgpack")
  appPkg,       # the user-source derivation (per ADR-0008)
  sourcePath ? "/app",
  concurrency ? null,   # ADR-0011 — opts into warm-process tier
}
```

Returns the `{ extraFiles, servicePackages, service }` triple a
downstream `mkGuest` composition layer consumes.

`extraFiles` always contains:
- `/etc/mvm/entrypoint` → `/usr/lib/mvm/wrappers/runner`
- `/usr/lib/mvm/wrappers/runner` → the language's wrapper script
  (cold-tier `oneshot.*` or warm-tier `longrunning.*` depending on
  `concurrency`).
- `/etc/mvm/wrapper.json` → wrapper config (module, function, format,
  working_dir, mode).
- `/etc/mvm/runtime.json` → agent config (language, module, function,
  format, source_path, optional concurrency).

## Hardening invariant

v1 wrapper hardening lives inside the per-language wrapper sources
(`nix/wrappers/<lang>/{oneshot,longrunning}.{py,mjs}`), which mirror
the audited Rust `mvm-runner` crate's semantics. A follow-up PR
replaces the inlined script with the compiled `mvm-runner` binary
baked at `/usr/lib/mvm/wrappers/runner`. Until then, **changes to
mvm-runner's hardening must be mirrored into the wrappers** (and
vice versa).
