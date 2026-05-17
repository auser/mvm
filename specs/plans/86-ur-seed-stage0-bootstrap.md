# Plan 86 — Ur-seed Stage 0 bootstrap (closing the Plan 77 W5 contributor gap)

**Status:** drafted 2026-05-17, awaiting review.

## Problem

Plan 77 W5 added a hard seed contract: `bootstrap_builder_vm_image_via_dev_image_stage0` requires the seed dev image to contain `/sbin/mvm-builder-init`. On contributor hosts whose dev image pre-dates the W5 commit, every recovery path now fails closed:

1. Cached dev image fails the W5 byte-scan contract check.
2. Stage 0 refuses to proceed without a contract-compliant seed.
3. Published-prebuilt download is correctly compile-time disabled per W4.
4. Building a fresh dev image requires a working builder VM, which requires Stage 0, which requires a fresh dev image. Catch-22.

Concrete reproduction on this host: `~/.mvm/dev/current/rootfs.ext4` (May 15, 736 MB) has zero occurrences of the byte sequence `/sbin/mvm-builder-init`. The directory `~/.mvm/dev/builds/` holds 69 `.staging-*` orphans from crashed rebuild attempts. `cargo run -- dev up` exits with `source checkout builder VM cache is missing and no local dev image cache was found … that contains /sbin/mvm-builder-init`.

The deeper issue: Plan 77 W5 assumes the dev image is the only possible Stage 0 seed type, which silently couples the bootstrap path to a steady-state artifact. ADR-046 wants contributors to see flake changes reflected in the next `dev up`; it does not authorize requiring contributors to already have a working build to make a working build.

## Goal

Make `cargo run -- dev up` work on a contributor host with **no** cached dev image and **no** runtime download of an mvm-published artifact, while preserving:

- The ADR-046 invariant ("contributor edits to `nix/images/builder-vm/flake.nix` reflect in the next `dev up`").
- The no-host-Nix rule (CLAUDE.md).
- The no-automatic-prebuilt-builder-VM-download rule (memory `feedback_no_prebuilt_builder_vm_artifact.md`).

## Design — ur-seed as Stage –1

Introduce a third layer **upstream** of the builder VM:

```
ur-seed (vendored/explicit-fetch) → builder VM (built from source flake) → dev image (built from source flake)
```

The ur-seed is a minimal aarch64-linux env (initramfs CPIO + libkrunfw-bundled kernel) that exists only to run `nix build` against `nix/images/builder-vm/flake.nix`. It is independent of every flake in the repo; modifying any flake invalidates the relevant cache but does not invalidate the ur-seed.

### Ur-seed contents

| Component               | Source                                    | Size  | Refresh cadence |
| ----------------------- | ----------------------------------------- | ----- | --------------- |
| Linux kernel            | `libkrunfw.5.dylib` (already used)        | —     | libkrunfw bumps |
| `mvm-builder-init.musl` | release CI cross-build from this repo     | ~5 MB | release cuts    |
| `busybox-static`        | pinned upstream release + sha256          | ~1 MB | manual bumps    |
| `nix-portable`          | pinned `DavHau/nix-portable` + sha256     | ~80 MB | manual bumps    |

Total payload: ~85 MB per arch. Packaged as a single tarball per arch.

### Acquisition: Shape C (explicit, opt-in)

Per user direction, the ur-seed is **never** downloaded automatically by `dev up`. Instead:

