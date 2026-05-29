# ADR-064 — Single builder/dev image with host-built mvm binaries

**Status:** Proposed (2026-05-29).
**Supersedes:** the dev-image-vs-builder-VM-image split established by
ADR-046 §"Two artifact layers, two acquisition paths" — see §Migration.
**Related (do not change in this ADR):** the SDK end-user transparency
story (`crates/mvm-sdk/src/compile/flake.rs`, ADR-0007), and ADR-046's
source-checkout invariant (preserved unchanged here).

## Context

`mvmctl dev up` from a source checkout today drives this chain:

1. mvmctl spawns Stage 0 (libkrun + libkrunfw kernel + Alpine + nix).
2. Stage 0 runs `nix build path:/work/nix/images/builder-vm#packages.<system>.default`.
3. The flake calls `rustPlatform.buildRustPackage` for `mvm-builder-init`,
   `mvm-egress-proxy`, and (via the dev-image flake) a second copy of
   `mvm-builder-init`.
4. Nixpkgs translates `Cargo.lock` into ~290 per-crate `fetchCrate`
   derivations.
5. Each `fetchCrate` curls `https://crates.io/api/v1/crates/<name>/<v>/download`
   with no User-Agent and gets HTTP 403 under crates.io's data-access
   policy. The build collapses.

Two further smells were uncovered while debugging this:

- **Two flakes producing nearly the same artifact.** `nix/images/builder-vm/`
  produces the headless builder VM; `nix/images/builder/` produces the
  "dev shell" image. The only structural reason the dev image existed
  separately was a circular Stage-0 bootstrap: when the builder-VM cache
  was empty, mvmctl could boot the dev image as PID 1 (`mvm-builder-init`)
  and ask it to build the builder VM. The dev image carries its own
  copy of `mvm-builder-init` solely for that fallback.
