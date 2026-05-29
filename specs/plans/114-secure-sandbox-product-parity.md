# Plan 114: Secure sandbox product parity for mvm

**Status:** Proposed
**Date:** 2026-05-29
**Comparator:** secure sandbox runtime products, hosted function platforms, and the current MVM product positioning
**Goal:** bring `mvm` to product capability parity where it is compatible with the security-first, Nix-first microVM model, while preserving stronger `mvm` claims around signed plans, audited launches, builder VM isolation, cold mode, and rootfs integrity.

## Scope correction

This is not a website clone. The target is product capability parity:

- SDK-owned runtime lifecycle for sandbox creation, command execution, files, snapshots, and cleanup.
- Decorator-style build/deploy declarations for code-first workload authoring.
- Local runtime primitives in `mvm`.
- Secure build and launch path through the builder VM, signed plans, audit, and microVM backends.
- OCI compatibility without making OCI the core trust story.

## Runtime and decorator DX targets

The imperative runtime path should let callers create a `Sandbox`, run commands through `sandbox.commands.run(...)`, manage files, lifecycle, persistence, snapshots, metadata, metrics, secured access, and CLI operations. The code-first deployment path should support an app object, image declarations, function decorators, local runs, local entrypoints, secrets, volumes, schedules, service functions, and sandboxes.

The product site frames MVM as a secure microVM runtime for AI-native workloads: signed images and execution plans, policy-plane mediation, no ambient long-lived credentials, no bypass egress path, audit-chain records, and backend choice as an implementation detail. The plan below keeps that as the product center of gravity. Ergonomics should improve access to those controls, not hide or weaken them.

That maps well to the split we already have:

- Imperative runtime lifecycle is the `Sandbox.create(...)` side.
- Declarative build/deploy is the static decorator side.
- Product parity is the SDK, CLI, local runtime, snapshot, and policy story around those primitives.

Security-first interpretation:

- Prefer static decorator parsing for deployable workloads because it does not import or execute user modules on the host.
- Keep runtime `record` and `live` modes explicit because they intentionally run the user's script on the host process that invokes the SDK.
- Require SDK methods to return audit/run identifiers where the underlying `mvm` operation emits them.
- Treat secrets as references and grants, not SDK string literals.
- Keep network policy explicit and deny by default in examples.
- Treat cold/snapshot state as sensitive state with retention and restore verification.

## Existing local runtime implementation

The imperative runtime side is not starting from zero. The current tree already has:

- `sdks/python` with `mvm.Sandbox.create(...)`, `Sandbox.commands`, `Sandbox.files`, record mode, live mode, context-manager cleanup, and tests.
- `sdks/typescript` with `Sandbox.create(...)`, `SandboxCommands`, `SandboxFiles`, record mode, live mode, `Symbol.dispose` cleanup, and tests.
- `crates/mvm-sdk/src/runtime.rs` as the Rust lowering contract from runtime recordings into Workload IR.
- `mvmctl compile <Sandbox-script>` and `mvmctl run --mode plan|live <script>` as the local CLI transports.
- CLI-level proc, fs, pause/resume, snapshot, logs, and image/build primitives that the SDK should wrap.

The parity gap is therefore productization and completion, not invention. Remaining work should tighten method naming and behavior against secure sandbox runtime expectations and wire more primitives through the Python/TypeScript SDKs.

## Parity matrix

| Capability | Product target | Current mvm posture | Parity status |
| --- | --- | --- | --- |
| Runtime SDK lifecycle | SDK creates sandboxes, execs commands, transfers files, snapshots/stops. | Python/TS SDKs already support `Sandbox.create`, command start, file write, record mode, live mode; CLI has more lifecycle primitives than SDK exposes. | Partial |
| Python SDK | First-class SDK. | `sdks/python` exists with runtime and decorator-adjacent surfaces, tests, record/live transports. | Partial |
| TypeScript/Node SDK | First-class SDK. | `sdks/typescript` exists with runtime surface, tests, record/live transports. | Partial |
| Rust SDK | Lower-level typed authoring and Workload IR contract. | `mvm-sdk` exists and owns the lowering model. | Partial |
| C/native integration | Native/lower-level integration where product need justifies it. | No shipped C SDK; CLI subprocess integration is the current path. | Optional |
| Declarative decorators | Code-first decorator DX for deployable workloads. | `mvm-sdk` has decorator/IR work and static compile. | Lead/Partial |
| CLI | Sandbox lifecycle commands. | `mvmctl` has broad CLI surface, including build/up/exec/pause/snapshot/image. | Partial/Lead |
| OCI input | Registry/image compatibility. | OCI pull/materialization exists in code/docs but claim status needs careful gating; Nix remains preferred. | Partial |
| Nix-first builds | Reproducible build path with auditable artifacts. | Builder VM + Nix artifacts are core. | Lead |
| Persistent workspaces | Sandbox state can persist. | Volumes, snapshots, cold mode, and named VM registry exist; product story needs unification. | Partial/Lead |
| Cold mode | Snapshot and restore state as a product lifecycle. | Firecracker sealed pause/resume, Vz save/restore, pool Sleeping/Running paths. | Lead if documented and wired through SDK |
| Network policy/proxy | Explicit sandbox egress story. | L3/default-deny and L7 pieces exist or are planned. | Partial |
| Secret references | Secure secret reference story expected. | ADR-049/Plan 74 specify managed-ref flow; ADR-062 rescope dropped current host-services secrets. | Gap/Decision needed |
| SSH gateway | Some products expose SSH gateway access. | `mvm` security posture prefers no SSH in microVMs; console/vsock are current paths. | Deliberate non-goal unless separately justified |
| Observability | Metrics, logs, and audit identifiers. | `mvm` has audit, metrics, boot reports; SDK return values need more identifiers. | Partial/Lead in audit |
| Examples | Multi-language examples. | Existing examples plus new tutorial docs; runtime SDK examples are planned. | Partial |