- New verb `mvmctl dev fetch-ur-seed [--arch aarch64|x86_64]` is the one and only call site that touches the network. It downloads the tarball + sha256 manifest from a documented mirror, verifies, extracts to `~/.cache/mvm/ur-seed/<arch>/`.
- `dev up` Stage 0 fails closed with a clear `run 'mvmctl dev fetch-ur-seed' once, then retry` message when the ur-seed cache is empty.
- Mirror = GitHub releases of this repo, populated only at release cuts (consistent with the user's stance: "the only time I want that built in public on github is when we make a new release"). Tarball name: `ur-seed-<arch>.tar.gz` at the tagged release.

### Stage 0 lookup order (after this plan)

`bootstrap_builder_vm_image` Stage 0 path tries seeds in priority order:

1. Contract-compliant dev image at `~/.mvm/dev/current/` (existing W5 path).
2. Contract-compliant dev image at `~/.mvm/dev/prebuilt/v*/` or `~/.mvm/dev/builds/*/`.
3. **NEW:** ur-seed at `~/.cache/mvm/ur-seed/<arch>/`.
4. Hard-fail with fetch-ur-seed hint.

Order matters: real dev images carry the user's actual Nix store and produce faster builds than ur-seed cold starts. Ur-seed is the safety net, not the default.

### Seed contract check upgrade (independent improvement)

Replace `file_contains_bytes(rootfs, b"/sbin/mvm-builder-init")` (a brittle byte-scan that yields both false negatives and false positives) with a real ext4 inode check via the `ext4` crate. Look up `/sbin/mvm-builder-init`, verify it's a regular file or symlink to one, verify mode has `IXUSR`. The ur-seed path uses initramfs (CPIO) so the contract check needs an analogous path for CPIO; the implementation will dispatch by extension.

This change matters even without the ur-seed: it closes the W5 contract's false-positive hole (any file in the rootfs containing the literal byte sequence passes).

## Workstreams

### W1 — ur-seed assembly pipeline

- `nix/ur-seed/flake.nix` — pure Nix derivation that produces the ur-seed tarball for each `aarch64-linux` / `x86_64-linux`. Only ever evaluated by release CI; contributor builds never touch it.
- `nix/ur-seed/pins.json` — sha256 pins for `nix-portable` upstream URL and `busybox-static` upstream URL.
- `nix/ur-seed/README.md` — what this is, when to bump pins, how release CI consumes it.
- Release workflow lane builds the tarballs and attaches them to the GitHub release as `ur-seed-<arch>.tar.gz` + `ur-seed-<arch>.tar.gz.sha256`.

### W2 — `mvmctl dev fetch-ur-seed`

- New `DevCommand::FetchUrSeed { arch: Option<String>, mirror: Option<Url> }` in `crates/mvm-cli/src/commands/env/dev.rs`.
- Fetches `ur-seed-<arch>.tar.gz` + sha256, verifies hash, atomically extracts to `~/.cache/mvm/ur-seed/<arch>/` (staging dir + rename, matching Plan 77 W2 pattern).
- Default mirror = the latest GitHub release for the current `CARGO_PKG_VERSION`. `--mirror` accepts an arbitrary URL for air-gapped relays.
- Audit emits `UrSeedFetched { arch, version, sha256_prefix }` / `UrSeedFetchFailed { stage, reason }`.

### W3 — Stage 0 fallback wiring

- Extend `BuilderVmBootstrapAction` with `BuildFromSourceViaUrSeed { flake_dir, ur_seed_dir }`.
- `resolve_builder_vm_bootstrap_action` returns the new variant when no contract-compliant dev image exists but `~/.cache/mvm/ur-seed/<arch>/` is present and verified.
- New `bootstrap_builder_vm_image_via_ur_seed_stage0` mirrors the dev-image variant but loads the ur-seed CPIO + libkrunfw kernel.
- Same advisory-lock + staging-dir + audit pattern as Plan 77 W2/W3.

### W4 — Seed contract check rewrite

- Add `ext4` crate dep (read-only ext4 reader).
- New `validate_stage0_seed_contract_ext4` opens the rootfs, walks to `/sbin/mvm-builder-init`, asserts regular file or symlink chain landing on executable.
- New `validate_stage0_seed_contract_cpio` for initramfs ur-seed.
- `validate_stage0_seed_contract` dispatches by extension. Existing byte-scan code becomes a fast-path heuristic to skip the heavier check when the bytes are clearly absent.
- New `validate_stage0_seed_contract_cpio_test` and `validate_stage0_seed_contract_ext4_test` cover present/absent/symlink-chain/non-executable cases.

### W5 — Orphan cleanup surfacing

- `mvmctl dev cache inspect --json` reports `orphan_staging_count` for both `~/.mvm/dev/builds/` and `~/.cache/mvm/builder-vm/`.
- `mvmctl cache prune` (already wired per Plan 77 W2) sweeps both; verify and document.
- `dev up` warning when orphan count exceeds a threshold (e.g. 10) suggesting `cache prune`.

### W6 — Plan 86 tests + docs

- Unit tests per W4 + W2 + W3.
- New ADR-054 ("Ur-seed Stage –1 bootstrap layer") with rationale, alternatives considered (Shapes A/B), and the trade-off where mvm-builder-init in the ur-seed is release-frozen.
- Update CLAUDE.md security-model section #8 to note ur-seed is part of the trusted host-side bootstrap chain (signed sha256, never auto-fetched).
- Update SPRINT.md.

## Non-goals

- **Auto-downloading the ur-seed from `dev up`.** Hard policy boundary.
- **Vendoring the ur-seed in-tree.** Considered (Shapes A/B in the design discussion) and rejected: 85 MB per arch in git history is the wrong trade-off vs. one explicit `fetch-ur-seed` call.
- **Replacing nix-portable with something we own.** Bounded bridge per memory `feedback_replace_over_workaround.md`; nix-portable is a pinned external, not vendored.
- **Fixing the in-VM dev-image build crash** (the `linux-6.12.87.drv` failures filling `~/.mvm/dev/builds/`). Tracked separately once Stage 0 is unstuck; likely DNS / virtio-fs / resolv.conf in the builder VM per Plan 72 W5.D bullets 6 + 10.
- **mvm-builder-init in the ur-seed is release-frozen.** Contributor edits to `crates/mvm-builder-init/` reflect in the steady-state builder VM (rebuilt every `dev up`) but not in the ur-seed bootstrap layer. Acceptable trade-off documented in ADR-054.

## Success criteria

1. `mvmctl dev fetch-ur-seed` succeeds on a fresh contributor host with no prior mvm state.
2. `mvmctl dev up` after step 1 succeeds end-to-end on a host with no cached dev image and no cached builder VM image.
3. `cargo test --workspace` clean; `cargo clippy --workspace -- -D warnings` clean.
4. Modifying `nix/images/builder-vm/flake.nix` and re-running `dev up` produces a fresh builder VM image reflecting the edit (ADR-046 still holds).
5. Modifying `crates/mvm-builder-init/` and re-running `dev up` produces a builder VM whose in-guest init reflects the edit (steady-state path).
6. No path from `dev up` reaches `download_builder_vm_image` or any other network fetch.

## Order of operations

W1 and W2 are mostly independent. W3 depends on W2 (needs a place to write the ur-seed cache). W4 is independent of W1–W3 and should land first as a separate small PR — it improves the existing contract check on its own. W5 can land any time. W6 lands with each workstream's PR.

Suggested PR sequence: W4 → W2 → W3 → W1 (release plumbing) → W5 → W6 (docs).
