# mvm Sprint 7: Role-Based NixOS Profiles & Reconcile Ordering

Previous sprints:
- [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete)
- [SPRINT-2-production-readiness.md](sprints/SPRINT-2-production-readiness.md) (complete)
- [SPRINT-3-real-world-validation.md](sprints/SPRINT-3-real-world-validation.md) (complete)
- Sprint 4: Security Baseline 90% (complete)
- Sprint 5: Final Security Hardening (complete)
- [SPRINT-6-minimum-runtime.md](sprints/SPRINT-6-minimum-runtime.md) (complete)

Sprint 7 implements role-based NixOS microVM profiles and reconcile ordering. Based on [specs/plans/8-configuration-and-isolation.md](plans/8-configuration-and-isolation.md).

The Role enum, RuntimePolicy, instance timestamps, config drive, vsock, and sleep eligibility were implemented in Sprint 6. This sprint adds the NixOS module system, build integration, CLI support, and reconcile ordering.

---

## Phase 1: pool_create with Role Parameter
**Status: NOT STARTED**

Pass the role through from CLI and reconcile into pool_create().

- [ ] `src/vm/pool/lifecycle.rs` — add `role: Role` parameter to `pool_create()`
- [ ] `src/agent.rs` — pass `dp.role.clone()` to `pool_create()` in reconcile
- [ ] Tests: pool_create with explicit role, verify persisted in pool.json

## Phase 2: CLI --role Argument
**Status: NOT STARTED**

Add `--role` to `mvm pool create`.

- [ ] `src/main.rs` — add `--role` arg to `PoolCmd::Create` (default: "worker")
- [ ] `src/main.rs` — parse string to `Role` enum, pass to `pool_create()`
- [ ] `src/infra/display.rs` — add `role` to PoolRow and PoolInfo display structs

## Phase 3: Reconcile Ordering (Gateway Before Worker)
**Status: NOT STARTED**

Ensure gateway pools are reconciled before worker pools within each tenant.

- [ ] `src/agent.rs` — `role_priority()` function: Gateway=0, Builder=1, Worker=2, CapabilityImessage=3
- [ ] `src/agent.rs` — Phase 2-3: sort `dt.pools` by role_priority before iteration
- [ ] `src/agent.rs` — Phase 6: reverse sort for sleep (workers sleep before gateways)
- [ ] Tests: verify ordering with mixed-role pools

## Phase 4: NixOS Manifest Parser (mvm-profiles.toml)
**Status: NOT STARTED**

Config-file-driven Nix build: manifest maps (role, profile) → .nix module paths.

- [ ] `src/vm/pool/nix_manifest.rs` — NixManifest, ProfileEntry, RoleEntry structs
- [ ] `src/vm/pool/nix_manifest.rs` — load(), resolve(), role_requirements()
- [ ] `src/vm/pool/mod.rs` — add `pub mod nix_manifest;`
- [ ] Tests: TOML parse roundtrip, resolve valid/invalid, role_requirements

## Phase 5: Build Integration
**Status: NOT STARTED**

Update nix build to use manifest-driven role+profile attribute resolution.

- [ ] `src/vm/pool/build.rs` — try loading mvm-profiles.toml from flake_ref
- [ ] `src/vm/pool/build.rs` — if found: `tenant-<role>-<profile>`, else fallback to `tenant-<profile>`
- [ ] Tests: build attribute construction with and without manifest

## Phase 6: Nix Role Modules
**Status: NOT STARTED**

Create NixOS role modules and update flake for role+profile combinations.

- [ ] `nix/roles/gateway.nix` — gateway service, hostname, config drive consumption
- [ ] `nix/roles/worker.nix` — worker agent service
- [ ] `nix/roles/builder.nix` — builder capabilities (from existing nix-builder)
- [ ] `nix/roles/capability-imessage.nix` — placeholder
- [ ] `nix/mvm-profiles.toml` — reference manifest mapping roles+profiles to modules
- [ ] `nix/flake.nix` — mkGuest with roleModules, combined tenant-role-profile outputs
- [ ] Backward compat: keep legacy `tenant-minimal`, `tenant-python` outputs

## Phase 7: Documentation
**Status: NOT STARTED**

- [ ] `docs/roles.md` — role semantics, drive model, reconcile ordering, NixOS modules
- [ ] `specs/SPRINT.md` — update with final metrics

---

## Summary

| Metric | Value |
|--------|-------|
| Lib tests | TBD |
| Integration tests | TBD |
| Total tests | TBD |
| Clippy warnings | 0 |

## Files to Create/Modify

| File | Changes |
|------|---------|
| `src/vm/pool/nix_manifest.rs` | **NEW** — TOML manifest parser |
| `src/vm/pool/mod.rs` | Add `pub mod nix_manifest` |
| `src/vm/pool/lifecycle.rs` | `pool_create()` gains `role` param |
| `src/vm/pool/build.rs` | Manifest-driven build attribute |
| `src/agent.rs` | Role ordering in reconcile, pass role to pool_create |
| `src/main.rs` | `--role` CLI arg for pool create |
| `src/infra/display.rs` | Role in pool display structs |
| `nix/roles/gateway.nix` | **NEW** — gateway role module |
| `nix/roles/worker.nix` | **NEW** — worker role module |
| `nix/roles/builder.nix` | **NEW** — builder role module |
| `nix/roles/capability-imessage.nix` | **NEW** — placeholder |
| `nix/mvm-profiles.toml` | **NEW** — reference manifest |
| `nix/flake.nix` | roleModules param, combined outputs |
| `docs/roles.md` | **NEW** — role documentation |
