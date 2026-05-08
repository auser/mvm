---
title: "ADR-014: `VmBackend` single trait; backend-as-impl pattern"
status: Proposed
date: 2026-05-07
related: ADR-013 (microsandbox pivot), plan 60-mvm-microsandbox-migration
---

## Status

Proposed. Implementation lands in Phase 0 (workspace reshape) and Phase 1 (Firecracker + microsandbox impls).

## Context

The current `mvm-runtime` skeleton introduces two parallel backend abstractions:

- `mvm-backend/src/backend/sandbox.rs` defines `Backend<Sandbox, Context>` with `prepare/boot/teardown` (most methods `todo!()`).
- `mvm-builder/src/builder/mod.rs` defines `BuilderBackend` with `prepare/build/extract_artifacts/cleanup` (no impls).

Meanwhile, the previous iteration at `../mvm/crates/mvm-core/src/protocol/vm_backend.rs` already defines a stable `VmBackend` trait (~700 LOC) that **mvmd's agent + hostd already depend on** through the `mvmctl` facade. mvmd's `LifecycleDispatch` enum routes either to direct trait calls (dev mode) or to `mvm-hostd` IPC (production), and the dispatch types are the `VmBackend` trait's request/response shapes.

Maintaining two parallel abstractions would require either:
1. A bridge layer translating between `Backend<S,C>` and `VmBackend` — pure overhead.
2. mvmd refactoring to consume the new trait — breaks the facade contract.

Neither is acceptable.

## Decision

1. **Delete the hand-written `Backend<S,C>` and `BuilderBackend` traits.** They were placeholder skeletons the user OK'd replacing.
2. **Adopt `mvm_core::protocol::vm_backend::VmBackend` as the single backend trait** (port verbatim from `../mvm/crates/mvm-core/src/protocol/vm_backend.rs` in Phase 0).
3. **Implementations live in their own modules**, not their own traits:
   - `mvm-runtime/src/vm/firecracker.rs` → `impl VmBackend for FirecrackerBackend`
   - `mvm-runtime/src/vm/microsandbox.rs` → `impl VmBackend for MicrosandboxBackend`
   - Future: `mvm-runtime/src/vm/cloud_hypervisor.rs` (post-Phase-10, gated by `backend-cloud-hypervisor` feature)
4. **Build vs. execution split is preserved** via a separate (existing) abstraction: `mvm_core::build_env::{ShellEnvironment, BuildEnvironment}`. `mvm-build` consumes `BuildEnvironment`; this is an orthogonal concern from `VmBackend`.
5. **Backend selection** is centralized in `mvm-cli/src/commands/mod.rs::pick_backend()`:
   - env override `MVM_BACKEND` (explicit)
   - else: KVM available + Linux → Firecracker; macOS/Windows/no-KVM → microsandbox
6. **Plug-in registration** via `inventory` crate (post-Phase-10): new backends register at startup; core code stays closed for modification but open for extension.

## Consequences

**Positive**:
- mvmd's compile gate stays unbroken — same trait shape, same paths, same wire format.
- Single source of truth for backend semantics; no bridge layer.
- New backends are a file + an `impl VmBackend`, not a new trait + bridge.

**Negative**:
- We inherit `VmBackend`'s current shape, which carries some Lima-era assumptions in argument names. We accept this for facade stability; ADR-014.1 (future) can rename if needed.
- The trait isn't `dyn`-safe today (uses `async fn` in trait via `async-trait`). Plug-in registration via `inventory` works but precludes some advanced compositions; acceptable trade-off.

**Neutral**:
- Both `Backend<S,C>` and `BuilderBackend` go away — the user's hand-written code is replaced, but no consumer uses them.

## Alternatives considered

- **Two traits, `BuildBackend` + `RunBackend`**: rejected. The previous iteration tried this and the lines blurred (snapshots, sleep policy, etc. cross both); a single richer trait + sub-namespaces is cleaner.
- **Trait objects (`Box<dyn VmBackend>`)**: deferred. Async-fn-in-trait works for static dispatch but not yet `dyn`-safe. We use enums (`BackendKind`) for dispatch today; switch to trait objects when the toolchain supports it.

## Threat model impact

None — purely a refactor of the abstraction layer. The same security operations (jailer, seccomp, dm-verity) bind to the same trait methods.

## Compliance impact

None.
