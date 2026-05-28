# Plan 101 — Single builder image rollout

**Sprint:** 56
**ADR:** [ADR-060](../adrs/060-single-builder-image.md)
**Status:** Proposed

## Goal

Collapse the current `builder-vm` vs `dev-shell` split into one
canonical builder VM image. After this plan, `mvmctl dev up` boots the
same image that build automation and persistent builder jobs use; the
difference is runtime mode, not rootfs lineage.

## Success criteria

- `nix/images/builder-vm/` is the only canonical builder/dev image
  source.
- `mvmctl dev up` boots the builder VM image in interactive mode.
- the booted interactive VM can build microVMs using the same toolchain
  and store layout as the non-interactive builder path.
- no Stage 0 or cache-status path depends on a separate
  `nix/images/builder/` rootfs lineage.
- docs and CLI language stop describing Layer 1 builder vs Layer 2 dev
  as separate image classes.

## Waves

- [ ] **W0 — Inventory and terminology freeze.** Audit every place that
  treats `nix/images/builder/` as a distinct image class. Lock the new
  terms:
  - "builder VM image" = canonical artifact
  - "interactive builder mode" = what `dev up` boots
  - "persistent builder" = lifecycle mode, not a different image

- [ ] **W1 — Interactive mode inside `builder-vm`.** Extend the
  canonical image so it can boot either:
  - job mode via `mvm-builder-init`
  - interactive mode with the developer-facing shell/agent surface
  Decide whether this is an alternate init, a mode flag read by PID 1,
  or a second entry binary.

- [ ] **W2 — Route `mvmctl dev up` to the canonical image.** Change the
  CLI/runtime path so `dev up` boots `nix/images/builder-vm/` rather
  than building and booting `nix/images/builder/`. Keep
  `nix/images/builder/` as a compatibility wrapper or alias while this
  wave lands.

- [ ] **W3 — Stage 0/bootstrap/cache migration.** Replace assumptions
  that a separate dev image exists:
  - Stage 0 seed selection
  - ur-seed bootstrap wording
  - cache provenance / status reporting
  - source-checkout detection that currently keys off
    `nix/images/builder/flake.nix`

- [ ] **W4 — Persistent-builder and dev-path convergence.** Ensure the
  persistent builder lifecycle and interactive dev lifecycle operate on
  the same rootfs and state contract. `dev shell` / `dev status` /
  `persistent-builder submit` must agree on which image is canonical.

- [ ] **W5 — Remove the compatibility lineage.** Delete
  `nix/images/builder/` once no runtime path requires it. Clean up
  helper functions, comments, tests, and docs that still describe a
  separate Layer 2 dev image.

- [ ] **W6 — Verification and documentation close-out.** Run the full
  verification matrix, update ADR references, and close the sprint item
  only after the user-facing model and cache-status output are aligned
  with ADR-060.

## Critical files

- `nix/images/builder-vm/flake.nix`
- `nix/images/builder/flake.nix`
- `crates/mvm-builder-init/src/main.rs`
- `crates/mvm-cli/src/commands/env/dev.rs`
- `crates/mvm-cli/src/commands/env/apple_container.rs`
- `crates/mvm-cli/src/commands/build/persistent_builder.rs`
- `specs/adrs/046-builder-vm-via-libkrun.md`
- `specs/adrs/054-ur-seed-stage0-bootstrap.md`
- `specs/adrs/056-vz-backend.md`
- `specs/adrs/057-symmetric-builder-vm.md`
- `specs/SPRINT.md`

## Risks

- **Bootstrap churn.** Stage 0 and ur-seed currently assume a distinct
  dev-image seed in multiple places.
- **Image growth.** The single canonical image may gain some developer
  tooling that a build-only appliance would not need.
- **Mode confusion during rollout.** As long as `nix/images/builder/`
  exists as a wrapper, old and new terminology can drift again.
- **Backend parity.** Interactive mode has to work under every builder
  backend that can currently boot the builder VM.

## Verification

- `cargo run -- dev up` boots the canonical builder image and presents
  the interactive environment.
- The same booted VM can build a representative microVM without
  switching to a second image lineage.
- `cargo run -- persistent-builder start` and `cargo run -- dev up`
  report the same underlying image provenance.
- Cache/status output no longer refers to a separate dev image when run
  from a source checkout.
- `cargo test --workspace`, `cargo check --workspace`, and Linux-side
  builder verification stay green after the cutover.