- **mvm is rebuilding its own product on every contributor's `dev up`.**
  `mvm-builder-init`, `mvm-egress-proxy`, and (out of scope here)
  `mvm-guest-agent`, `mvm-runner` are mvm's binaries — not user code.
  They should be *inputs* to the builder-VM image build, the same way
  the Linux kernel and busybox are inputs. Today they are translated
  through nixpkgs's curl-based crate fetcher every time, which is both
  the wrong tool (cargo handles this trivially with a proper UA) and
  the wrong responsibility (the builder VM exists to build microVMs,
  not to recompile mvm's source code).

The crates.io 403 problem will be fixed upstream in days (PR
NixOS/nixpkgs#525067 merged 2026-05-28; backport PR #525491 open). A
nixpkgs overlay would unblock today's `dev up`. But shipping that
overlay would entrench both smells above. We have a chance to address
the underlying shape instead.

## Decision

1. **Collapse the dev image and the builder VM image into a single flake
   with two attributes.** `nix/images/builder/flake.nix` is deleted.
   `nix/images/builder-vm/flake.nix` becomes the only flake, producing:
   - `packages.<system>.default` — the headless builder VM. Boots
     for `mvmctl build`, `mvmctl run`, and every other command that
     needs an internal builder. Exits when its job completes.
   - `packages.<system>.dev` — the same base image plus the
     interactive layer: `bashInteractive`, `cargo`, the Rust toolchain
     matching the workspace, an editor, motd, and PTY-over-vsock
     console plumbing. Boots only for `mvmctl dev up`, which always
     attaches a shell — there is no headless `dev` variant by design.

   The Stage 0 chicken-and-egg dance (`bootstrap_builder_vm_image_via_
   dev_image_stage0`) dissolves because there is no longer a separate
   dev image to bootstrap from. Only the Alpine + libkrunfw Stage 0
   path remains.

2. **mvm's own Linux binaries become inputs to the flake, produced by
   host cargo.** A new contract:

   - **Single source of truth: `nix/lib/mvm-host-binaries.nix`** — a
     small Nix attrset declaring each binary mvm needs in a Linux
     image and where it lives:
     ```nix
     {
       mvm-builder-init = {
         cargo_package = "mvm-builder-init";
         install_path = "/sbin/mvm-builder-init";
         mode = "0755";
       };
       mvm-egress-proxy = {
         cargo_package = "mvm-egress-proxy";
         install_path = "/sbin/mvm-egress-proxy";
         mode = "0755";
       };
     }
     ```
     Pure data, parseable from Nix natively (the flake's primary
     consumer). The Rust side (mvmctl / mvm-build) does **not** parse
     the Nix file at runtime — CLAUDE.md forbids mvmctl from invoking
     host Nix in any form. Instead, a small mirrored constant lives
     alongside the host-binaries module in Rust, and a CI lane
     (`xtask check-mvm-host-binaries-mirror` or similar) asserts the
     two stay in sync. The file is small (a handful of entries) and
     changes only when an mvm binary is added or renamed, so the
     mirror discipline is cheap.

   - **Two consumers, no manual sync.** mvmctl reads the manifest,
     runs `cargo zigbuild --target aarch64-unknown-linux-gnu --release
     -p <cargo_package>` per entry (or plain `cargo build` for native
     Linux contributors), stages outputs to a content-addressed temp
     dir, exposes the dir path. The flake reads the same manifest,
     iterates entries under `--impure` using `MVM_HOST_BIN_DIR` to
     find the staged binaries, and generates the corresponding
     `extraFiles` entries automatically — no per-binary `extraFiles`
     lines hand-written.

   - **No `rustPlatform.buildRustPackage` for mvm's binaries** in the
     builder-VM flake (or in the deleted dev-image flake). The
     `fetchCrate` path stops being on `dev up`'s critical path
     entirely, regardless of what crates.io's data-access policy does.

   - **Contributor toolchain delta:** one `brew install zig` (the
     dependency `cargo-zigbuild` needs). Probed by `mvmctl doctor`
     with install hints, same surface as the existing libkrun trio.
     Native Linux contributors require nothing new.

3. **The dev VM is the builder VM with interactivity.** Both attrs build
   from the same kernel, base userland, networking, mvm binaries,
   security posture, audit chain. The `dev` attr adds packages and
   wires a TTY; nothing about the underlying VM model changes. There
   is no headless dev VM and no interactive builder VM.

## Why cargo zigbuild

Three reasons, in order of how much each actually matters:

1. **Crates with C in `build.rs` actually compile.** `ring`,
   `aws-lc-rs`, `openssl-sys`, etc. typically fail under Homebrew's
   `aarch64-elf-gcc` because there is no glibc sysroot. Zig ships its
   own multi-arch C toolchain with a real glibc sysroot.
2. **Single Homebrew install (`brew install zig`), no Docker.** Uses
   cargo's native `target/` directory, so incremental compile shares
   state with the contributor's normal `cargo build`. Second `dev up`
   rebuilds only what they edited.
3. **Explicit glibc version pinning.** `--target aarch64-unknown-linux-
   gnu.2.17` lets us pin the glibc version to match what nixpkgs's
   base userland ships, avoiding the "binary requires newer glibc
   than the rootfs has" foot-gun.

Alternatives considered:

- **`cross`** (Docker-based) — slower startup, separate target dir
  from cargo's, Docker dependency on macOS contributors.
- **Hand-rolled Homebrew cross-toolchain** — high per-contributor
  setup tax, breaks on C-in-`build.rs` crates, no glibc sysroot.
- **Build inside a Linux container/VM** — slower inner loop (separate
  target/), pushes complexity into mvmctl, runs against the
  responsibility split established here (the dev/builder VM's job is
  *building microVMs*, not recompiling mvm).
- **Cargo inside the builder VM, bootstrap-staged** — recreates the
  Stage 0 chicken-and-egg shape in a new place.

## Architecture / data flow

### Layers (with sharp boundaries)

- **Host (mvmctl + cargo)** — produces mvm's Linux binaries. Inputs:
  workspace source, `Cargo.lock`, `mvm-host-binaries.nix`. Outputs:
  staged binary dir, content-addressed.
- **Stage 0 (libkrun + libkrunfw kernel + Alpine + nix)** — unchanged
  in role. New inputs: the staged binary dir (mounted at `/mvm-bins`
  via virtio-fs) and `MVM_HOST_BIN_DIR=/mvm-bins` in env. Output:
  builder-VM image artifacts (`vmlinux` + `rootfs.ext4` + cmdline.txt
  + manifest.json). The flake never compiles Rust.
- **Builder VM (the produced image)** — one image, two attrs as
  defined in §Decision.

### `mvmctl dev up` end-to-end

Steps in **bold** are new or substantially changed; the rest match
today's shape.

1. User runs `mvmctl dev up` (always interactive).
2. mvmctl detects source-checkout mode (workspace + flake present).
3. **mvmctl parses `nix/lib/mvm-host-binaries.nix`** to get the cargo
   packages to build and their install paths.
4. **mvmctl runs `cargo zigbuild --target aarch64-unknown-linux-gnu
   --release -p <pkg>`** (or `cargo build` for native Linux) per
   entry. Outputs land in the host's `target/aarch64-unknown-linux-
   gnu/release/`. Cargo's incremental compile means second `dev up`
   rebuilds only changed crates.
5. **mvmctl stages binaries into a content-addressed dir** at
   `~/.cache/mvm/host-bins/<hash>/{mvm-builder-init, mvm-egress-proxy}`.
   The hash is one of the cache keys for the builder-VM image.
6. mvmctl boots Stage 0 with two virtio-fs shares: `/work` (workspace)
   and **`/mvm-bins`** (the staged dir from step 5).
7. Stage 0 runs `nix build path:/work/nix/images/builder-vm#packages.
   <system>.dev --impure` (`.default` for non-`dev` commands).
   `MVM_HOST_BIN_DIR=/mvm-bins` set in env.
8. **The flake reads `mvm-host-binaries.nix`, iterates entries, and
   generates `extraFiles` entries pointing at `/mvm-bins/<name>` with
   the declared `install_path` and `mode`.** No `rustPlatform`. No
   `fetchCrate`.
9. Nix produces `vmlinux` + `rootfs.ext4`; Stage 0 powers down.
10. mvmctl extracts to `~/.cache/mvm/builder-vm/<system>/`, keyed on
    (workspace SHA, host-bins hash, flake SHA).
11. mvmctl boots the dev VM via whichever backend the host selects
    (libkrun / Vz / Apple Container per the existing
    `MVM_BUILDER_BACKEND` rules).
12. mvmctl opens a PTY-over-vsock console into the running VM.

For `mvmctl build`, `mvmctl run`, and other non-`dev` commands: same
path, but step 7 targets `packages.<system>.default`, step 11 boots
headless, no step 12.

### Cache invalidation

- `mvm-host-binaries.nix` changes → host-bins hash changes → cache key
  changes → rebuild.
- Source files in a configured cargo package change → cargo's
  incremental detection rebuilds → host-bins hash changes → cache
  key changes → flake re-bakes (most of the closure stays cached in
  the persistent `/nix-store`).
- Workspace SHA changes elsewhere → cache key changes → rebuild.
- Nothing changed → cargo no-op, mvmctl boots straight from cache.

## Component-level diff

### New

- `nix/lib/mvm-host-binaries.nix` — manifest (see §Decision).
- A small Rust module in `crates/mvm-build/` that parses the manifest,
  invokes cargo (zigbuild on macOS, native on Linux), stages outputs,
  computes the content-addressed hash, returns the dir path.

### Modified

- `nix/images/builder-vm/flake.nix` — substantially rewritten:
  - Two attrs: `packages.<system>.default` and `packages.<system>.dev`.
  - No `rustPlatform.buildRustPackage` for mvm binaries.
  - Reads `mvm-host-binaries.nix` and `MVM_HOST_BIN_DIR` under
    `--impure`; generates `extraFiles` mechanically.
  - The `dev` attr adds `bashInteractive`, `cargo`, Rust toolchain,
    editor, motd, PTY-over-vsock console wiring.
- `nix/lib/workspace-filter.nix` — drops `nix/images/builder` from
  its list of consumers (3 → 2).
- `crates/mvm-cli/src/commands/env/apple_container.rs` — collapses the
  source-checkout dispatch: the `find_dev_image_flake` /
  `ensure_source_checkout_dev_image` /
  `resolve_source_checkout_dev_image` branches go away (no separate
  dev image flake to find). `cmd_dev_libkrun` / `cmd_dev_vz` call
  into the new host-binaries module before invoking nix, and target
  the `dev` attr.
- `crates/mvm-build/src/pipeline/dev_build.rs` — `dev_build_with_
  builder_vm` mounts the staged binary dir and passes
  `MVM_HOST_BIN_DIR` into the in-VM nix invocation.
- `crates/mvm-cli/src/doctor.rs` — adds a probe for `cargo-zigbuild`
  on macOS contributors with an install hint
  (`brew install zig` + `cargo install cargo-zigbuild`, or the
  appropriate equivalents). Native Linux contributors pass trivially.
- `CLAUDE.md` "Host dependencies (macOS)" — adds the zigbuild
  requirement for source-checkout contributors.

### Deleted

- `nix/images/builder/flake.nix` — gone.
- The four `rustPlatform.buildRustPackage` call sites for `mvm-
  builder-init` / `mvm-egress-proxy` across the builder-vm and
  builder flakes.
- `find_dev_image_flake`, `ensure_source_checkout_dev_image`,
  `resolve_source_checkout_dev_image`, `bootstrap_builder_vm_image_
  via_dev_image_stage0` in `apple_container.rs`.
- The `mvmBuilderInitFor` helper duplicated between the two flakes —
  only one consumer survives, and it's not `rustPlatform`-based.

### Touched only mechanically

- Tests referencing the deleted flake or dispatch helpers — updated to
  the single-flake shape or removed if redundant.
- `nix/images/runtime-overlay/flake.nix` — left intact (out of scope;
  it still uses `rustPlatform` for `mvm-runner` and the guest agent).
  The mechanism defined here is reusable by a later spec that converts
  runtime-overlay to consume `mvm-host-binaries.nix`; doing so is
  explicitly *not* required for this spec.

## Error handling

- **`cargo-zigbuild` / `zig` missing on macOS:** mvmctl doctor probes
  for `zig` and `cargo-zigbuild` and emits a clear install hint. The
  `dev_build` path fails fast with the same hint if either is missing
  at use-time.
- **Cargo build fails for any configured package:** mvmctl surfaces
  cargo's stderr directly with the failing package name in the outer
  error context.
- **`MVM_HOST_BIN_DIR` not set when the flake is evaluated:** the
  flake errors loudly with the contract documented inline (a
  contributor running `nix build` directly without going through
  mvmctl gets a useful message, not a Nix evaluation failure 12
  layers deep).
- **A binary declared in `mvm-host-binaries.nix` not present in
  `MVM_HOST_BIN_DIR`:** the flake errors with the missing name + the
  staged-dir path, so the cause is locatable.

## Testing

- **Unit tests in `crates/mvm-build/`:** parse `mvm-host-binaries.nix`
  fixture, assert structure; given a stub staged dir, assert mvmctl
  passes the right `MVM_HOST_BIN_DIR` and `--impure` to the in-VM
  build invocation.
- **Flake-side fixture test:** a test that feeds a hand-crafted
  `MVM_HOST_BIN_DIR` (with placeholder binaries) into `nix build` and
  asserts the produced rootfs.ext4 has files at the declared install
  paths with the declared modes.
- **End-to-end smoke (CI macOS lane):** runs the real cargo zigbuild
  step + Stage 0 + flake, asserts the produced builder-VM image has
  `/sbin/mvm-builder-init` and `/sbin/mvm-egress-proxy` with SHA-256
  matching the cargo outputs.
- **`mvmctl doctor` test:** asserts the zigbuild probe runs and
  emits the expected hint when zig is absent.
- **Tests touching the deleted dev-image dispatch helpers** — updated
  to reflect the collapse (most likely removed; the helpers are gone).

## Out of scope

- **Converting `nix/images/runtime-overlay/flake.nix` and the guest
  agent's build to use this mechanism.** The mechanism is reusable;
  doing the conversion is a follow-up spec. Keeps blast radius small.
- **The SDK's `mkGuest` adoption of the same contract** for end-user
  microVMs (so end-user `mvmctl compile` becomes
  `fetchCrate`-independent). Same reasoning — separate spec, separate
  PR. The mechanism here is designed to be adopted there later
  without changes.
- **Release pipeline changes** to ensure `mvm-builder-init` and
  `mvm-egress-proxy` ship as standalone artifacts that
  end-user-mode mvmctl can download into `MVM_HOST_BIN_DIR`. Today's
  release workflow already cross-compiles to aarch64-unknown-linux-
  gnu; ensuring the binaries are uploaded as named release assets is
  a separate, small change. Called out as an assumption this spec
  relies on but does not enforce.
- **The `builder_vm_timeout()` value** and the partial-cache promotion
  bug observed during debugging this. Both are pre-existing,
  unrelated, and out of scope. Calling them out so future readers
  know they were noticed and parked.
- **Any change to `mvm.toml` shape or the SDK's end-user transparency
  story.** Reserved for the SDK's own specs.

## Consequences

### Positive

- **`fetchCrate` exits mvm's hot path.** crates.io's User-Agent policy,
  rate limits, and future surprises stop being a `dev up` concern.
- **Single image, single source of truth.** The dev/builder split
  dissolves. The Stage-0 chicken-and-egg fallback (boot dev image to
  build builder VM) dissolves with it.
- **Faster contributor inner loop.** Cargo's incremental compile on
  the host beats anything that uses a separate target directory.
  Edit `mvm-builder-init`, `dev up` again, only that crate rebuilds.
- **Less surface area in mvmctl.** Three dispatch helpers
  (`find_dev_image_flake`, `ensure_source_checkout_dev_image`,
  `resolve_source_checkout_dev_image`) go away. One bootstrap path
  remains, not two.
- **Single producer for mvm's binaries.** Source-checkout uses
  cargo; end-user uses release artifacts. Same `MVM_HOST_BIN_DIR`
  contract on the flake side. Same paths inside the rootfs.
- **Aligns with existing release infrastructure.** `release.yml`
  already cross-compiles to `aarch64-unknown-linux-gnu`; the host
  path uses the same target triple and toolchain logic.

### Negative

- **New host dependency for macOS source-checkout contributors:**
  `zig` + `cargo-zigbuild`. One brew install + one cargo install,
  probed by doctor. Native Linux contributors are unaffected.
- **Cargo glibc-version pinning becomes part of the contract.** The
  binaries must be compatible with the rootfs's glibc. The pin lives
  in mvmctl's cargo invocation and must move when the rootfs's glibc
  moves.
- **The Rust mirror of `mvm-host-binaries.nix` introduces a sync
  burden.** Two-file edit when adding a new binary; CI check
  asserts they match. Cost is small because the manifest is small
  and changes rarely.
- **Existing release artifacts must publish the binaries by name.**
  We rely on this for the end-user path, even though we don't change
  the release pipeline in this spec. Called out as an assumption.

## Migration

The deletion of `nix/images/builder/flake.nix` is the high-blast
change. Specifically:

- Anything that called `find_dev_image_flake()` returns `Err` because
  the file is gone; the dispatch above it is removed in the same PR
  so the call no longer exists.
- The Stage 0 path `bootstrap_builder_vm_image_via_dev_image_stage0`
  is deleted. Only `bootstrap_builder_vm_image_via_root_dir_stage0`
  remains (the Alpine + libkrunfw path that's been the source-checkout
  default since Plan 92/95).
- `~/.mvm/dev/current/` (the cached dev image, separate from the
  builder-VM cache) becomes a stale concept. A best-effort cleanup
  on first `dev up` after the upgrade isn't required; the dir simply
  stops being read.

## Verification

- `mvmctl dev up` from a clean macOS source checkout, with `zig` +
  `cargo-zigbuild` installed: cargo cross-compiles, Stage 0 produces
  the builder-VM image, dev VM boots with a working interactive
  shell. No `crates.io` reachability required at any point during
  Stage 0's `nix build`.
- `mvmctl build` (or any non-`dev` command requiring the builder VM)
  from the same setup: same Stage 0 path, builder VM boots headless,
  job completes, VM exits.
- Edit a file in `mvm-builder-init/src/` and re-run `mvmctl dev up`:
  cargo's incremental rebuilds only that crate; the staged-dir hash
  changes; Stage 0 re-bakes the rootfs with the new binary; the
  rest of the closure stays cached.
- Audit chain (claims 8 / 9 / 10): builder VM image's audit
  emission and verification are unaffected because the rootfs
  contents, paths, and binaries' SHA-256s are still deterministic
  given a fixed workspace + flake + cargo lock.

## References

- ADR-046 — Builder VM via libkrun. This ADR amends the "Two artifact
  layers" rule by collapsing the dev image into the builder VM image.
- Plan 72 — Builder VM via libkrun (the implementation of ADR-046).
- Plan 92, 95 — Alpine + libkrunfw Stage 0; the path this spec
  doubles down on as the only Stage 0 path going forward.
- `crates/mvm-sdk/src/compile/flake.rs` and ADR-0007 — the end-user
  flake generation path, which adopts the same `MVM_HOST_BIN_DIR`
  contract in a future spec.
- [rcarmo/pve-microvm](https://github.com/rcarmo/pve-microvm) — a
  reference implementation for assembling `vmlinux` + `rootfs.ext4`
  pairs that's worth borrowing from during the implementation plan.
- NixOS/nixpkgs PR #525067 — the upstream `fetchCrate` fix
  (static.crates.io) that motivated this redesign. The overlay was
  considered as a workaround and explicitly rejected in favor of
  removing the dependency on `fetchCrate` for mvm's own binaries.
