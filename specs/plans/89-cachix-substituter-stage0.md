# Plan 89 — Cachix substituter for Stage 0 acceleration

**Status:** drafted 2026-05-18, awaiting review + one-time user provisioning.
**Pairs with:** nothing (no ADR — see "Why no ADR" below).
**Depends on:** Plan 87 PR3/PR4 (passt as default; merged as #360).

## Problem

A cold `mvmctl dev up` rebuilds four derivations from source inside
the Stage 0 builder VM because cache.nixos.org doesn't host them:

- the TSI / passt kernel under `nix/images/builder-vm/kernel/`
- `mvm-builder-init`
- `mvm-egress-proxy`
- `mvm-guest-agent`

These are our derivations. Source builds dominate the first-run wall
clock — typically several minutes of `rustc` + `make` before the
builder VM even comes up. Every fresh checkout, every fresh CI runner,
and every `~/.mvm` reset pays the cost again.

Cache.nixos.org can't host them (they're our outputs, not nixpkgs').
A binary cache we control would fix this with a Stage-0-only substituter
fetch.

## Goal

After this plan: a `mvmctl dev up` on a fresh machine pulls the four
big derivations as pre-built binaries from `https://mvm.cachix.org`
in seconds instead of minutes. CI runners get the same win.

Steady-state user-flake builds are explicitly out of scope (see "What
this does NOT help" below). The cache only fires during Stage 0.

## Why no ADR

Three reasons:

1. The trust model is the same one ADR-002 already enumerates for
   `cache.nixos.org` — a `trusted-public-keys` entry that gates
   accepting any substituted closure. We're adding one more entry, not
   inventing a new mechanism.
2. The blast radius is bounded: a compromised cachix.org host can
   ship bytes, but Nix refuses any closure not signed by our key, so
   the attacker also has to compromise our CI signing token. The
   pre-existing `cache.nixos.org-1:…` trust already has the same
   shape.
3. The scope is narrow: Stage-0-only. Steady state remains air-gapped
   per ADR-047's existing posture.

If reviewers want an ADR captured anyway, ADR-056 is the next free
slot.

## Design

### One substituter line in `cmd.sh`'s `NIX_CONFIG`

`crates/mvm-build/src/libkrun_builder.rs` emits the `cmd.sh` that
runs inside both Stage 0 and steady-state builder VMs. The
`NIX_CONFIG` block grows two entries (compared to today):

```
substituters       = https://cache.nixos.org/ https://mvm.cachix.org
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= \
                      mvm.cachix.org-1:<USER_PROVISIONS_THIS>
connect-timeout    = 5
```

`connect-timeout = 5` is defense against the steady-state iptables
lockdown: in steady state the substituter URL is unreachable
(`OUTPUT` chain default-deny), and without a connect timeout Nix
would block ~63 seconds per cache-miss derivation waiting for the
default TCP SYN retries. Five seconds is enough for Stage 0's
network to be honest about reachability and short enough that a
steady-state miss doesn't visibly stall.

### CI workflow that pushes closures on every `main` merge

`.github/workflows/cache-push.yml` runs on `push` to `main`:

1. Build `nix/images/builder-vm#packages.{x86_64,aarch64}-linux.default`
   on a Linux runner.
2. `cachix push mvm <result>` against the produced closure.
3. Optional follow-up: push the `nix/images/builder/` (dev-shell)
   closure too once we measure how much it adds to Stage 0 budget.

The workflow needs `CACHIX_AUTH_TOKEN` as a repo secret (one-time
user provisioning).

### What this does NOT help

- **Steady-state user-flake builds.** `mvm-builder-init` installs an
  iptables `OUTPUT` default-deny rule before `cmd.sh` runs in steady
  state. The substituter URL is present but unreachable; Nix falls
  back to local store (warm from Stage 0). Making the cache work in
  steady state would require either (a) opening the egress allowlist
  for `mvm.cachix.org` (ADR-047 surface growth) or (b) extending
  `mvm-egress-proxy` to gate the substituter fetch like it does for
  the installer fetches. Deferred to a separate plan if/when steady-
  state cache hits become a measured pain point.
- **Contributor edits to the builder VM flake / kernel / workspace
  `Cargo.lock`.** Input hash changes → cache miss → local rebuild.
  Intentional: preserves the CLAUDE.md "source-checkout never depends
  on mvm-published artifacts" invariant.
- **Bug fixes to mvmctl Rust code** that don't touch Nix derivations.
  No `nix build` happens — Cachix is irrelevant.

## One-time user provisioning

This plan ships placeholders. Before the cache becomes functional,
exactly four things have to happen on the user side (~10 minutes):

1. Sign up at https://app.cachix.org/ and create a public cache
   named `mvm` (open-source plan — free).
2. Run `cachix generate-keypair mvm`. Save the private key in a
   password manager; copy the printed **public** key.
3. Replace the placeholder `<USER_PROVISIONS_THIS>` in
   `crates/mvm-build/src/libkrun_builder.rs` with the public key.
   (Only one place — the cache-push workflow consumes the key
   through the `cachix/cachix-action` setup, not as a literal
   string.)
4. Add `CACHIX_AUTH_TOKEN` (from `cachix authtoken`) to the GitHub
   repo secrets at Settings → Secrets and variables → Actions.

Until step 3 is done the substituter line points at a key Nix won't
trust; signed closures from the (real) cache get rejected and Nix
falls back to source builds — same behavior as no Cachix at all,
just slightly more network traffic. Until step 4 the CI push job
fails closed.

## Workstreams (one PR each)

This plan splits into one PR because the pieces only make sense
together; reviewing them separately wastes everyone's time. The PR
contains:

- `specs/plans/89-cachix-substituter-stage0.md` — this file
- `crates/mvm-build/src/libkrun_builder.rs` — substituter + key + timeout
- `crates/mvm-build/src/libkrun_builder.rs` tests — locked
- `.github/workflows/cache-push.yml` — new
- `README.md` (or appropriate contributor doc) — one-time setup section

## Followups (not in this PR)

- Measure Stage 0 wall-clock with/without cache hit. Today's
  "estimated minutes → seconds" is a heuristic; a real number lives
  in `specs/runbooks/<runbook>.md`.
- Decide whether to push the `nix/images/builder/` (dev-shell)
  closure too.
- Decide whether to amend ADR-047 to allow steady-state cache hits.
- Decide whether to mirror to Attic (Hetzner self-hosted) if Cachix's
  open-source tier ever caps us.

## References

- CLAUDE.md §"Source-checkout builds never depend on mvm-published
  artifacts" — the constraint that bounds when the cache fires.
- ADR-002 — security claim 6 (pre-built dev image hash-verify) is
  the precedent for trusting binary closures over the network.
- ADR-047 — the iptables lockdown that prevents steady-state cache
  use; would need amendment to extend Cachix beyond Stage 0.
- Plan 87 / PR #360 — passt becoming the default networking layer
  (unblocked this plan; substituter HTTP works now).
