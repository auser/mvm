# Plan 71 — `mkFunctionWorkload` IR-to-image helper

Stacks on plans 48 + 49 (function-service factories + wrapper-templates relocation) and on plan 60 Phase 5 Slice E1 (the unified `mkFunctionService` factory). Closes the gap between "factory exists" and "user can author a function workload's `flake.nix` in one line."

## Context

`mkFunctionService` (plan 60 Phase 5 Slice E1, `nix/lib/factories/mkFunctionService.nix`) bakes the per-language wrapper files a function-call workload needs — `/etc/mvm/wrapper.json`, the runner symlink at `/etc/mvm/entrypoint`, and the wrapper script at `/usr/lib/mvm/wrappers/runner`. It's exposed as `mvm.lib.<system>.mkFunctionService` but **nothing in the IR-to-image pipeline calls it** today: a user authoring a function workload would have to manually pull the IR's module / function / format out and feed them to the factory, then compose with `mkGuest`.

This slice (the "E2 pipeline-integration" follow-on referenced in the Phase 5 slice sequence) closes that gap by adding a single Nix helper, `mkFunctionWorkload`, that:

1. Reads the workload's IR JSON (`builtins.fromJSON (builtins.readFile irFile)`).
2. Extracts the primary function entrypoint's fields.
3. Calls `mkFunctionService` for those fields.
4. Composes the result with `mkGuest` to produce a full rootfs.

After this slice a user's `flake.nix` for a function workload is one line:

```nix
packages.${system}.default = mvm.lib.${system}.mkFunctionWorkload {
  irFile = ./workload-ir.json;
  appPkg = ./src;
};
```

This unblocks **Phase 5 Slice E3** (live-VM smoke): the Python SDK's `mvm.emit_json()` writes the IR, the one-line flake.nix consumes it, `mvmctl up --flake` boots the resulting image, and `mvmctl invoke` dispatches the same function the `--no-vm` shortcut already validates (slice E1b SDK wiring, commit `e030786`).

## Out of scope (deferred)

- **Rust-side flake.nix + launch.json generator.** The user still authors both files today; this slice does not change that. Natural follow-up: `mvmctl emit-flake <workload-dir>` that synthesizes both from the IR. After this slice the generated flake.nix is **one line** calling `mkFunctionWorkload`, so the Rust template stays trivial; the value-add is owning `launch.json`'s `ir_hash` / `toolchain_version` / `artifact_format_version` fields (which fake-mvm already enforces but nothing currently writes for the in-repo Python SDK path). A small additional slice, not a major rewrite.
- **Multi-app workloads.** `mkFunctionWorkload` asserts `length apps == 1` and rejects otherwise with a hint pointing at the long-form `mkGuest` composition.
- **Multi-function workloads (ADR-0014 Phase 2).** The factory bakes only the primary entrypoint; non-primary entrypoints are flagged in a warning.
- **wasm.** The factory's language registry doesn't include wasm — `mkFunctionWorkload` raises the same `unsupported language` error the factory does today.
- **`image.kind` shapes other than `nix_packages`.** Rejected loudly with a clear error pointing at the long form.

## Approach

### Files to add / modify

**Add: `nix/lib/mkFunctionWorkload.nix`** — the new helper. Reads IR JSON, extracts the primary function entrypoint, invokes `mkFunctionService`, composes with `mkGuest`. Pure-Nix; no Rust changes.

Signature (final attribute names settled during implementation):
```nix
mkFunctionWorkload {
  irFile,           # path to workload-ir.json
  appPkg,           # the user's source-tree derivation (per ADR-0008)
  hypervisor    ? "firecracker",
  vcpus         ? 1,
  memory_mib    ? 256,
  extraPackages ? [],    # additional packages on top of image.packages from the IR
  extraExtraFiles ? {},  # additional extraFiles merged after the factory's
}
```

Returns: the `rootfsImage` derivation `mkGuest` returns.

Behavior:
1. `ir = builtins.fromJSON (builtins.readFile irFile)`.
2. Validate `length ir.apps == 1`, `ir.apps.[0].entrypoints` contains exactly one `kind == "function"` primary entry. Fail with operator-actionable hints otherwise.
3. Extract `{ language, module, function, format, working_dir }` from the primary entrypoint.
4. Extract `image.kind == "nix_packages"` + `image.packages`; reject other `image.kind` values with a clear error.
5. Call `mkFunctionService { pkgs, language, workloadId = ir.id, module, function, format, appPkg, sourcePath = working_dir, }`.
6. Call `mkGuest { name = ir.id; entrypoint.services = { mvm-function = factoryOutput.service; }; extraFiles = factoryOutput.extraFiles // extraExtraFiles; packages = factoryOutput.servicePackages ++ imagePackages ++ extraPackages; … }`.

