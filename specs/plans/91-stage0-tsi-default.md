# Plan 91 — Stage 0 networking defaults to TSI

**Owner:** Ari
**Status:** in progress 2026-05-19
**Tracks:** ADR-056 (Stage 0 vs steady-state networking defaults).
**Unblocks:** `mvmctl dev up` on every contributor host whose ur-seed predates PR #382.

## Goal

Stage 0 (libkrun-backed boot from an ur-seed image) defaults to
TSI mode. Steady-state builder VM keeps the per-OS gvproxy/passt
default from ADR-055. `MVM_NETWORKING` remains the explicit
override in both contexts.

Drops one env-var requirement (`MVM_NETWORKING=tsi`) for every
contributor on every fresh `dev up` until a release cycle ships a
fresh ur-seed carrying PR #382's eth0-up fix.

## Why

Documented in full in ADR-056. Short version:

- Stage 0's bundled-closure design (ADR-054) means it doesn't need
  real network. TSI is sufficient.
- The steady-state builder VM and arbitrary user flakes do need
  real network. gvproxy/passt remain the right defaults there.
- The current "everything inherits the gvproxy/passt default"
  behaviour from Plan 87/88 wedges Stage 0 against the
  release-frozen ur-seed's buggy `mvm-builder-init` (PR #382).
  TSI sidesteps the dependency entirely.

## Scope

**In scope (this plan):**

1. `default_networking_mode(is_stage0)` and `resolve_networking_mode(is_stage0)`
   in `crates/mvm-build/src/libkrun_builder.rs` — split-default per
   ADR-056's table.
2. Both `apply_networking_mode` call sites in `run_build` and
   `run_shell_job` pass `self.image_override.is_some()` as the
   Stage 0 hint.
3. Update the in-tree `MVM_NETWORKING` warn-fallback log line to
   name the chosen default explicitly (so a contributor sees
   `falling back to TSI (stage 0)` vs `falling back to gvproxy
   (per-host default)` and doesn't have to guess which one fired).
4. Tests for both contexts × all four `MVM_NETWORKING` states.

**Out of scope:**

- `mvmctl doctor` rework — the existing per-OS gateway probe tests
  the steady-state path and is unchanged.
- Any change to passt or gvproxy themselves.
- A fresh ur-seed release (see
  `feedback_no_ur_seed_publish_during_dev`).
- Runtime microVM networking. Vsock-only per ADR-002, unaffected.

## Implementation

One file, ~30 net-LOC. Function signatures shift but no public
contract changes — `grep -r 'resolve_networking_mode\|default_networking_mode'`
finds zero callers outside the file itself.

### Signature change

```rust
pub fn default_networking_mode(is_stage0: bool) -> NetworkingPreference {
    if is_stage0 {
        // Stage 0 builds a single in-repo flake against a pre-staged
        // closure (ADR-054 §"Ur-seed shape"). No external substituter
        // fetch needed; TSI is sufficient and sidesteps the
        // release-frozen ur-seed's `mvm-builder-init` (ADR-056).
        NetworkingPreference::Tsi
    } else if cfg!(target_os = "macos") {
        NetworkingPreference::Gvproxy
    } else {
        NetworkingPreference::Passt
    }
}

pub fn resolve_networking_mode(is_stage0: bool) -> NetworkingPreference {
    // `MVM_NETWORKING` always wins, in both contexts. Stage 0
    // default only kicks in when the env var is unset.
    match std::env::var("MVM_NETWORKING") … {
        Some("tsi") => NetworkingPreference::Tsi,
        Some("passt") => NetworkingPreference::Passt,
        Some("gvproxy") => NetworkingPreference::Gvproxy,
        None | Some("") => default_networking_mode(is_stage0),
        Some(other) => { /* warn + fallback to default_networking_mode(is_stage0) */ }
    }
}
```

### `apply_networking_mode` plumbing

Take `is_stage0: bool` and forward to `resolve_networking_mode`.
Both call sites in `run_build` and `run_shell_job` pass
`self.image_override.is_some()` (the existing "this is a Stage 0
boot" signal).

## Test plan

Unit tests in the existing `tests` module:

- `resolve_networking_mode_steady_state_defaults_match_host_os` — both target_os arms.
- `resolve_networking_mode_stage0_defaults_to_tsi` — `is_stage0 = true`, env unset, expect TSI on every host.
- `resolve_networking_mode_env_overrides_in_stage0` — `is_stage0 = true`, env = `gvproxy`, expect gvproxy (proves the explicit override wins).
- `resolve_networking_mode_env_overrides_in_steady_state` — existing test, adapted.
- `default_networking_mode_stage0_is_tsi` — pure logic.

All gated on `crate::TEST_ENV_LOCK` (or local mutex) so parallel
env mutation doesn't race.

## Manual verification

On a contributor host with a pre-PR-#382 ur-seed in
`~/.cache/mvm/ur-seed/<arch>/`:

```sh
rm -f ~/.cache/mvm/builder-vm/nix-store-*.img    # if a previous stale image exists
cargo run --bin mvmctl -- dev up
```

Expected: Stage 0 boots in TSI, `mvm-builder-init`'s broken
udhcpc-eth0 path errors immediately with ENODEV (no eth0 in TSI)
and continues (setup_network is non-fatal), the nix build of
the builder-vm flake completes against the bundled closure, the
builder-vm image lands in cache, the second `dev up` skips
Stage 0 entirely. No `MVM_NETWORKING` env var required.

Cross-check with the old behaviour:

```sh
MVM_NETWORKING=gvproxy cargo run --bin mvmctl -- dev up
```

Expected: Stage 0 uses gvproxy (override honoured), hits the same
hang documented in PR #382 because the cached ur-seed's
`mvm-builder-init` is the release-frozen broken one. Proves the
env-var override still works and Stage 0's TSI default is the
only thing protecting the user from the bug.

## Migration

None. The new default is strictly more permissive on Stage 0 (a
boot that hangs becomes a boot that succeeds) and identical on
the steady-state path. No external API surface changes.

## Risk

- **Stage 0 nix build fails on a missing substituter path.**
  Surface: a contributor edits `nix/images/builder-vm/flake.nix`
  to add a dep the ur-seed doesn't pre-stage, Stage 0 tries to
  fetch it in TSI mode, fetch fails. Mitigation: error is "missing
  path", not "HTTP corruption" — the remedy is to add the dep to
  the ur-seed's `urSeedPackages` or temporarily set
  `MVM_NETWORKING=gvproxy` for that one build. Easy to document.
- **Future Plan that needs network in Stage 0.** This plan doesn't
  preclude that — set `MVM_NETWORKING=gvproxy` or extend
  `default_networking_mode` with a third context. The split is
  semantic, not structural.

## References

- ADR-056 — decision record.
- ADR-054 — ur-seed bundled closure.
- ADR-055 — original TSI → gvproxy/passt flip.
- PR #382 — `mvm-builder-init` eth0-up fix (Stage 0 default of TSI
  is a coverage layer until that fix ships in a fresh ur-seed).
- Memory: `feedback_no_ur_seed_publish_during_dev`.
