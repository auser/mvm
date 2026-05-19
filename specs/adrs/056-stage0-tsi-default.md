# ADR-056 — Stage 0 defaults to TSI; steady-state builder VM keeps gvproxy/passt

**Status:** accepted 2026-05-19, implements Plan 91. Amends ADR-054 §"Stage 0 fallback order" and ADR-055 §"Cross-platform backends" — the per-OS gvproxy/passt default applies to the steady-state builder VM only; Stage 0 (ur-seed) defaults to TSI.

## Context

Plan 87 / ADR-055 flipped the libkrun networking default from TSI →
passt (Linux) / gvproxy (macOS) because TSI doesn't support the
modern HTTP behaviour `nix build` relies on against external
substituters (HTTP/2 multiplexing, HTTPS redirect chains,
substituter availability probes). The default flip applies to
*every* `apply_networking_mode` call site, including the Stage 0
boot path that uses an ur-seed rootfs as the bootstrap image.

This is correct for the **steady-state** builder VM (every
`mvmctl build` / `mvmctl deps install` against an arbitrary user
flake — those routinely fetch from `cache.nixos.org` and friends).
It is **not** correct for **Stage 0**:

- Stage 0's job is one thing only: `nix build path:/work#packages.$arch-linux.default`
  against the in-repo `nix/images/builder-vm/flake.nix`.
- ADR-054 §"Ur-seed shape" guarantees the ur-seed rootfs ships
  *the full runtime package closure mirroring the steady-state
  builder VM* pre-staged under `/nix/store`. No external substituter
  fetch is required to complete the Stage 0 build.
- Plan 86's original ur-seed-init shell script (still present at
  `/sbin/ur-seed-init` in every ur-seed rootfs — see
  `nix/ur-seed/flake.nix`) explicitly uses TSI-mode nix-portable.
  Plan 86 / ADR-054 demonstrated end-to-end that TSI is sufficient
  for the ur-seed's specific build.

The cost of inheriting the gvproxy/passt default into Stage 0 is
real. The ur-seed's release-frozen `mvm-builder-init` carries any
networking-stack bugs forward into every contributor's bootstrap
path (PR #382 — busybox 1.36.x udhcpc no longer auto-ups the
interface, so Stage 0 hangs on `udhcpc: sendto: Network is down`
because `eth0` is administratively `DOWN` after virtio-net probe).
Contributors hit a hard hang before the per-OS gvproxy/passt
default even gets a chance to be useful — the build never reaches
the point where substituter access would matter.

`MVM_NETWORKING=tsi` exists as an opt-out escape hatch, but
"contributors must remember to set an env var to make the bootstrap
not hang" is a poor default. The right move is to recognise that
Stage 0 and the steady-state builder VM have meaningfully different
network requirements and treat them as such.

## Decision

**Stage 0 (libkrun + ur-seed) defaults to TSI. Steady-state builder
VM keeps the per-OS gvproxy/passt default from ADR-055.**

The dispatch happens in `default_networking_mode(is_stage0: bool)`:

| `is_stage0` | `MVM_NETWORKING` | Result |
| --- | --- | --- |
| `true` (Stage 0) | unset | **TSI** |
| `true` (Stage 0) | `tsi` / `passt` / `gvproxy` | explicit override honoured |
| `false` (steady-state) | unset | per-OS default (gvproxy on macOS, passt on Linux) |
| `false` (steady-state) | `tsi` / `passt` / `gvproxy` | explicit override honoured |

`MVM_NETWORKING` retains its full meaning in both contexts — it's
an explicit-override escape hatch, never silently overridden by the
context default. Contributors who need to debug Stage 0 with real
networking can set `MVM_NETWORKING=gvproxy` and get the previous
behaviour back.

The Stage 0 signal is `LibkrunBuilderVm::image_override.is_some()`
— it's the same signal that already tells the rest of `run_build`
"boot from a caller-supplied bootstrap image rather than the cached
builder VM image". `apply_networking_mode` reads this flag from
the surrounding `LibkrunBuilderVm` context and passes it through.