## Required mvm workstreams

### W1 — Runtime SDK contract

Productize the existing local runtime SDK around current `mvm` primitives:

- `Sandbox.create(...)`
- `sandbox.commands.run(...)` or a compatibility alias over the current `commands.start(...)`
- `sandbox.files.read/write/list/remove(...)`
- `sandbox.logs(...)`
- `sandbox.ports.forward(...)`
- `sandbox.snapshot(...)`
- `sandbox.pause()` / `sandbox.resume()` / `sandbox.cold()`
- `sandbox.stop()` / `sandbox.destroy()`
- `sandbox.detach()`

Security requirements:

- every created sandbox has an owner lease;
- parent-death cleanup or explicit detach;
- secret-bearing errors/logs are redacted;
- network defaults to deny unless policy says otherwise;
- audit/run ids are returned to callers;
- cold/snapshot state is explicit in the type model.

Initial language priority:

1. Keep the Rust recording/lowering contract as the ground truth.
2. Finish Python runtime parity around command result capture, file read/list/remove, logs, ports, snapshots/cold mode, and lifecycle state.
3. Finish TypeScript/Node parity for the same fixture suite.
4. Go/C only after the product need is confirmed.

### W2 — Decorator SDK

Keep the decorator surface separate from lifecycle:

- static declaration parsing;
- Workload IR emission;
- Nix image/package selection;
- resource declarations;
- network policy declarations;
- secret references;
- build hooks and readiness hooks.

Security requirements:

- static compile should not import/execute user modules;
- record-mode/auto-run paths must be opt-in and clearly labeled;
- decorator secrets are references, not plaintext;
- generated IR is hashable and bindable to signed plans.

### W3 — Secure build and image parity

Product parity should not make OCI the center of the trust story. The product should expose:

- Nix-first build path through the builder VM;
- persistent developer builder VM via `cargo run -- dev up`;
- persistent non-interactive build worker via `cargo run -- build`;
- OCI input as compatibility path;
- digest-pinned production policy;
- artifact provenance;
- runtime overlay and verified artifact checks where backend-supported.

### W4 — Cold mode as product feature

Unify current primitives into one product model:

- Running
- Paused/Sleeping
- Cold
- Restoring
- Stopped
- Destroyed

`mvm` owns local save/restore mechanics and should expose them consistently through CLI and SDK surfaces.

### W5 — Secrets and network decision

Secure sandbox parity expects a strong secrets story. `mvm` must choose one of:

1. Implement opaque-token substitution with a hostile-guest threat model, destination-bound grants, redaction, and L7 enforcement.
2. Explicitly state that guest-visible secret materialization is a deliberate non-goal and compete on signed plans/audit/Nix isolation instead.

Given the security-first positioning, default path should be option 1 unless the threat-model review rejects it.

### W6 — Examples and tutorials

Examples must prove product shape:

- Python runtime SDK: create, exec, files, cold, stop.
- TypeScript runtime SDK: same fixture.
- Python decorator: declare, build, run.
- TypeScript decorator: declare, build, run.
- LLM tool sandbox with deny-by-default egress.
- Browser automation with explicit credential/state handling.
- OCI compatibility example with digest pin.
- Nix-first example with builder VM and signed plan.

## Priority order

1. Reconcile the existing Python/TypeScript `Sandbox` SDK method names and return values with the target lifecycle expectations.
2. Add missing SDK wrappers for file read/list/remove, command result capture/streaming, logs, ports, snapshot/cold/resume, and detach/destroy.
3. Tighten the shared Rust recording/lowering contract and live transport tests around those wrappers.
4. Decorator SDK docs/tests tightened around static compile and declarative authoring.
5. Secret reference implementation decision and first provider path.
6. Product examples across Python and TypeScript.

## Non-goals and cautions

- Do not add SSH as a default guest control path just because other products expose one. `mvm`'s posture is vsock/control-plane-first.
- Do not weaken signed plans or audit to chase SDK brevity.
- Do not present Windows as shipped; track [mvm#428](https://github.com/tinylabscom/mvm/issues/428).
- Do not present future runtime SDK methods as shipped until tests prove them; distinguish current `commands.start` / `files.write` from planned convenience aliases.
- Do not claim universal OCI compatibility; production OCI input must be policy-bound and digest-pinned.

## Exit criteria

- A Python app can create a sandbox, run code, transfer files, snapshot/cold it, and stop it through the SDK.
- A TypeScript app can do the same.
- A decorator example can build a secure Nix microVM workload without importing user code during static compile.
- Product examples pass in CI or are explicitly labeled planned.
- Security claim lint and docs build pass.
