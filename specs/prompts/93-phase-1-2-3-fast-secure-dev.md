# Plan 93 Phases 1 + 2 + 3 — fast + secure dev path kickoff

Self-contained kickoff prompt for a fresh Claude Code session.
Load this file's contents into the new session — the session
should be able to execute the planning + first-PR shape of the
remaining Plan 93 work without further clarification.

---

## Goal

Land the remaining work of
[Plan 93](../plans/93-fast-secure-dev-path-followups.md) —
Phases 1 (fast Layer 2 dev cycles), 2 (sub-200 ms runtime
microvm launch), and 3 (DX polish) — in that priority order.
Phase 0 (fingerprint correctness fix, PR-A) shipped in PR #504
on 2026-05-29.

These phases are *not* mechanical: Phase 1's host cross-compile
lever has an open design question (cross-sysroot provenance);
Phase 2 has a hard gate (benchmark harness must land before any
optimisation); Phase 3 is distributed across the other two.
**Plan before code.** Expect the first session to produce a
planning PR and at most one tracer-bullet implementation PR,
not the full phase.

## Read first

In this order:

1. [`specs/plans/93-fast-secure-dev-path-followups.md`](../plans/93-fast-secure-dev-path-followups.md)
   — the whole doc. Pay particular attention to:
   - Phase 1 §"Levers" lever 2's open question on cross sysroot
     provenance.
   - Phase 2 §"Ship checklist" — benchmark harness gates every
     other item.
   - Phase 3 §"Ship checklist" — each item rides alongside
     Phase 1 or 2 work, not standalone.
   - §"Reproducibility (Phase 1 lever 2)" — sets the deterministic-
     build contract for the cross-compile path.
   - §"Success criteria" — the user-facing targets every PR
     should be measurable against.
