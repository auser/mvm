# Plan 127 — Metering / observability + boot/size benchmark harness

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the metering surface (ADR-040 axes; mvm emits, mvmd aggregates), build a per-phase boot-latency harness with a real methodology (incl. the measured macOS code-signing cold-start lever), and stand up a perf/size budget dashboard (boot, build cold/warm, image size, binary size, dep-count) that **flags regressions without failing the build** (hosts differ — ADR-066 §7).

**Architecture:** Extend, don't reinvent. `mvm-core/src/metering.rs` (Plan 46) already has `MeteringSample`/`MeteringBucket`, the three axes (CPU/memory/storage), the `~/.mvm/metering/<tenant>/<date>.jsonl` rollup, and the `MeteringEpoch` audit chaining; this adds the ADR-040 egress + build-minutes axes and the emit points. `crates/mvm-cli/src/commands/ops/bench.rs` (the PR-#517 probe) is the harness base; this adds per-phase decomposition. The dep-count comes from 126's `docs/investigations/dep-baseline.md`. **Aggregation stays mvmd's** — mvm emits samples, the control plane rolls them up and bills.

**Tech Stack:** Rust (`mvm-core::metering`, `mvm-cli::ops::bench`), `tracing` (structured spans), the existing audit chain. No new third-party crates.

**Prereqs:** 126 (the re-baselined dep methodology + `dep-baseline.md`), 123 (per-backend boot capability), 120 (a green boot path to measure).

---

## Phase A — metering (ADR-040 axes; mvm emits, mvmd aggregates)

### Task A1: add the egress + build-minutes axes

`metering.rs` meters CPU/memory/storage. ADR-040 also bills **egress bytes** and **build-minutes**.

**Files:** `crates/mvm-core/src/metering.rs`.

- [ ] **Step 1:** Failing test — a `MeteringSample` carries `egress_bytes` + `build_minutes` alongside the existing axes; serde round-trip; a bucket sums them per minute.
- [ ] **Step 2:** Extend the sample/bucket types (keep `deny_unknown_fields`); the `MeteringEpoch` audit chaining covers the new fields. Commit.

### Task A2: wire the emit points

- [ ] **Step 1:** Failing tests — a VM's lifecycle emits CPU/mem/storage samples on a tick; the egress proxy (123) emits `egress_bytes`; a build emits `build_minutes`. Each lands in the per-tenant rollup.
- [ ] **Step 2:** Add the emit calls at the lifecycle/proxy/build points (the data already flows through those paths). **Aggregation is out of scope — mvmd rolls up the rollup files** (note it, don't build it here). Commit.

## Phase B — boot-latency harness (per-phase methodology)

### Task B1: decompose boot into phases

ADR-066 §7 wants a *methodology*, not a single number. Phases: `spawn` (process fork/exec — where the macOS code-signing penalty lives) → `kernel` (kernel boot to init) → `init` (PID-1 to agent fork) → `agent_ready` (vsock `Ping` answered, the 120 marker).

**Files:** `crates/mvm-cli/src/commands/ops/bench.rs`; a `boot report` surface (the `BootReport` verb).

- [ ] **Step 1:** Failing test — `bench` records a `BootProfile { spawn, kernel, init, agent_ready }` (monotonic deltas) for a boot; the sum equals wall-clock within tolerance. Use the console-log timestamps + the `wait_for_guest_agent` return as the phase boundaries.
- [ ] **Step 2:** Implement the per-phase capture; emit the profile as JSON + a human table. Commit.

### Task B2: cold + warm, per backend

- [ ] **Step 1:** `bench --backend <…> --cold|--warm` runs N iterations, reports p50/p95 per phase. Cold = fresh boot; warm = from a 123 snapshot (where the backend supports it — `DiskOnly`/`SaveRestore`/`LiveMemory`). Gated on a real VM (`MVM_E2E_SMOKE`).
- [ ] **Step 2:** Implement; the warm path uses 123's `snapshot_capability` to pick the resume mode (skip warm where `Unsupported`, don't fake it). Commit.