The "match the factory's output shape to `mkGuest`'s inputs" piece is structurally sound: `mkGuest` (`nix/lib/mk-guest.nix:71-81`) already accepts `extraFiles`, `packages`, and an `entrypoint` that can take a `services` map — the exact shape `mkFunctionService` emits.

**Modify: `nix/lib/default.nix`** — add `mkFunctionWorkload = import ./mkFunctionWorkload.nix { inherit nixpkgs microvm mvmSrc; };` and re-export it from the public lib attrset alongside `mkGuest` and `mkFunctionService`. Keep existing exports unchanged.

**Modify: `tests/factory_shape.nix`** — add a `workload_shape` test target that writes a synthetic IR JSON (via `pkgs.writeText`) representing a single-app, single-function workload, calls `mkFunctionWorkload`, and asserts the resulting derivation's `passthru.mvm.entrypointKind == "services"` and that the rootfs tree contains the three expected files (`/etc/mvm/entrypoint`, `/etc/mvm/wrapper.json`, `/usr/lib/mvm/wrappers/runner`). The existing `factory_shape` test which exercises `mkFunctionService` directly stays as a unit-shape gate.

**Modify: `nix/lib/factories/README.md`** — extend with a short section documenting `mkFunctionWorkload` as the recommended user-facing entry point. The factory remains documented for advanced users who want custom `mkGuest` composition.

### Reused utilities (do not reinvent)

- `nix/lib/mk-guest.nix:71-81` — `mkGuest` accepts the `{ extraFiles, services, packages }` shape `mkFunctionService` emits. Composition is attribute-spread; no transformation layer needed.
- `nix/lib/factories/mkFunctionService.nix` — validates language, format, working_dir; emits `runtime.json` + `wrapper.json` + entrypoint symlink files. `mkFunctionWorkload` orchestrates inputs and consumes the factory's triple.
- `nix/lib/factories/languages/default.nix` — language registry. `mkFunctionWorkload` does not re-implement the language allowlist; an unsupported language bubbles up through the factory's existing error.

### IR shape this slice supports

Reads only these fields (others ignored — `builtins.fromJSON` returns the full tree but unused branches stay unread):

- `ir.id` — workload id → `mkGuest`'s `name` and `mkFunctionService`'s `workloadId`.
- `ir.apps[0].image.kind == "nix_packages"`, `ir.apps[0].image.packages`.
- `ir.apps[0].entrypoints[*]` where exactly one has `kind == "function"` and `primary == true`. From that entry: `language`, `module`, `function`, `format`, `working_dir`.

Anything else (network, dependencies, mounts, addons, resources) is rejected loudly with a hint pointing at the long-form composition. This keeps the helper honest about its scope; users with richer needs drop down to `mkGuest` + `mkFunctionService` directly.

## Verification

1. **Existing factory tests still pass:**
   ```
   nix eval --impure --raw -f tests/factory_shape.nix factory_shape
   ```
   Expected (unchanged from Slice E1): `factory_shape: 2/2 passed (python: ok, node: ok)`.

2. **New `mkFunctionWorkload` test passes:**
   ```
   nix eval --impure --raw -f tests/factory_shape.nix workload_shape
   ```
   Expected: `workload_shape: ok (python passthru.mvm.entrypointKind=services, all three extraFiles present)`.

3. **No Rust regressions:**
   ```
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   ```
   Should report all-green; this slice touches no Rust.

4. **No Python SDK regressions:**
   ```
   uv run pytest sdks/python
   ```
   Expected: 68 passed + 3 skipped (post-slice E1b baseline). The 8 pydantic-missing failures in `test_schema_derive.py` are pre-existing.

5. **End-to-end gate (manual, dev box with Nix only — no KVM required):**
   - In a tempdir, write a synthetic IR JSON file and a sibling `adder_module.py`.
   - Author a 6-line `flake.nix` calling `mkFunctionWorkload { irFile = ./workload-ir.json; appPkg = ./.; }`.
   - Run `nix build .#default` — confirm it succeeds and the resulting `result/` derivation contains the rootfs with `/etc/mvm/wrapper.json` baked correctly (inspectable via `nix store cat`).

The next slice (E3 live-VM smoke) layers on top: same flake produced here gets booted by `mvmctl up --flake`, with the Python SDK's `await f(2, 3) == 5` assertion driving the in-VM dispatch path that mirrors the `--no-vm` test slice (E1b) already exercises.

## Critical files

- `nix/lib/mkFunctionWorkload.nix` (new)
- `nix/lib/default.nix` (modify export attrset, currently lines 18-31)
- `nix/lib/factories/mkFunctionService.nix` (no changes — reused as-is)
- `nix/lib/mk-guest.nix` (no changes — reused as-is; lines 71-81 show the inputs, lines 295-310 show `extraFiles` population)
- `tests/factory_shape.nix` (extend with `workload_shape` target)
- `nix/lib/factories/README.md` (document `mkFunctionWorkload` as the recommended entry point)