2. [`specs/plans/91-stage0-alpine-bootstrap.md`](../plans/91-stage0-alpine-bootstrap.md)
   — Plan 91 is the precedent for the trust model Phase 1 lever 2
   reuses (hash-pinned upstream fetch). Confirm what actually
   shipped vs. what the plan proposes; Plan 91 was in flight
   when Plan 93 was drafted (PR #417 merged 2026-05-20).
3. [`specs/plans/92-stock-kernel-builder-vm.md`](../plans/92-stock-kernel-builder-vm.md)
   and any Plan 95 / slim-kernel work — Phase 2 lever 1 depends
   on the cmdline contract that lands there.
4. [`specs/adrs/046-source-checkout-builds.md`](../adrs/046-source-checkout-builds.md)
   — §"Two artifact layers, two acquisition paths" + §"Why the
   contributor path doesn't download". Every fetched-byte
   proposal in Phase 1 / 2 / 3 must reconcile with this ADR.
5. `AGENTS.md` (top section) and `CLAUDE.md` — repo invariants
   apply. Note especially:
   - "Host Nix is never used by mvmctl, even when present."
   - "Source-checkout builds never depend on mvm-published
     artifacts."
6. Memory entries, in priority order:
   - `feedback_check_inflight_work_before_diagnosing.md`
   - `feedback_always_use_git_worktrees.md`
   - `feedback_no_backcompat_first_version.md`
   - `feedback_no_prebuilt_builder_vm_artifact.md`
   - `feedback_no_external_cache_providers.md`
   - `feedback_replace_over_workaround.md`
   - `feedback_dev_vm_vs_prod_security_tiers.md`
   - `feedback_track_deferred_work_in_specs_plans.md`
   - `feedback_plans_use_checkbox_format.md`
   - `project_spec_numbering_chaos.md`

## Pre-flight before any code

- `git worktree list && gh pr list --state open --search 'plan
  93 OR plan 91 OR plan 92 OR plan 95 OR cross-compile OR
  warm-pool OR bench microvm'` — confirm no parallel session is
  already on these items.
- `gh pr view 504` — confirm Phase 0 landed and that
  `builder_vm_source_fingerprint` is the post-PR-A shape. Phase
  1 lever 2's reproducibility CI lane depends on this binding.
- Verify recently-merged work that may have shifted the surface:
  Plan 91 (#417), Plan 92/95 slim kernel, any Apple
  Virtualization (Vz) work, and any other mid-flight `worktree-*`
  branches under `git worktree list`. Skim
  `git log origin/main --oneline -30` for context.
- Grep the load-bearing surfaces before trusting line references
  in the Plan 93 doc:
  - `rg 'fn start' crates/mvm-backend/src/libkrun.rs`
  - `rg 'fn builder_vm_source_fingerprint'
    crates/mvm-cli/src/commands/env/apple_container.rs`
  - `rg 'dev-shell|dev-compile|dev-minimal' nix/images/`
  - `rg 'mvmctl bench|microvm-launch' crates/ specs/`

## What to do

**Plan first, code second.** The first PR out of this session
is a planning PR (or a planning commit on the implementation
PR), not Phase-1-complete.

### Step 1 — design refinement (~half a day)

Re-read Plan 93's Phase 1 / 2 / 3 ship checklists against what
actually shipped in Plan 91 and Plan 92/95. Update Plan 93 in
place where the world has moved (mark items already covered,
drop items the new substrate makes irrelevant, add items the
new substrate creates). Commit the plan update first so the
implementation PRs reference a current document.

Open design questions to resolve in this step:

- **Phase 1 lever 2 — cross sysroot provenance.** The plan
  leans toward a hash-pinned upstream sysroot fetched lazily,
  same trust shape as Plan 91's Alpine pin. Confirm this still
  reconciles with ADR-046 §"Why the contributor path doesn't
  download" — Plan 91 carved out a precedent; Phase 1 reuses
  it. Write up the proposed sysroot fetcher contract (URL,
  hash manifest location, verification path, audit-emit kind)
  as an ADR draft or an §Open-questions amendment to Plan 93
  before any code lands.
- **Phase 2 benchmark harness shape.** Decide whether
  `mvmctl bench microvm-launch` is a new top-level verb, an
  `--bench` flag on `mvmctl up`, or an xtask. Decide what gets
  measured (wall-clock from libkrun_create_ctx through guest
  agent ack) and how results persist for regression tracking.
  This decision is load-bearing for the rest of Phase 2.
- **Phase 1 levers 1 + 3 sequencing.** Lever 2 is the
  load-bearing one for "no LONG dev cycles." Lever 1 (split
  dev shell) and lever 3 (lazy Nix fetch inside dev-compile)
  are cleanup. Decide whether they ship before or after
  lever 2 — they're not gating but they may simplify lever 2's
  trust model if landed first.

### Step 2 — tracer-bullet PR (~1-2 days)

Pick the smallest end-to-end vertical slice that exercises the
load-bearing decision from step 1 and ship it as a single PR.
Candidates, in rough order of value:

1. **Phase 2 benchmark harness skeleton.** Cheapest item that
   unblocks Phase 2. Even an empty `mvmctl bench microvm-launch`
   that prints a single wall-clock number, with no
   optimisations, lets every subsequent Phase 2 PR measure
   progress. Recommended first PR.
2. **Phase 1 lever 1 (split dev shell).** Mechanical Nix flake
   restructure with no cross-compile complexity. Good "I want
   to feel the dev loop today" win.
3. **Phase 3 — `LocalAuditKind::VendorBlobFetched` audit
   kind.** Forward-compat with both Plan 91's Alpine fetch
   (already shipped) and Phase 1 lever 2's pinned sysroot.
   Small, independent, valuable.

Do *not* try to ship Phase 1 lever 2 (host cross-compile +
bind-mount) in the first PR — too many unresolved design
questions; needs its own brainstorm + ADR pass.

### Step 3 — subsequent PRs

After step 2 ships, recommended sequence:

1. Phase 2 lever 1 — kernel cmdline trim + initrd elimination
   (small, measurable via the harness from step 2).
2. Phase 1 lever 2 — host cross-compile + bind-mount, gated on
   the sysroot ADR from step 1.
3. Phase 1 lever 2c — reproducibility CI lane.
4. Phase 2 lever 2 — guest agent startup parallelism.
5. Phase 3 — `cache info` / `doctor` enrichment, public docs.
6. Phase 2 lever 3 — warm pool.
7. Phase 2 lever 4 — VMM ballooning.

Tick the boxes in
[`specs/plans/93-fast-secure-dev-path-followups.md`](../plans/93-fast-secure-dev-path-followups.md)
as items land. Commit checkbox flips together with the code
change in the same PR (per
`feedback_plans_use_checkbox_format.md`).

## Conventions

- **Worktree per PR**: each implementation PR lives in its own
  worktree under `../.worktrees/mvm-plan93-<phase>-<slug>/`
  with branch `feat/plan93-<phase>-<slug>`. Examples:
  - `feat/plan93-phase2-bench-harness`
  - `feat/plan93-phase1-split-dev-shell`
  - `feat/plan93-phase3-vendor-blob-audit-kind`
- `cargo test` / `cargo clippy` / `cargo fmt --all -- --check`
  on the macOS host (not in the builder VM).
- **No backcompat**: caches blown away on upgrade is the design
  posture (`feedback_no_backcompat_first_version.md`). Do not
  add `mvm-v1-cache-recogniser` style migration code.
- **No external build-cache providers** (Cachix, attic, etc.)
  per `feedback_no_external_cache_providers.md`.
- **No prebuilt builder VM artifact** per
  `feedback_no_prebuilt_builder_vm_artifact.md` — Phase 1's
  cross-sysroot fetch is the *only* new download surface this
  plan introduces, and it follows Plan 91's already-established
  Alpine pin pattern.
- **PR title prefix**: `feat(<crate>): plan 93 Phase <N> — <slug>`
  or `docs(plans): plan 93 — <slug>` for plan updates.

## Don't (in this session)

- Don't attempt all three phases in one PR. The first session's
  output is one planning PR + at most one tracer-bullet PR.
- Don't ship Phase 1 lever 2 (host cross-compile + bind-mount)
  without the cross-sysroot ADR. The fetched-byte surface is a
  supply-chain decision, not an implementation detail.
- Don't add optimisation PRs to Phase 2 before the benchmark
  harness lands. Per the plan: "Without measurement we'll
  optimise the wrong thing."
- Don't paint into a corner that blocks the egress-secret
  detection feature (`project_egress_secret_detection_is_core.md`).
  Phase 2 lever 2's guest-agent + vsock work touches adjacent
  territory.
- Don't claim a plan slot or renumber anything per
  `project_spec_numbering_chaos.md`.
- Don't propose host-Nix-store mirrors or any multi-GB
  contributor download — explicitly out of bounds per Plan 93
  §"What this does NOT do".

## Done criteria

When done, summarize in the final turn:

- Plan update PR link (the design-refinement PR from step 1, if
  it shipped as a standalone PR).
- Tracer-bullet PR link (the step 2 PR, if it shipped).
- Open ADR / design draft links.
- Count of Plan 93 Phase 1 / 2 / 3 ship-checklist boxes ticked
  this session (expected: 1-3 across all phases; the first
  session is mostly setup).
- The three `cargo` gate results from the tracer-bullet PR's
  verification section.
- Any in-flight conflicts detected during pre-flight, and how
  they were resolved.
- A one-sentence recommendation for the next session's first
  move, based on what shipped here.
