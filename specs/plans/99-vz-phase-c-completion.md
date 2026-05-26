# Plan 99 — Vz backend Phase C completion (audit/cache + kernel + acceptance)

**Status:** drafted 2026-05-25.
**Closes:** the two unchecked items in `specs/plans/97-vz-backend.md`
under "Phase C sub-tasks" — Stage 0 audit emit + cache-prune contract
participation (line 172) and Phase C acceptance (lines 175–181).
**Depends on:** plan 92's slim builder-VM kernel work currently on
`worktree-plan-92-stock-kernel` (commits `e663abf4`, `fd04817c`) being
validated and merged to main.
**Sibling track:** plan 93 (Alpine steady-state builder VM) and plan 95
(kernel slimming followup) — neither blocks this plan; this plan
consumes whatever kernel ships through plan 92.

## Problem

Plan 97 shipped the Vz workload-runtime path end-to-end on macOS 13+
(`MVM_BACKEND=vz mvmctl up` admits, boots, snapshots round-trip), but
Phase C — Vz as a *builder-VM* backend — has two unchecked items
preventing the checkbox from flipping green:

1. **Audit/cache contract participation** is unverified. The orphan
   reaper at `reap_orphaned_vm_helpers_at`
   (`crates/mvm-cli/src/commands/env/apple_container.rs:3292`) already
   iterates `~/.cache/mvm/builder-vm/vms/` prefix-agnostically and
   `VzBuilderVm` writes `builder.pid` (`crates/mvm-build/src/vz_builder.rs:258`),
   so the contract works implicitly. But nothing pins this — a future
   refactor that narrows the dir traversal to a libkrun prefix, or
   renames the PID sidecar, would silently break Vz cleanup with no
   CI signal.
2. **Phase C acceptance is blocked on the kernel.** The libkrun-built
   builder VM image's kernel won't direct-boot via `VZLinuxBootLoader`,
   so `MVM_BUILDER_BACKEND=vz mvmctl build --flake .` can't produce a
   rootfs to compare against the libkrun-hosted output. The slim-kernel
   work on `worktree-plan-92-stock-kernel` makes the builder kernel
   hypervisor-agnostic by construction — once plan 92 lands, Vz
   inherits a bootable image.

## Decision

Ship Phase C completion as two stacked PRs:

- **PR-1** — pin the audit/cache contract. Single reaper test +
  doc-comment update + plan-97 checkbox flip for the audit/cache item
  only. Lands first, independent of any kernel work.
- **PR-2** — land plan 92's slim kernel, smoke under Vz on a real
  macOS Apple Silicon host, flip Phase C acceptance, add the
  `CONFIG_VIRTIO_PCI=y` regression test. Branched off PR-1's merge
  commit.

The split exists because PR-2 needs **live macOS host validation** that
no CI lane covers (GHA-hosted macOS doesn't expose Hypervisor.framework
to user processes — see plan 97 line 253–255), so its merge cadence is
bound to a human's `dev up` session, whereas PR-1 is fully
CI-verifiable.

## PR-1 — audit/cache contract pin

### Code

- [ ] `crates/mvm-cli/src/commands/env/apple_container.rs` — update the
      doc-comment on `reap_orphaned_vm_helpers` (around line 3250–3283)
      to name both supervisors (`mvm-libkrun-supervisor` and
      `mvm-vz-supervisor`) and to call out that the dir traversal is
      prefix-agnostic so both `mvm-builder-vm-*` and `mvm-builder-vz-*`
      state dirs are covered.
- [ ] `crates/mvm-cli/src/commands/env/apple_container.rs` — add one
      unit test `reap_picks_up_orphaned_vz_builder_state_dir` alongside
      the existing `sweep_*` / `reap_*` tests (around lines
      5317–5421). Pattern mirrors `sweep_removes_orphan_staging_dirs`:
      tempdir-isolated, creates `mvm-builder-vz-<job_id>/builder.pid`
      with `i32::MAX`, invokes `reap_orphaned_vm_helpers_at`, asserts
      `outcome.removed_dirs == 1` and the dir is gone.

### Specs

- [ ] `specs/plans/97-vz-backend.md` line 172 — flip checkbox `[ ]` →
      `[x]`; append one-sentence implementation log entry pointing at
      the new test as the green pin and noting Stage 0 audit emits +
      the `stage0.lock` already cover both backends because Stage 0
      itself runs upstream of backend dispatch.
- [ ] `specs/plans/97-vz-backend.md` top-of-file status banner (lines
      3–66) — reflect that Phase C now has only the kernel-direct-boot
      blocker left.

### Verification

- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo clippy --workspace -- -D warnings` zero warnings.
- [ ] `cargo test --workspace` passes, new test included.
- [ ] `cargo test -p mvm-cli reap_picks_up_orphaned_vz_builder_state_dir`
      passes in isolation.

### Out of scope for PR-1

- Stage 0 sweep coverage of vz-prefixed staging dirs — Vz doesn't
  create those.
- `mvm-vz-supervisor` lifecycle accounting beyond `builder.pid`.
- Any change to ADR-056 — the per-backend reaper coverage is not
  ADR-worthy; a doc-comment + checklist flip suffice.

## PR-2 — slim kernel + Vz acceptance

Branches off PR-1's merge commit. **Requires a live macOS Apple Silicon
host with Hypervisor.framework available** for steps marked `(local)`.

### Land plan 92 (slim kernel)

- [ ] Carry forward plan 92's two commits (`e663abf4`, `fd04817c`)
      from `worktree-plan-92-stock-kernel` onto the new branch.
- [ ] (local) `cargo run -- dev up` end-to-end with the slim kernel
      (plan 92's remaining item line 184–188). Boot the steady-state
      builder VM to the same checkpoint the stock-kernel intermediate
      direction reached.
- [ ] If the kernel panics on a missing symbol, add it to `enables`
      at `nix/images/builder-vm/kernel/default.nix:42–84` and rebuild.
      Iterative one-line fixes (plan 92 line 189–192).
- [ ] Apply plan 92's "Doc sweeps" (lines 201–217) — drop the TSI
      qualifier from the four doc-comment cites; decide on
      `extract_bundled_kernel()` (probably delete, ~80 lines).

### Vz smoke + acceptance

- [ ] (local) `MVM_BUILDER_BACKEND=vz mvmctl build --flake .` on macOS
      Apple Silicon. Confirm the slim kernel boots under
      `VZLinuxBootLoader` and produces a rootfs.
- [ ] (local) Byte-identity check: build the same flake once via
      libkrun, once via Vz; confirm `sha256sum` of the resulting
      `rootfs.ext4` matches. If it doesn't, root-cause before flipping
      the acceptance checkbox.
- [ ] Add `crates/mvm-build/tests/builder_vm_kernel_config.rs` (or
      equivalent) that builds `nix/images/builder-vm/.#kernel-configfile`
      and asserts `CONFIG_VIRTIO_PCI=y` + `CONFIG_PCI=y` appear in the
      generated `.config`. Guards against a future slimming pass that
      silently breaks Vz.

### Specs

- [ ] `specs/plans/97-vz-backend.md` line 175–181 — flip Phase C
      acceptance checkbox `[ ]` → `[x]`; append implementation-log
      entry citing the local smoke + byte-identity check + the
      kernel-config regression test.
- [ ] `specs/plans/97-vz-backend.md` top-of-file status banner —
      reflect that Phase C is closed (only the optional
      `LibkrunBuilderVm`-seam refactor and Phase B follow-ups remain).
- [ ] `specs/adrs/056-vz-backend.md` — add a one-paragraph note that
      the slim kernel from plan 92 is the integrity-boundary shared
      across libkrun and Vz (dm-verity over the same rootfs under
      both backends).
- [ ] Move `specs/plans/97-vz-backend.md` Phase C from 🟡 → ✅ in the
      top-of-file checklist.

### Verification

- [ ] `cargo fmt --all -- --check`, clippy, full workspace tests
      clean.
- [ ] `cargo test` includes the new kernel-config test and it passes.
- [ ] `mvmctl doctor` on a macOS Apple Silicon host reports Vz
      available + supervisor present + the slim kernel image resolved
      (no regression from main).
- [ ] PR description carries screenshots / terminal pastes of the Vz
      boot + byte-identity check (CI can't validate this; the paste is
      the verification artifact).

### Out of scope for PR-2

- The `LibkrunBuilderVm` seam refactor (plan 97 §"Phase C seam design
  (recommendation)" lines 183–245). The current `VzBuilderVm` impl is
  ~350 lines because the refactor was already partly done; the full
  seam is an optimization, not a blocker.
- Phase B follow-ups: Hypervisor.framework concurrent-VM cap probe +
  `MVM_BACKEND=vz mvmctl run dev-shell` acceptance. Both deferred in
  plan 97 for orthogonal reasons.
- Performance numbers (plan 97 line 252–255) — still blocked on a CI
  runner with Hypervisor.framework.
- mvmd integration (separate repo).

## Risk register

- **R1 — plan 92 slim kernel doesn't boot under Vz on first try.**
  Mitigation: the kernel-config inspection already shows the right
  Kconfigs in `enables`; if it still panics, add the missing symbol
  to `enables` (same loop as plan 92's own remaining work).
- **R2 — rootfs byte-identity diverges between libkrun and Vz.**
  Likely cause is non-determinism in the build (timestamps,
  randomness), not a real Vz issue. Mitigation: investigate before
  flipping acceptance; document any tolerated divergence in the PR
  description.
- **R3 — plan 92 lands ahead of this plan via a different session.**
  Mitigation: PR-2 then drops the "carry forward plan 92's two
  commits" line items and starts directly from the local-smoke step.
  Cheap rebase.

## Pickup notes for future sessions

The fastest pickup for a fresh session is:

1. Read this file top to bottom.
2. Check `worktree-vz-stage0-contract` — if it still exists with no
   PR, PR-1 hasn't shipped; resume there. If PR-1 merged, jump to
   PR-2.
3. For PR-2: check plan 92's worktree state
   (`git -C .claude/worktrees/plan-92-stock-kernel log --oneline main..HEAD`)
   to see if its commits have already been merged or rebased.
4. Verify nothing in `worktree-plan-98-finish` or `worktree-phase-c-seam`
   has claimed Phase C acceptance behind our backs.