### Task B3: the macOS code-signing lever

ADR-066 §7 / §"survey" — the measured macOS per-page code-signing penalty in the `spawn` phase, and its warm-inode / cache-prewarm mitigation.

- [ ] **Step 1:** Measure the `spawn` phase cold (first exec, pages unsigned in cache) vs warm (re-exec, signatures cached); record the delta in a `docs/investigations/` note.
- [ ] **Step 2:** Implement the mitigation (prewarm the binary's pages / keep the inode warm across runs); show the before/after in the note. macOS-only; a no-op elsewhere. Commit.

## Phase C — perf/size budget dashboard

### Task C1: the budget metrics + regression flagging

Boot (cold/warm per backend, from B), build (cold/warm), image size (the slim `mkGuest` rootfs), binary size (`mvmctl` + the prod agent), dep-count (from 126's `dep-baseline.md`).

**Files:** `xtask budget` (or extend `bench`); a committed `docs/budgets.json` baseline.

- [ ] **Step 1:** Failing test — `xtask budget` collects the metrics into a `Budget` struct, compares against `docs/budgets.json`, and **prints regressions as warnings, exit 0** (a regression flags, does not fail — hosts differ; ADR-066 §7). A `--update` rebaselines.
- [ ] **Step 2:** Implement the collection + the diff; wire it as an **informational** CI job (not a required check — 128 owns the required gates). Commit.

## Phase D — structured tracing

### Task D1: one tracing strategy

- [ ] **Step 1:** Failing test — the host path emits structured `tracing` spans for the boot phases + the admit/launch steps (claim-8 lineage), with a stable field schema; `RUST_LOG`/`--verbose` controls them; no secret values in any span (ties to the 129 no-secret-bytes invariant).
- [ ] **Step 2:** Add the spans at the lifecycle/admit/build points; assert the no-secret-in-spans property in a test. Commit.

## Acceptance

- [ ] `MeteringSample` carries all five ADR-040 axes (CPU/mem/storage/egress/build-minutes); emit points fire on lifecycle/egress/build; the per-tenant rollup + `MeteringEpoch` audit chaining cover them. Aggregation is explicitly mvmd's.
- [ ] `bench` reports a per-phase `BootProfile` (spawn/kernel/init/agent_ready), cold + warm, p50/p95 per backend, picking the warm mode from 123's `snapshot_capability`.
- [ ] The macOS code-signing cold-start delta is measured + mitigated, with the numbers in `docs/investigations/`.
- [ ] `xtask budget` flags boot/build/size/dep-count regressions as **warnings (exit 0)**, baselined in `docs/budgets.json`; runs as an informational CI job.
- [ ] Structured `tracing` spans on the host path, no secret values; `cargo test --workspace` + clippy + fmt green; no new dependency.

### deferred follow-ups

- [ ] The mvmd aggregation/billing rollup (mvmd plan) — consumes the rollup files this plan writes.
- [ ] Promote a budget metric to a *required* gate only if it proves stable across hosts (128's call).

## Self-review

- **Spec coverage (brief 127):** ADR-040 metering with aggregation deferred (Phase A), per-phase boot-latency methodology + the macOS code-signing lever (Phase B), the re-baselined dep-count + perf/size budget dashboard (Phase C), structured tracing (Phase D). All present.
- **Grounding:** extends the real `metering.rs` (Plan 46) + `ops/bench.rs` (PR #517), reads 126's `dep-baseline.md`; the boot phases use the real boundaries (console-log timestamps, `wait_for_guest_agent`).
- **Honesty:** the budget dashboard **flags, doesn't fail** (ADR-066 §7 — hosts differ); warm-boot skips backends that can't do it rather than faking; every number is measured + written to `docs/`.
- **Voice:** comments mark the non-obvious (why the budget is a warning not a gate, why warm-boot mode comes from the backend capability, why spans must exclude secrets), not the calls.
