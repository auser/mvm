# ADR-060 — Single canonical builder VM image

**Status:** Proposed
**Date:** 2026-05-27
**Related:** ADR-046 (builder VM via libkrun), ADR-056 (Vz backend), ADR-057 (symmetric builder VM), Plan 101

## Context

Today the repo treats the builder path as two artifact lineages:

1. `nix/images/builder-vm/` — the Layer 1 builder appliance that runs
   `nix build`.
2. `nix/images/builder/` — the Layer 2 dev-shell image that
   contributors actually boot with `mvmctl dev up`.

That split was useful while the project was carving the builder VM away
from the old dev-image implementation, but it now creates the wrong
product shape for the workflow we actually want:

- contributors debug one VM while automation builds with another
- Stage 0/bootstrap logic has to reason about two image classes and two
  caches
- docs and CLI language keep repeating "Layer 1" vs "Layer 2"
- builder-VM changes are harder to validate interactively because the
  interactive environment is not the canonical builder image

The intended model is simpler: there should be one authoritative Linux
environment that both humans and automation use. "Dev shell" is a mode
of the builder VM, not a separate image lineage.

## Decision

Adopt a **single canonical builder VM image**.

- `nix/images/builder-vm/` remains the canonical image definition.
- "Dev shell" becomes **interactive builder mode**, not a separate
  rootfs lineage.
- `mvmctl dev up` boots the canonical builder VM image in interactive
  mode.
- build orchestration (`mvmctl build`, persistent builder jobs, Stage 0
  bootstrap) uses the same image in non-interactive mode.
- `nix/images/builder/` becomes a compatibility shim during migration
  and is then removed.

This ADR **supersedes ADR-046's "Two artifact layers, two acquisition
paths" section**. ADR-046 still owns the builder-VM transport and
distribution decisions; this ADR changes the artifact model above that
transport.

## Non-goals

- Changing the host-VMM selection policy from ADR-046 / ADR-056 /
  ADR-057.
- Reintroducing host Nix on the contributor or end-user path.
- Defining the final interactive UX in detail beyond "same image,
  different mode".

## Migration shape

The rollout is phased so bootstrap and cache semantics move without
breaking current contributors.

1. Keep `nix/images/builder-vm/` as the sole canonical image source.
2. Teach the builder image to support both:
   - non-interactive job mode (`mvm-builder-init`)
   - interactive developer mode (`dev shell`)
3. Point `mvmctl dev up` at the canonical builder image.
4. Convert Stage 0 / ur-seed / cache status code from "dev-image seed"
   terminology to "interactive builder image" terminology.
5. Remove `nix/images/builder/` once no code path requires it.

Plan 101 owns the implementation sequence.

## Consequences

### Positive

- One authoritative Linux environment for both humans and automation.
- Builder bugs become reproducible in the same VM contributors boot.
- Cache, docs, and CLI concepts get simpler: one builder image, two
  modes.
- Future hardening and slimming work applies to one rootfs lineage
  instead of two.

### Negative

- The canonical builder image will likely be somewhat larger than a
  pure build-only appliance.
- Stage 0/bootstrap work has to be retouched because several plans
  assume a distinct dev image exists.
- The migration touches user-facing terminology, cache status, and
  command behavior in many places.

### Neutral

- Persistent builder infrastructure still exists; it just runs the same
  image that interactive `dev up` uses.
- Backend selection (`libkrun` vs `vz` vs Linux builder policy) remains
  governed by the existing ADRs.

## Rejected alternatives

- **Keep separate builder and dev images.** Rejected because it bakes
  drift into the architecture and keeps the contributor-facing VM from
  being the canonical build substrate.
- **Make the dev image canonical instead.** Rejected because the
  builder-VM path already owns the bootstrap, cache, and release-image
  contract; folding the dev path into it is less disruptive than the
  reverse.
- **Only share packages, keep two rootfs artifacts.** Rejected because
  the conceptual split, bootstrap complexity, and cache duplication all
  remain.

## References

- [Plan 101](../plans/101-single-builder-image-rollout.md)
- [ADR-046](046-builder-vm-via-libkrun.md)
- [ADR-056](056-vz-backend.md)
- [ADR-057](057-symmetric-builder-vm.md)
