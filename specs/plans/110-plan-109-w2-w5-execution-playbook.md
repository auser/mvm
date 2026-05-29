# Plan 110 — Plan 109 W2 / W2′ / W5 execution playbook

Status: ready (drop-in for a fresh AI session)
Created: 2026-05-28
Related: Plan 109 (the parent plan), ADR-060 (pid0 portability boundary), ADR-063 (boundary language policy), Sprint 58

## What this plan is

This is **not a normal implementation plan**. It is a self-contained execution prompt for a fresh AI session picking up Plan 109's three non-blocked workstreams (W5, W2′, W2). The parent plan ([Plan 109](109-zig-pid0-exploration.md)) holds the strategy and rationale; this plan holds the executable steps with checkboxes so the user can reference the file path instead of pasting a 1500-word prompt every session.

The three workstreams covered here are intentionally the ones that do **not** require coordination with the parallel "secrets work over vsock" session (Plan 104 / ADR-059). The deferred workstreams (W3 vsock encryption design, W4a control-plane-vs-data-plane ADR, W6 ADR-002 threat-model delta) are listed in §"Anti-patterns" so the new session knows to stay out of them.

## Read first (before touching anything)

The new session MUST read these in order before making any edits. Each item resolves a specific question the session will otherwise re-derive:

- [ ] [`specs/plans/109-zig-pid0-exploration.md`](109-zig-pid0-exploration.md) (branch `worktree-plan-109-zig-pid0-exploration`, HEAD `9cdde6a0`) — the parent plan. Focus on §"Systems-design recommendation" (it's positional — lean Rust v2 is the primary track), §"Process lineage", §"Workstreams W2 + W2′", §W5. The plan was originally drafted as Plan 105; the renumbering note at the top explains why. **Use "Plan 109" in all new work.**
- [ ] [`specs/research/agent-evolution-tradeoff-note.md`](../research/agent-evolution-tradeoff-note.md) (branch `worktree-plan-109-w1-tradeoff-note`, HEAD `958b0714`) — the W1 deliverable. The measurement rubric in §"Shared verification + measurement" is what your prototypes must produce numbers for. §"Vsock surface — broader than the agent control channel" explains why your work touches only the agent channel, not the broker.
- [ ] [`specs/adrs/060-pid0-portability-boundary.md`](../adrs/060-pid0-portability-boundary.md) — §8 (language neutrality) tells you what byte-identical contract your prototypes must preserve.
- [ ] [`specs/adrs/063-boundary-language-policy.md`](../adrs/063-boundary-language-policy.md) — §4 lists six required deliverables you must produce alongside any non-Rust adoption (only relevant for W2 Zig, not W2′).
- [ ] `crates/mvm-guest/src/bin/mvm-guest-netinit.rs` + `crates/mvm-guest/src/netinit.rs` — the baseline Rust binary (~87 LoC main + ~100 LoC support) you're porting. **Stays canonical**; W2/W2′ are additions, not replacements.
- [ ] `crates/mvm-core/src/protocol/vm_backend.rs` — the `VmCapabilities` struct + `BackendSecurityProfile` that W5's capability matrix derives from.
- [ ] [`specs/SPRINT.md`](../SPRINT.md) Sprint 58 — where Plan 109 lives in sprint tracking. Update the workstream checkboxes there as you complete pieces.

## Scope discipline (non-negotiable)

- [ ] **Don't touch W3, W4a, or W6.** Those workstreams are deliberately deferred pending coordination with the secrets-work session. W3 is the vsock encryption design (Noise_NK); W4a is the control-plane-vs-data-plane ADR; W6 is ADR-002 threat-model edits. Any encryption design, any ADR-002 edit, any "control plane vs data plane" framing must wait.
- [ ] **Don't claim new plan or ADR slot numbers without verification.** Slot 105 was lost to a renumbering race mid-conversation. When this session needs any new spec file with a number, re-verify with `gh pr list --state all --search "plan-NNN OR adr-NNN"` + `git ls-tree --name-only origin/main:specs/plans/` + `git ls-tree --name-only origin/main:specs/adrs/` immediately before commit. Don't unilaterally renumber other sessions' work.
- [ ] **`mvm-guest-agent` stays untouched.** This session is about the netinit prototype (one-shot, no process supervision) and the capability matrix (doc-only). The agent is explicitly out of scope per Plan 109's non-goals.
- [ ] **All current Plan 109 branches are unpushed.** The user reviews and decides when to push and how to stage PRs. Don't push without instruction.

## Worktree workflow (required)

Per the user's standing instruction, all work happens in git worktrees, never in the main checkout. Use the `EnterWorktree` tool. One worktree per workstream:

- [ ] `worktree-plan-109-w5-capability-matrix` — for W5
- [ ] `worktree-plan-109-w2-prime-lean-rust-netinit` — for W2′
- [ ] `worktree-plan-109-w2-zig-netinit` — for W2

Each worktree branches from the latest Plan 109 commit so cross-references resolve. To branch a worktree from a non-default base: from inside any current worktree or the main checkout, run

```bash
git worktree add -b <branch-name> .claude/worktrees/<dir-name> worktree-plan-109-w4-adrs
```

(or whichever Plan 109 branch is latest at the time), then `EnterWorktree(path=...)` to switch the session into it.

## Recommended sequencing

Start with **W5** (smallest, fully unblocked, gives the user a quick win + something to review). Then **W2′** (the primary track per Plan 109's recommendation). Then **W2** (the comparison check).

W5 + W2′ can run in parallel if the session is aggressive; W2 should wait until W2′ has numbers so the comparison column exists.

---

## W5 — Provider capability matrix

**Worktree**: `worktree-plan-109-w5-capability-matrix`
**Deliverable**: `specs/reference/provider-capabilities.md` (new doc).
**Estimated effort**: ~1 hour.

### Steps

- [ ] Create the worktree, branch from `worktree-plan-109-w4-adrs` HEAD (or latest Plan 109 branch).
- [ ] Read `crates/mvm-core/src/protocol/vm_backend.rs`. Find the `VmCapabilities` struct and the `AnyBackend` enum (or equivalent backend-dispatch type).
- [ ] Draft `specs/reference/provider-capabilities.md` with:
  - [ ] **Columns**: vsock, virtiofs, snapshotting, egress control, attestation, rosetta, gpu, nested virt, **vsock-encryption-support** (new — informs Plan 109 W3 feasibility; values are "yes" for any backend whose vsock transport supports plaintext byte streams, which is all of them — encryption is application-level Noise_NK on top, not a backend property).
  - [ ] **Rows**: Firecracker, Cloud Hypervisor, libkrun (Linux), libkrun (macOS), Apple VZ, Apple Container, Docker, Mock.
  - [ ] One brief sentence per row about the backend's role / production tier.
  - [ ] One brief sentence per column about what the capability means.
- [ ] **Code-of-truth check**: every column maps 1:1 to a field on `VmCapabilities`; every row maps 1:1 to a variant on `AnyBackend`.
- [ ] If a column has no field: add the field to `VmCapabilities` (one-line struct change + bool default + update each backend's profile builder).
- [ ] If a row has no variant: the matrix is wrong, don't add a phantom row.
- [ ] Optionally add `vsock_encryption_support: bool` to `VmCapabilities` (likely missing — Plan 109 W3 anticipates needing it). Default `true` wherever `vsock: true`.
- [ ] Smoke checks: `cargo fmt --all -- --check` + `cargo test --workspace --no-run`.
- [ ] Commit. Suggested message: `docs(reference): provider capability matrix (Plan 109 W5)`.
- [ ] Update `specs/SPRINT.md` Sprint 58's success-criteria checkbox for W5.
- [ ] Stop. Let the user review before pressing into W2′.

---

## W2′ — Lean Rust v2 netinit prototype

**Worktree**: `worktree-plan-109-w2-prime-lean-rust-netinit`
**Deliverable**: new crate `crates/mvm-guest-netinit-lean/` producing a binary behaviorally identical to `mvm-guest-netinit` but with a dramatically smaller dep tree.
**Estimated effort**: 2–4 days.

### Dependencies allowed

- [ ] `polling` (smol team's epoll/kqueue abstraction)
- [ ] `linux-raw-sys` (auto-generated kernel header bindings)
- [ ] `libc`

That's it. No `tokio`, no `rtnetlink`, no `netlink-packet-route`, no `serde_json` (the marker JSON output is small and stable enough to hand-format).

### Behavior contract (preserved byte-identically per ADR-060 §8)

- [ ] Install AF_NETLINK socket; send RTM_NEWROUTE messages installing blackhole routes for `MANDATORY_DENY_RANGES`. Copy or `pub use` the constant from `crates/mvm-guest/src/netinit.rs`; do not redefine.
- [ ] Emit the `__MVM_NETINIT_REPORT__` marker JSON to stdout. Format must match the baseline byte-for-byte.
- [ ] Exit codes: 0 = success, 1 = some routes failed, 2 = systemic rtnetlink failure.

### Validation harness

- [ ] Add a workspace-level integration test (`tests/netinit_parity.rs` or similar in the new crate): spawn both `mvm-guest-netinit` and `mvm-guest-netinit-lean` on the same kernel (Linux only — gated `#[cfg(target_os = "linux")]`); diff stdout/stderr/exit-code. Must be identical.
- [ ] Fuzz harness: `cargo-fuzz` target on the netlink reply parser surface. Mirror the existing harness style in `crates/mvm-guest/fuzz/fuzz_targets/`.
- [ ] Reproducible build: build the binary twice (same source, same toolchain) and confirm byte-identical (claim 7's invariant).

### Measurement (the headline question per Plan 109 §D1)

- [ ] Produce a section in `specs/research/netinit-prototype-measurements.md` populating the rubric:
  - [ ] Dep-tree LoC reaching the boundary
  - [ ] Transitive crate count + advisory exposure
  - [ ] Stripped binary size (musl, aarch64 + x86_64)
  - [ ] Compile time cold + warm
  - [ ] Behavioral parity (pass/fail vs baseline)
  - [ ] Schema parity (pass/fail vs `mvm-core` types)
  - [ ] Fuzz time-to-first-crash on adversarial corpora
  - [ ] Memory ceiling under stress (100k route attempts)
  - [ ] Reproducibility (byte-identical rebuilds)
  - [ ] CI cost delta vs baseline
- [ ] Three columns in the measurements table: **baseline Rust** / **lean Rust v2** / (Zig column reserved for W2).

### Workspace integration

- [ ] Add `crates/mvm-guest-netinit-lean` to the root `Cargo.toml`'s `[workspace] members`.
- [ ] Don't change any other crate's `Cargo.toml`.
- [ ] Don't make the dev image cmdline switch to use this binary yet. Keep Rust baseline as default per Plan 109 §I4 (reversibility invariant). Both binaries sit side-by-side.

### Wrap-up

- [ ] Smoke checks: `cargo fmt --all -- --check` + `cargo test --workspace --no-run` + `cargo test -p mvm-guest-netinit-lean` (the parity test if on Linux).
- [ ] Commit. Suggested message: `feat(mvm-guest-netinit-lean): W2′ lean Rust v2 prototype (Plan 109)`.
- [ ] Update Sprint 58 W2′ checkbox.
- [ ] Stop at a natural checkpoint and let the user review before pressing into W2.

---

## W2 — Zig netinit prototype

**Worktree**: `worktree-plan-109-w2-zig-netinit`
**Prerequisite**: W2′ must have committed measurements first so the comparison column exists.
**Deliverable**: `zig/mvm-guest-netinit/` directory with `build.zig` + Zig source.
**Estimated effort**: 2–4 days.

### Adoption gate

ADR-063 §4 lists six deliverables required before any non-Rust adoption. **The prototype builds them up; adoption is a separate later decision, not part of this session.** The plan's expected outcome (per §"Expected outcome + branches") is Outcome B — lean Rust v2 wins — and the Zig binary stays as evidence in tree, not as the shipping artifact.

### Layout

- [ ] Top-level `zig/mvm-guest-netinit/` directory with its own `build.zig`.
- [ ] **Do NOT** put Zig under `crates/` (cargo would try to recurse).
- [ ] Pin a Zig version in the `build.zig` comments (Zig is pre-1.0; pin the exact minor).
- [ ] Use musl-static cross-compile for `aarch64-linux` and `x86_64-linux`.

### Behavior contract (identical to W2′)

- [ ] Same AF_NETLINK + RTM_NEWROUTE installation for `MANDATORY_DENY_RANGES`.
- [ ] Same `__MVM_NETINIT_REPORT__` marker (byte-for-byte).
- [ ] Same exit codes (0 / 1 / 2).

### Schema parity test (ADR-063 §3, ADR-060 §8, Plan 109 §D2)

The Zig binary must consume Rust-canonical types. Cheapest approach:

- [ ] Emit the report-marker JSON Schema from the Rust side: `schemars` derive on the report struct + a one-shot binary in the lean crate (`bin/emit_netinit_schema.rs`) that dumps the schema as JSON.
- [ ] `build.zig` invokes that emitter; parses the resulting JSON Schema; generates Zig type definitions at build time.
- [ ] If the Rust types drift, the Zig build fails. This is the contract-preservation mechanism Plan 109 §D2 requires.

### Fuzz + reproducibility

- [ ] AFL++ instrumentation on the Zig netlink parser. Document the invocation in `zig/mvm-guest-netinit/README.md`.
- [ ] Reproducible build: byte-identical rebuilds, same as W2′.

### Measurement

- [ ] Fill in the third column of `specs/research/netinit-prototype-measurements.md`.
- [ ] Compare against W2′ on every rubric row.

### Stop discipline

- [ ] **Do not propose adopting the Zig binary as the default.** Plan 109's expected outcome is Outcome B (lean Rust v2 wins).
- [ ] **Do not delete or gate the Rust baseline.** Both binaries stay side-by-side per I4.
- [ ] Commit. Suggested message: `feat(zig): W2 Zig netinit prototype + measurements (Plan 109)`.
- [ ] Update Sprint 58 W2 checkbox.

---

## Reporting cadence

- [ ] After W5: stop. Report commits, files, smoke-check results. Don't chain into W2′ without confirmation.
- [ ] After W2′: stop. Report measurements table state (which rubric items have numbers, which are TODO).
- [ ] After W2: stop. Report the final three-column measurements table + a one-sentence recommendation (lean Rust v2 wins / Zig wins / inconclusive — matching Plan 109's §"Expected outcome + branches" framing).

## Expected session output

By end of this session:

- [ ] `worktree-plan-109-w5-capability-matrix` branch with `specs/reference/provider-capabilities.md` committed, optionally a `vsock_encryption_support` field added to `VmCapabilities`.
- [ ] `worktree-plan-109-w2-prime-lean-rust-netinit` branch with `crates/mvm-guest-netinit-lean/` committed, parity test committed, fuzz target committed, first two columns populated in `specs/research/netinit-prototype-measurements.md`.
- [ ] `worktree-plan-109-w2-zig-netinit` branch with `zig/mvm-guest-netinit/` committed, schema-parity test committed, AFL harness committed, third column populated in the measurements doc.

All three branches **unpushed**. The user reviews and decides when to push and how to stage PRs.

## Anti-patterns to avoid

- [ ] Touching `crates/mvm-guest/src/bin/mvm-guest-agent.rs` (out of scope — Plan 109 explicitly defers agent rewrite).
- [ ] Writing the W3 encryption design doc (deferred, waiting for secrets session coordination).
- [ ] Editing `specs/adrs/002-microvm-security-posture.md` (deferred per W6).
- [ ] Drafting a control-plane-vs-data-plane ADR (deferred per W4a).
- [ ] Claiming new plan or ADR slot numbers without re-verifying against current `main` + open PRs.
- [ ] Using "Plan 105" in any new commit message, file content, or cross-reference — Plan 109 is canonical.
- [ ] Stacking all three workstreams into one branch / one commit. One worktree per workstream; one PR per worktree later.
- [ ] Making the dev image cmdline switch to W2′ or W2 binaries by default — both prototypes stay opt-in via flake option per I4.
- [ ] Removing the Rust baseline netinit binary — it stays canonical until at least one release cycle past any default flip (ADR-063 §5).

## Cross-references

- [`Sprint 42 §Track E`](../SPRINT.md#track-e--zig-evaluation-gates) — origin of the Zig evaluation gates Plan 109 implements.
- [`Plan 104`](104-host-services-broker.md) + [`ADR-059`](../adrs/059-host-services-broker.md) — adjacent vsock surface; broker is host-side and out of pid0 scope per ADR-060.
- [`ADR-002`](../adrs/002-microvm-security-posture.md) — 10 security claims; your work must not regress any of them (I1/I2/I3/I4 in Plan 109 capture the specifics).
- `crates/mvm-egress-proxy/` — existing precedent for "lean Rust" discipline (libc only, no tokio). Read before W2′ if unfamiliar with no-tokio Rust binaries.

## Open uncertainties (call out in measurements, don't work around)

- **`polling` at agent scale**: proven for one-shot binaries (this prototype), unproven for the agent's 33-variant dispatcher + long-lived loop + warm-pool state. W2′ only calibrates the boundary-binary case; flag any musl-static netlink surprises in the measurements doc rather than working around them.
- **`schemars` for the marker schema**: if the Rust report struct doesn't currently derive `Serialize`, you may need to add it. Keep the addition minimal — don't refactor the struct.
- **Zig version**: pin the version at first commit; document upgrade policy in `zig/mvm-guest-netinit/README.md`. Floating Zig versions break claim 7 reproducibility.

## Session start checklist

- [ ] Read all seven items in §"Read first" before any edits.
- [ ] Confirm Plan 109 is canonical (not Plan 105 — that slot belongs to a different plan on main).
- [ ] Confirm three Plan 109 branches exist locally and are unpushed (`git branch -a | grep plan-109`).
- [ ] Confirm slot 110 is this playbook (don't claim a different slot for it).
- [ ] Start with W5.
