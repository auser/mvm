# Plan 93 Phase 0 — fingerprint correctness fix (PR-A)

Self-contained kickoff prompt for a fresh Claude Code session.
Load this file's contents into the new session — the session
should be able to execute Phase 0 end-to-end without further
clarification.

---

## Goal

Land [Plan 93](../plans/93-fast-secure-dev-path-followups.md)
Phase 0 / PR-A: the fingerprint correctness fix for
`builder_vm_source_fingerprint` in
`crates/mvm-cli/src/commands/env/apple_container.rs`. This is
the smallest, most independent piece of Plan 93. It closes a
*shipping* security gap: any cached builder VM keyed by the old
fingerprint can be silently stale when contributors edit
`crates/mvm-builder-init/src/*.rs` or
`crates/mvm-egress-proxy/src/*.rs`.

Phase 0 is independent of Plan 91 (Alpine Stage 0) and the
Apple Virtualization work — even if those have shifted the
surrounding code substantially, the fingerprint correctness
gap remains and the fix is mechanical.

## Read first

1. [`specs/plans/93-fast-secure-dev-path-followups.md`](../plans/93-fast-secure-dev-path-followups.md)
   — the whole doc. The ship checklists at the bottom of each
   phase are the source of truth.
2. `AGENTS.md` (top section) and `CLAUDE.md` — repo invariants
   apply.
3. Memory entries that bear on this work, in priority order:
   - `feedback_check_inflight_work_before_diagnosing.md`
   - `feedback_always_use_git_worktrees.md`
   - `feedback_no_backcompat_first_version.md`
   - `feedback_plans_use_checkbox_format.md`
   - `project_stage0_audit_and_cache_prune_contract.md`

## Pre-flight before any code

- `git worktree list && gh pr list --state open --search 'plan
  93 OR fingerprint OR builder_vm_source_fingerprint'` — confirm
  no parallel session is already on Phase 0.
- Verify Plan 91 (#417, Alpine Stage 0) merged:
  `gh pr view 417`. If it did, the surrounding code around
  `builder_vm_source_fingerprint` has likely shifted — re-read
  the actual function before trusting any specific line
  references in the Plan 93 doc.
- Verify the Apple Virtualization in-flight work merged.
  Skim `git log origin/main --oneline -20` for context.
- Grep for the function: `grep -rn 'fn
  builder_vm_source_fingerprint' crates/`. If it has been
  renamed or moved, follow the rename and update the plan doc
  in the same commit.

## What to do

Execute the "Ship checklist (Phase 0 / PR-A)" in
[`specs/plans/93-fast-secure-dev-path-followups.md`](../plans/93-fast-secure-dev-path-followups.md):

### Code changes

- [ ] Extend `builder_vm_source_fingerprint` to include
      workspace `Cargo.lock` +
      `crates/mvm-builder-init/{Cargo.toml,src/**}` +
      `crates/mvm-egress-proxy/{Cargo.toml,src/**}` via a
      deterministic sorted walk. Skip `.DS_Store`, `*.swp`,
      `target/`.
- [ ] Add `flavor=current` field to `stage0_boot` /
      `stage0_cache_promoted` audit detail strings. Forward-
      compat field; ~10 lines.

### Tests

- [ ] Editing `crates/mvm-builder-init/src/foo.rs` changes the
      fingerprint.
- [ ] Editing `crates/mvm-builder-init/README.md` (or any
      non-`src` file) does NOT change the fingerprint.
- [ ] Deterministic walk: same fingerprint twice in a row.

### Verification

- [ ] `cargo test -p mvm-cli --lib --
      builder_vm_bootstrap_tests` passes.
- [ ] `cargo test --workspace` — 0 failures.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

Tick the boxes in
[`specs/plans/93-fast-secure-dev-path-followups.md`](../plans/93-fast-secure-dev-path-followups.md)
as items land. Commit checkbox flips together with the code
change in the same PR.

## Conventions

- **Worktree**: `../.worktrees/mvm-plan93-phase0-fingerprint/`,
  branch `feat/plan93-phase0-fingerprint`. All `git` commands
  from the main checkout with `-C <worktree>`.
- `cargo test` / `cargo clippy` on the macOS host (not in the
  builder VM).
- **No backcompat**: the widened fingerprint will invalidate
  existing builder-vm caches on first run after this PR. That's
  intended; do not add migration logic.
  (`feedback_no_backcompat_first_version.md`.)
- **PR title**: `feat(mvm-cli): plan 93 Phase 0 — fingerprint
  correctness fix`.

## Don't (in this session)

- Don't start Phase 1 / 2 / 3 of Plan 93. They're bigger, may
  need design refinement based on what landed since Plan 93 was
  drafted, and need an explicit user signal first.
- Don't try to e2e-verify on a real `mvmctl dev up`. Unit tests
  cover the contract; the 10-30 min wall-clock for a real
  Stage 0 isn't worth it for this PR.
- Don't claim a different plan slot or renumber anything per
  `project_spec_numbering_chaos.md`.

## Done criteria

When done, summarize in the final turn:

- PR link.
- Count of checkboxes ticked in Plan 93's Phase 0 ship checklist
  (target: 8/8).
- The three `cargo` gate results from §Verification.
- Any in-flight conflicts detected during pre-flight, and how
  they were resolved.
