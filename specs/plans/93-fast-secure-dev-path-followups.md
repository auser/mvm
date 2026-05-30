# Plan 93 — fast + secure dev path (post-Plan-91 follow-ups)

**Status:** drafted 2026-05-22. **Phase 0 / PR-A shipped** (PR #504,
`b848a309`). **Phases 1–3 in flight** under Sprint 59
(`worktree-plan-93-fast-secure-dev-path`), sequenced as a chain of
PRs (PR-1 bench harness → PR-2/3 observability → PR-4..8 Phase 1 →
PR-9/10 Phase 2). Phase 1 host cross-compile resolved to **static
musl** (see Phase 1 Lever 2).

**Follows:**
- [Plan 91](91-stage0-alpine-bootstrap.md) — Stage 0 Alpine
  bootstrap. The cold-Stage-0 redesign lives there; this plan
  *does not* re-litigate it.
- [Plan 77](77-stage0-bootstrap-via-dev-image.md) — Stage 0
  bootstrap via the cached dev image (the predecessor architecture
  that #414/#415/#417 supersede).

**Tracks:** sub-30 s `mvmctl dev up` on warm hosts; sub-200 ms
runtime microvm launch; "no LONG dev cycles" for daily
contributor work.

## Context

Plan 77 W2/W3/W4 shipped to main (advisory lock, audit emits,
structural no-prebuilt gate). Plan 91 is the chosen Stage 0
redesign (Alpine minirootfs + `apk add nix-bin`) and is in flight
via PR #417 + the already-merged #414/#415.

What's left for "fast + secure mvm dev path":

1. **A shipping fingerprint correctness bug** in
   `builder_vm_source_fingerprint` (Plan 91 doesn't touch this).
2. **Slow Layer 2 dev-shell rebuilds** — separate code path from
   Stage 0, so Plan 91 doesn't address it.
3. **Sub-200 ms runtime microvm launch** — completely decoupled
   from Stage 0; lives in `LibkrunBackend::start` + vsock
   handshake.
4. **DX polish** for the post-Plan-91 dev loop —
   `mvmctl doctor`, `cache info`, progress UI, public docs.

User targets that drive this work:

- Sub-30 s end-to-end `mvmctl dev up` on a fresh checkout.
- Sub-200 ms cold launch for runtime microvms.
- Fast Layer 2 dev-shell rebuilds — no LONG development cycles.
- Security top priority — no shortcut breaks the no-prebuilt
  download invariant, fingerprint binding, audit chain, or
  ADR-047 egress allowlist.
- Nix only in the builder VM — host has no Nix; runtime
  microvms have no Nix; dev VMs eventually have no Nix.
- No backward compatibility — hard cutover; existing caches blown
  away on upgrade (per `feedback_no_backcompat_first_version.md`).
- Source tree must not balloon.

## What this plan does NOT cover

- **Stage 0 architecture** — that's Plan 91 (Alpine minirootfs +
  `apk add nix-bin`). The trust model, fetch shape, and
  verification chain are settled there; nothing in this plan
  contradicts or duplicates it.
- **Slim kernel** — that's Plan 92/95.
- **The full sub-30 s cold-first-checkout target** — Plan 91 plus
  Plan 92's slim kernel are expected to deliver this; this plan
  contributes the *warm* dev-loop speed, not the cold-checkout
  number.

## Phase 0 — PR-A: fingerprint correctness fix (immediate, small)

### Why it's urgent independent of Plan 91

`crates/mvm-cli/src/commands/env/apple_container.rs::builder_vm_source_fingerprint`
hashes only `flake.nix + flake.lock` from `nix/images/builder-vm/`.
That's a *shipping security bug*: a contributor who edits
`crates/mvm-builder-init/src/install.rs` and runs `mvmctl dev up`
gets the cached builder VM from before the edit. Neither flake
file changed; fingerprint matches; stale image is served.

Plan 91 swaps the Stage 0 bootstrap binary but does not change
the source fingerprint binding. The bug remains shipping. Either
this PR lands before Plan 91 (recommended — it's small) or it
lands as a Plan 91 follow-up; either way, it's distinct work.

### Scope

#### Change 1 — extend `builder_vm_source_fingerprint`

Extend the fingerprint to also walk the Cargo source files that
compile into the builder VM image:

- workspace `Cargo.lock`
- `crates/mvm-builder-init/Cargo.toml` + recursive
  `crates/mvm-builder-init/src/**`
- `crates/mvm-egress-proxy/Cargo.toml` + recursive
  `crates/mvm-egress-proxy/src/**`

Deterministic walk (sorted-name traversal via `BTreeMap`). Skip
`.DS_Store`, `*.swp`, `target/`.

#### Change 2 — `flavor=` field on Stage 0 audit emits

Add a `flavor=current` field to `stage0_boot` and
`stage0_cache_promoted` detail strings. Today it's a single
constant; if Plan 91 ever needs a per-bootstrap-variant
identifier, the audit shape already accommodates it. ~10 lines.

#### Change 3 — no backcompat migration

Per `feedback_no_backcompat_first_version.md`, no v1-cache
recogniser. The widened fingerprint naturally invalidates
existing caches; the next `mvmctl dev up` pays one cold Stage 0.
The existing fingerprint-mismatch UI surface already explains
the diagnosis to the user.

### Critical files

- `crates/mvm-cli/src/commands/env/apple_container.rs` —
  `builder_vm_source_fingerprint`, Stage 0 audit emits.
- Tests under existing `builder_vm_bootstrap_tests` mod.

### Ship checklist (Phase 0 / PR-A)

Code changes:

- [x] Extend `builder_vm_source_fingerprint` to include workspace
      `Cargo.lock` + `crates/mvm-builder-init/{Cargo.toml,src/**}` +
      `crates/mvm-egress-proxy/{Cargo.toml,src/**}` via a
      deterministic sorted walk.
- [x] Add `flavor=current` field to `stage0_boot` /
      `stage0_cache_promoted` audit detail strings.

Tests:

- [x] Editing `crates/mvm-builder-init/src/foo.rs` changes the
      fingerprint.
- [x] Editing `crates/mvm-builder-init/README.md` (or any non-`src`
      file) does NOT change the fingerprint.
- [x] Deterministic walk: same fingerprint twice in a row.

Verification:

- [x] `cargo test -p mvm-cli --lib -- builder_vm_bootstrap_tests`
      passes (including the new tests).
- [x] `cargo test --workspace` — 0 failures.
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

PR-A is ~150-250 lines, ships in 1-2 days.

## Phase 1 — fast Layer 2 dev cycles

> **SUPERSEDED by [ADR-065](../adrs/065-single-builder-dev-image.md)
> (Proposed 2026-05-29).** ADR-065 landed on `main` while Phases 1–3
> were in flight and reshapes this phase end-to-end, so the Lever 1/2
> design below is retained only for history — **do not implement it as
> written.** Reconciliation:
> - **Lever 1** (split `nix/images/builder/flake.nix` into
>   `dev-minimal`/`dev-compile`) → ADR-065 **deletes** that flake and
>   makes `nix/images/builder-vm/flake.nix` the single flake with
>   `default` (headless) + `dev` (interactive) attributes.
> - **Lever 2** (host **musl** cross-compile + virtiofs **bind-mount**
>   into the running dev shell) → ADR-065 **embeds** the Linux binaries
>   into `mvmctl` at *mvmctl's own* build time (`build.rs` + `cargo
>   zigbuild --target aarch64-unknown-linux-gnu` + `include_bytes!`),
>   extracted at runtime to `~/.cache/mvm/host-bins/`. This uses
>   **glibc via zigbuild, not static musl** — the musl decision recorded
>   in the Context above is therefore moot.
>
> PR-4..8 are **deferred**; the new Phase 1 is "implement ADR-065,"
> coordinated with the `specs/prompts/93-phase-1-2-3-fast-secure-dev.md`
> track rather than raced. Phase 2 (PR-9/10) and the shipped Phase 0 /
> PR-1/2/3 observability work are unaffected.

Decoupled from Stage 0. The dev-shell flake at
`nix/images/builder/flake.nix` builds the rootfs contributors
`dev shell` into. Today it pulls rustc + cargo + gcc + busy
toolchain — minutes warm, 20-40 min cold. This is what
contributors actually feel daily.

### Levers

1. **Lazy / split dev shell**: monolithic dev shell splits into
   `dev-minimal` (bash + git + basics, ~50 MiB) and `dev-compile`
   (rustc + cargo + gcc, on-demand fetch from cache.nixos.org
   inside the VM). Most workflows only need minimal.

2. **Cross-compile our guest crates on host (static musl)**:
   contributors editing the Linux-resident guest binaries
   (`mvm-guest-agent`, `mvm-builder-init`, `mvm-egress-proxy`)
   cross-compile on the host via `cargo --target
   aarch64-unknown-linux-musl` and bind-mount the binary into the
   running dev shell. **No dev-shell rebuild needed for our own
   code edits.** This is the lever that actually delivers "no
   LONG dev cycles" — day-to-day work bypasses Nix entirely.
   (`mvmctl` itself is the host orchestrator; it is *not* a default
   bind-mount target — running it inside the Linux dev VM is
   meaningless.)

   **Resolved (2026-05-29) — target static musl, not a glibc
   sysroot.** A glibc-gnu cross binary expects `/lib/ld-linux-*.so`,
   which a Nix-built rootfs does not provide (Nix patches binaries
   to a `/nix/store` loader), so it likely would not even run in the
   dev shell. `<arch>-unknown-linux-musl` static is fully
   self-contained — **no loader, no sysroot fetch** — and is already
   the established pattern for `mvm-builder-init` in this repo. This
   eliminates the pinned-sysroot download and its entire
   supply-chain/verification surface. Caveat: a workspace crate with
   a C `build.rs` needs a musl-capable C compiler on the host (the
   same C-toolchain requirement glibc would impose, minus the
   loader/sysroot problem); surface that prerequisite in
   `mvmctl doctor`.

3. **Lazy Nix fetch inside dev-compile**: when the user does
   need in-VM compilation, the dev-shell flake uses Nix's
   substituter as normal. Slow first time per machine, fast
   after. The in-VM Nix store survives across `dev up`
   invocations via the persistent virtio-blk image already in
   place.

### What this does NOT do

- **No host-side Nix store mirror.** Multi-GB mirrors are
  explicitly out of bounds per user direction ("I don't think we
  want to store/ship 5 GB around").
- **No new download infrastructure beyond what Plan 91 already
  establishes.** The Alpine fetch pattern is the precedent;
  this phase reuses it.

### Scope

Per-lever: 1-2 weeks. Ships incrementally. Lever 2 is the
load-bearing one for the user's "no LONG dev cycles" target;
levers 1 and 3 are cleanup that makes the dev shell smaller and
the cold case better.

### Ship checklist (Phase 1)

- [ ] Lever 1 — split monolithic dev shell into `dev-minimal`
      and `dev-compile` flake outputs; default `dev shell` uses
      `dev-minimal`.
- [ ] Lever 2a — musl cross target: `targets = [...-musl]` in
      `rust-toolchain.toml` + `.cargo/config.toml` stanzas. No
      sysroot fetch (static musl is self-contained). Audit guest-
      binary crates for C `build.rs`; document any musl C-compiler
      prerequisite + surface in `doctor`.
- [ ] Lever 2b — `mvmctl dev compile` (alias `dev sync`):
      cross-compile the guest binaries to static musl and bind-mount
      via a **per-VM** virtiofs share `~/.mvm/dev/<vm>/binbridge`
      (RO) mounted at `/opt/mvm-binbridge`, prepended to `$PATH`. Live
      virtiofs → re-edit refreshes with no VM restart. **v1: libkrun/
      Apple-Container macOS path only.**
- [ ] Lever 2c — reproducibility regression CI lane: build same
      source from two pinned `macos-14` runners, assert byte-identical
      artifacts (musl-static; no sysroot to pin).
- [ ] Lever 3 — confirm the persistent `/nix` virtio-blk image
      survives `dev up`/`dev down` cycles with the dev-compile
      flake's closure; document the cold-fetch + warm-reuse
      contract in the dev-shell flake's README; add a
      `should_delete_nix_store(reset)` test.

### deferred follow-ups

- [ ] **Vz + Firecracker/Linux bind-mount coverage** — Phase 1
      Lever 2b ships libkrun/Apple-Container (macOS) only; the Vz
      builder backend and the Linux Firecracker/libkrun-direct
      contributor path use different mount wiring. Mirrors the
      existing gateway-audit-substrate backend-coverage gap.
- [ ] **Vz + Firecracker launch benches** — Phase 2 Lever 0
      (`mvmctl bench microvm-launch`) measures libkrun only in v1.
- [ ] **Phase 2 Lever 1 kernel cmdline trim** — the
      `console=hvc0`-drop trim is gated on Plan 92/95 (slim kernel)
      merging to `main`. The kernel-agnostic cmdline-override
      plumbing + validation allowlist land now.
- [ ] **Phase 2 Lever 4 VMM balloon** — blocked on libkrun exposing
      a virtio-balloon C API (`capabilities().balloon = false`
      today); upstream-tracked. Intended `KrunContext::with_balloon`
      shape documented; a balloon target may never exceed the
      admitted `plan.mem_mib`.

## Phase 2 — sub-200 ms runtime microvm launch

Decoupled entirely from Stage 0 + dev shell. Lives in
`crates/mvm-backend/src/libkrun.rs::start`, the vsock handshake,
and `mvm-guest`'s init path. Today `LibkrunBackend::start` is
1-3 s cold; the target is sub-200 ms.

### Levers

1. **Kernel cmdline + initrd minimisation.** Today's cmdline is
   conservative. Trim aggressively for runtime microvms (Plan 95's
   slim kernel work helps here). Initrd may be eliminable
   entirely for the runtime path.

2. **Guest agent startup parallelism.** Today the agent
   sequentialises vsock listen + handshake + capability
   negotiation. Parallelise + pre-bind on the host side. Issue
   tracking the handshake roundtrip latency is needed.

3. **Warm pool of pre-spawned libkrun supervisors awaiting
   rootfs attachment.** RAM cost: ~50-100 MiB per warm guest.
   Trade RAM for cold-start latency. Pool size configurable;
   `mvmctl up --warm-pool-size N`.

4. **VMM-side memory ballooning at boot.** Defer non-essential
   page allocation until after the guest is "ready" from the
   user's perspective. libkrun's balloon API is documented;
   today we don't use it on the runtime path.

### Scope

Multi-week, multi-PR. Needs a benchmark harness
(`mvmctl bench microvm-launch` or similar) so progress is
measurable before any lever lands. Without measurement we'll
optimise the wrong thing.

### Ship checklist (Phase 2)

- [ ] Benchmark harness lands first — `mvmctl bench
      microvm-launch` (or equivalent) measures cold launch
      wall-clock with sub-millisecond precision, persists
      results for regression tracking. Every other Phase 2
      item blocks on this.
- [ ] Lever 1 — kernel cmdline trim + initrd elimination on the
      runtime path. Document the minimum cmdline contract.
- [ ] Lever 2 — guest agent startup parallelism + host-side
      vsock pre-bind; measure handshake roundtrip via the
      benchmark harness.
- [ ] Lever 3 — warm pool of pre-spawned libkrun supervisors
      with `mvmctl up --warm-pool-size N` (defaults to 0 =
      off; document RAM cost per warm guest).
- [ ] Lever 4 — VMM-side memory ballooning at boot so commits
      are deferred until the guest is "ready".
- [ ] Sub-200 ms cold launch demonstrated end-to-end on an
      M-series macOS runner via the benchmark harness.

## Phase 3 — DX polish (distributed)

These ride alongside the implementation of Phases 0-2 rather
than landing as a standalone phase.

### Ship checklist (Phase 3)

- [ ] **Stage 0 progress feedback** (folded into Plan 91 ship):
      one-line progress per step (`Fetching Alpine minirootfs …
      0.4 s`, `apk add nix-bin … 1.8 s`, `nix build … 12.3 s`).
      Cheap; makes perceived speed match actual speed.
- [ ] **`mvmctl cache info`** reports vendored-blob ages,
      cross-target cache size, assembled rootfs age, last
      Stage 0 fingerprint.
- [ ] **`mvmctl doctor`** reports last Stage 0 timestamp,
      fingerprint, hit/miss outcome. Diagnoses "why did Stage 0
      fire again?" without grep-ing the audit log.
- [ ] **Public docs** at `public/src/content/docs/contributing/`:
      the new Stage 0 model (Plan 91), edit-rebuild semantics
      (Phase 1), triage paths.
- [ ] **CI reproducibility lane** (Phase 1 lever 2c, surfaced
      here for visibility): nightly cross-compile from two
      macOS runners with byte-identity assertion.
- [ ] **`LocalAuditKind::VendorBlobFetched`** audit kind:
      emitted on every download + SHA/PGP verify event.
      Forward-compat with Plan 91's Alpine fetch.

## Reproducibility (Phase 1 lever 2)

Inputs to the cross-compiled `mvm-builder-init` / workspace
binaries output bytes:

1. Rust source files
2. `Cargo.lock` (pinned)
3. `rustc` version — pin via `rust-toolchain.toml`
4. Cross sysroot — manifest-pinned bytes (same as Plan 91's
   Alpine pin)
5. Build flags — centralize in workspace `[profile]`; set
   `RUSTFLAGS=--remap-path-prefix=$PWD=.`
6. `build.rs` of dependencies — audit for non-determinism

With 1-6 pinned, two contributors on the same OS major version
produce byte-identical artifacts. Residual risk on different
macOS major versions — rustc occasionally embeds
host-version-dependent behavior. Tolerate; fingerprint mismatch
(PR-A's widened fingerprint catches this) causes one extra cold
rebuild, not a security issue.

## Considerations summary

| # | Consideration | Lands in |
|---|---|---|
| 1 | Egress allowlist regression check | Plan 91 (init refuses install w/o egress proxy) |
| 2 | Cache schema migration | Skipped (no backcompat) |
| 3 | `mvm-builder-init` refuses install in flake-only | Plan 91 |
| 4 | Cross-compile attestation | Phase 1 lever 2 (manifest-pinned sysroot, same shape as Plan 91 Alpine fetch) |
| 5 | Per-flavor lock | Skipped (Plan 91 retires the flavor concept) |
| 6 | Audit `flavor=` field | PR-A (Phase 0) |
| 7 | `cache prune` recogniser update | Plan 91 (clean cutover, no v1 layout to preserve) |
| 8 | Stage 0 progress feedback | Phase 3 (folded into Plan 91 ship) |
| 9 | `mvmctl cache info` enrichment | Phase 3 |
| 10 | `mvmctl doctor` enrichment | Phase 3 |
| 11 | Docs | Phase 3 |
| 12 | Plan numbering | Verified — Plan 93 free, Plan 91/92/95 in flight |
| 13 | CI reproducibility lane | Phase 3 |
| 14 | Reproducibility for cross-compile | Phase 1 lever 2 |
| 15 | Layer 2 dev-shell rebuild bottleneck | Phase 1 |
| 16 | Vendored-blob audit | Phase 3 (forward-compat with Plan 91) |

## Success criteria

User-facing targets that drive this plan. Each is ticked when
measurable via either the Phase 2 benchmark harness
(`mvmctl bench microvm-launch`) or a documented manual
procedure on an M-series macOS host.

- [ ] **Warm `mvmctl dev up` ≤ 5 s.** Contributor with a
      populated builder VM cache and a populated host cargo
      cache. Measured wall-clock from `mvmctl dev up` invocation
      to dev-shell prompt.
- [ ] **Cold `mvmctl dev up` from a fresh checkout ≤ 30 s.**
      Plan 93 contributes via Phase 1; Plan 91 (Alpine Stage 0)
      and Plan 92/95 (slim kernel) are co-load-bearing. This
      target is the headline "fast dev machine" goal.
- [ ] **Cold runtime microvm launch ≤ 200 ms.** Measured on a
      cold libkrun guest with the minimum cmdline + no initrd;
      Phase 2's headline goal.
- [ ] **No LONG dev cycles.** A contributor edit to any
      workspace crate reaches the running dev shell in < 10 s
      via Phase 1 lever 2's host cross-compile + bind-mount
      path. No dev-shell rebuild required for our own code
      edits.
- [ ] **No security regressions.** PR-A's fingerprint widening
      catches Cargo source edits. No Phase 1 / 2 / 3 path
      introduces a download bypassing hash + signature
      verification. Vendored-blob audit kind in Phase 3 makes
      every supply-chain fetch auditable.

## Process notes

- This plan was drafted in a session that initially proposed a
  competing Stage 0 redesign (busybox-static + vendored bytes +
  host cross-compile of `mvm-builder-init`). That proposal was
  retracted on discovery of Plan 91, which uses a stronger
  Alpine-based trust model already in flight. The lesson for
  future planning sessions: run `git worktree list && gh pr list`
  before designing in an area, per
  `feedback_check_inflight_work_before_diagnosing.md` in memory.
- Plan numbers verified free at draft time via `ls specs/plans/`
  + `gh pr list --search 'plan 93'`. Per
  `project_spec_numbering_chaos.md`, re-verify just before any
  implementation PR lands.