## Why this is safe

- Stage 0 only ever builds the in-repo builder-vm flake against
  the bundled closure. ADR-054 §"Ur-seed shape" pins this guarantee
  (the ur-seed flake mirrors the steady-state builder's runtime
  packages and stages them into `/nix/store` at rootfs-build time).
  No substituter fetch is needed in the Stage 0 happy path.
- If a contributor edits the builder-vm flake to add a dependency
  the ur-seed doesn't pre-stage, the Stage 0 nix build will fail
  *with a clear "missing path" error*, not a TSI HTTP corruption
  failure. The remedy is unambiguous: either add the dep to the
  ur-seed's `urSeedPackages` set, or set `MVM_NETWORKING=gvproxy`
  for that build.
- The steady-state builder VM is unchanged. It rebuilds every
  `dev up` from the local source, so the eth0-up fix from PR #382
  takes effect immediately for it — gvproxy / passt remain the
  right defaults there for arbitrary user flakes against arbitrary
  substituters.
- Runtime workload microVMs are unaffected. They're vsock-only per
  ADR-002 and never go through `apply_networking_mode`.

## Alternatives considered

- **Keep gvproxy/passt as the Stage 0 default, fix the ur-seed
  binary.** This is the obvious answer once the eth0-up bug is
  fixed in PR #382 — *and* the ur-seed is rebuilt and re-imported
  on every contributor host. The release-frozen nature of the
  ur-seed (`feedback_no_ur_seed_publish_during_dev`) makes that a
  release-cycle problem, not a routine bug fix. Until then,
  contributors are blocked. TSI as the Stage 0 default removes
  the dependency entirely.
- **Make TSI the universal default for libkrun.** Rejected by ADR-055
  precisely because the steady-state builder VM and arbitrary user
  flakes need real network. The split-default approach gets both
  contexts right.
- **Require contributors to set `MVM_NETWORKING=tsi` themselves.**
  Discoverability-poor; doesn't compose well with `mvmctl doctor`
  output and per-OS install hints. The right defaults should make
  the common path Just Work.

## Consequences

- **`mvmctl dev up` on a fresh contributor host works without any
  env var** even when the cached ur-seed predates PR #382's fix.
  Stage 0 boots in TSI mode, the eth0-up bug never executes,
  `nix build` against the pre-staged closure runs to completion,
  the builder-vm image lands in the cache. From the second run on,
  Stage 0 is skipped entirely (cache hit) and the steady-state
  builder VM picks up its own (already-fixed) `mvm-builder-init`.
- **`MVM_NETWORKING` semantics are unchanged.** The override behaves
  the same in both contexts.
- **`mvmctl doctor` per-OS gateway probe is unchanged.** It tests
  the steady-state networking path — which is still
  gvproxy/passt — and is unaffected by Stage 0's TSI default.
- **Adds a tiny shape change to two `pub fn`s** in
  `crates/mvm-build/src/libkrun_builder.rs`:
  `default_networking_mode(is_stage0: bool)` and
  `resolve_networking_mode(is_stage0: bool)`. Both are
  workspace-internal (no external callers per
  `grep -r 'resolve_networking_mode\|default_networking_mode'`).

## References

- ADR-054 §"Ur-seed shape" — pre-staged closure guarantee.
- ADR-055 §"Cross-platform backends" — why steady-state needs real network.
- Plan 87 / Plan 88 — the original TSI → passt/gvproxy flip.
- Plan 91 — implementation tracker.
- PR #382 (`fix(builder-init): bring eth0 up before udhcpc`) — fixes
  the underlying bug for the steady-state path. ADR-056 protects
  Stage 0 *until* a fresh ur-seed carrying #382 ships on the
  release cadence.
- Memory: `feedback_no_ur_seed_publish_during_dev` — release mirror
  only moves on prod release cuts.
