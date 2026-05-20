# Plan 92 — slim custom builder-VM kernel via `linuxManualConfig`

**Status:** drafted 2026-05-19, revised 2026-05-20. Initial
implementation landed on `worktree-plan-92-stock-kernel`. Validation
of the slim-kernel direction pending (the stock-kernel intermediate
direction was validated end-to-end through Stage 0 → steady-state
boot before being superseded — see "Revision history" below).
**Follows:** Plan 87 (`specs/plans/87-passt-virtio-net.md`) +
Plan 88 (`specs/plans/88-gvproxy-macos-backend.md`) — both moved
builder-VM networking onto virtio-net, removing the last reason
to vendor TSI kernel patches.
**Supersedes:** the parallel "slim builder-VM kernel config to a
microVM surface" attempt on `worktree-plan-91-alpine-impl`
(commit `e2cbf710`) — that approach kept the TSI patches and
the closure-copy module shipping. This plan deletes both.

## Problem

`cargo run -- dev up` was failing inside the builder VM during a
kernel compile — the build disk filled. Earlier `dev up` worked on
the same macOS dev machine. What changed?

`git log nix/images/builder-vm/kernel/default.nix` shows the
history in two commits:

- `c05f5666` — "builder-vm: TSI-patched kernel via vendored
  libkrunfw patches" — switched `kernelPkg` from
  `pkgs.linuxPackages.kernel` to
  `pkgs.linuxPackages.kernel.override { kernelPatches = …; }`.
- `e2cbf710` (worktree-only) — "kernel: slim builder-VM kernel
  config to a microVM surface" — added `structuredExtraConfig`
  disabling big subsystems on top of the patched kernel.

Patching the source invalidated nixpkgs' binary substituter
match, so the build dropped to from-source compile *inside the
builder VM*, against the full general-purpose nixpkgs default
config — thousands of `=m` drivers we never load. The
intermediate object tree exceeded the build disk's headroom.

But the deeper problem surfaced during the validation of the
"stock kernel + module closure-copy" intermediate direction (see
"Revision history"). Stock kernel ships hundreds of `=m` modules;
mvm-builder-init has to modprobe each one at the right time
(overlay before mount, vsock before socket, fuse + virtiofs
before mount, iptables tables before rule install), and mkGuest
has to closure-copy the matching `.ko` files into `/lib/modules/`.
The contract is a silent-failure surface — five different bugs
surfaced in one session, all variants of "the module wasn't
loaded when the consumer expected it."

## Decision

**Build a slim custom Linux 6.12 kernel via `pkgs.linuxManualConfig`,**
with every feature the builder VM needs flipped to `=y`
(built-in). `CONFIG_MODULES=n` — no module tree to ship, no
modprobe to orchestrate, no closure-walk in mkGuest to maintain.

The source of truth is the `enables` / `disables` lists in
`nix/images/builder-vm/kernel/default.nix`. The config file is
generated inside the derivation by `make tinyconfig` +
`scripts/config --enable/--disable` + `make olddefconfig`. No
`.config` file is checked in. No kernel patches are vendored.

### What the kernel actually contains

The minimum set needed for the builder VM:

- virtio bus + transports: VIRTIO, VIRTIO_PCI, VIRTIO_MMIO,
  VIRTIO_BLK, VIRTIO_NET, VIRTIO_CONSOLE, VIRTIO_FS, VSOCKETS,
  VIRTIO_VSOCKETS, VIRTIO_BALLOON, HW_RANDOM_VIRTIO.
- Filesystems: EXT4_FS, OVERLAY_FS, FUSE_FS, TMPFS, DEVTMPFS,
  PROC_FS, SYSFS.
- dm-verity: MD, BLK_DEV_DM, DM_VERITY (Claim 3 / Plan 25 W3).
- Namespaces + cgroups v2 + seccomp for the nix build sandbox.
- Net core + iptables-legacy + xt_owner + REJECT for the
  ADR-047 egress lockdown.
- Minimal NLS (UTF-8 + ASCII).

Explicitly disabled: MODULES, IPV6, DRM, SOUND, USB, WIRELESS,
BT, FB.

### Why slim, not stock + modules

Stock `pkgs.linuxPackages.kernel` + `mkGuest`'s closure copy
produced six distinct failure modes during Plan 92 validation:

| # | Symptom | Root cause |
|---|---------|------------|
| 1 | Validator false-negative on every fresh rootfs | `BUILDER_INIT_PATH` needle issue (independent of kernel choice) |
| 2 | `mount overlay /nix-merged: ENODEV` | overlay kernel module never modprobed |
| 3 | `socket(AF_VSOCK, …): EAFNOSUPPORT` | vsock kernel module never modprobed |
| 4 | `/sbin/udhcpc: No such file or directory` | mkGuest baked applets in `/bin/` only (independent of kernel choice) |
| 5 | Stage 0 OOMs on substitution | Rust binary compile closure size (independent of kernel choice) |
| 6 | `nix: libboost_url.so.1.87.0: cannot open shared object file` | mkGuest closure-walk didn't land transitive `.so` paths correctly |

Bugs 2 and 3 (modprobe race) and 6 (closure-walk fragility) are
directly caused by the stock+modules abstraction. Flipping to
slim eliminates them by construction: nothing to modprobe, no
module tree to walk.

Bugs 1, 4, 5 are kernel-independent and need fixing regardless.

### Why not `linuxPackages.kernel.override { structuredExtraConfig }`

That keeps the patches (or accepts the inheritance of stock's
hundreds of `=m` modules). Either way the cache miss + modprobe
contract stays. The `e2cbf710` worktree attempted this and was
superseded by this plan.

### Tradeoffs accepted

- **Contributor first-`dev up` compiles the kernel.** ~3-5 min on
  Apple Silicon; one-time per nixpkgs kernel pin. After that the
  nix store hash is stable and reused across all subsequent runs
  for that contributor. Subsequent contributors don't share the
  cache through `cache.nixos.org` (novel config), but within a
  contributor's machine the cost amortizes to zero.
- **Maintenance: `enables` / `disables` lists** against nixpkgs
  kernel version bumps. `make olddefconfig` reconciles dropped /
  renamed / added symbols automatically. Quarterly review at
  most.
- **IFD (`allowImportFromDerivation = true`)** slows pure-eval
  CI lanes. Tradeoff accepted — the alternative is vendoring the
  config, which violates the project's "no vendoring" preference.

## Implementation

### Done on `worktree-plan-92-stock-kernel`

**Kernel piece (slim):**

- [x] `nix/images/builder-vm/kernel/default.nix` —
      `pkgs.linuxManualConfig` over `make tinyconfig` + the
      `enables` / `disables` lists. No TSI patches. No kernel-
      module support (`CONFIG_MODULES=n`).
- [x] `nix/images/builder-vm/kernel/README.md` — describes the
      slim approach, the tradeoff (first-boot compile), the
      maintenance contract (add to `enables` / `disables`, run
      `olddefconfig`), and why no TSI patches.
- [x] Deleted the 22 vendored TSI patch series files + the 2
      stale `config-libkrunfw_*` files. No vendored content under
      `nix/images/builder-vm/kernel/`.

**Flake hookup:**

- [x] `nix/images/builder-vm/flake.nix`:
  - Imports `./kernel` for the slim kernel; drops the
    `pkgs.linuxPackages.kernel` reference and the
    `builderKernelModules` list.
  - `mkBuilderVmRootfs` no longer takes a `withKernel` parameter
    (no modules to optionally include); same rootfs whether the
    full image or the Stage 0 seed.
  - mkGuest call drops `kernel = ...` and `kernelModules = ...`
    — no module tree to copy.

**mvm-builder-init simplification:**

- [x] Removed the `modprobe overlay` + `modprobe vmw_vsock_virtio_transport`
      calls added in the stock-kernel intermediate direction.
      With everything `=y`, modprobe is a no-op; the calls were
      noise.

**Adjacent fixes (kernel-independent, kept):**

- [x] `crates/mvm-cli/src/commands/env/apple_container.rs` —
      `BUILDER_INIT_PATH` constant changed from `/sbin/mvm-builder-init`
      (22 bytes) to `mvm-builder-init` (16 bytes). The full path
      never appears as a contiguous byte sequence in an ext4 image
      (directory entries store basenames only).
- [x] `nix/lib/mk-guest.nix` — busybox applet symlinks now land
      in `/sbin/` as well as `/bin/`, so `mvm-builder-init`'s
      absolute-path call sites (`/sbin/udhcpc`,
      `/sbin/poweroff`) resolve. Kernel-independent.
- [x] `crates/mvm-cli/src/commands/env/apple_container.rs` —
      Stage 0 RAM bumped 8 → 24 GiB via `with_resources(4, 24576)`.
      Stage 0 still has to compile `mvm-builder-init` /
      `mvm-egress-proxy` (Rust binaries) via `buildRustPackage`;
      the rustc + workspace vendor closure doesn't fit in 8 GiB.
- [x] `crates/mvm-build/src/stage0/init.sh` — `/nix` tmpfs cap
      bumped 4 G → 20 G to match.

### Remaining

- [ ] **Validate `cargo run -- dev up` end-to-end with the slim
      kernel.** Stage 0 must compile the slim kernel (first run
      only; expected ~3-5 min), produce the rootfs + vmlinux, and
      the steady-state builder VM must boot and reach the same
      checkpoint the stock-kernel intermediate direction reached.
- [ ] **If the kernel fails to boot for a missing symbol,**
      add it to `enables` and rebuild. The failure mode is
      visible at `console.log` (panic with the missing-feature
      message). One-line fix.

### Out of scope (handed to Plan 93)

- The boost.so failure inside the steady-state builder VM —
  Plan 93 deletes the curated mkGuest rootfs for the builder VM
  in favor of an Alpine-based image, resolving the class of bug
  entirely.

### Doc sweeps

- [ ] `specs/adrs/055-passt-virtio-net.md` lines 100-103
      ("TSI patches in the kernel become dead code from mvm's
      perspective" — they're now actually gone).
- [ ] `specs/plans/87-passt-virtio-net.md` W6 — mark the patch
      removal item complete; point at this plan.
- [ ] `crates/mvm-build/src/libkrun_builder.rs:78` doc comment
      references "the VM image's TSI kernel" — drop the TSI
      qualifier.
- [ ] `crates/mvm-libkrun/src/sys.rs:400-414` comment mentions
      `nix/images/builder-vm/kernel/patches/` — drop.
- [ ] `crates/mvm-builder-init/src/main.rs:331` mentions "the
      in-repo TSI-patched kernel" — drop the qualifier.
- [ ] **Decide on `extract_bundled_kernel()`.** Stub today,
      returns `Err("not yet wired")`, no live caller. Probably
      delete (~80 lines).

## Maintenance contract

When the kernel needs a change:

1. **Add a built-in feature.** Append its short Kconfig symbol
   (no `CONFIG_` prefix) to the `enables` list in
   `nix/images/builder-vm/kernel/default.nix`. `make
   olddefconfig` pulls in transitive deps on the next build.
2. **Remove a feature.** Append to `disables`. If `olddefconfig`
   pulls it back in, the parent that depends on it also needs to
   come off.
3. **Bump the kernel version.** Edit the `pkgs.linux_6_12`
   reference (or follow nixpkgs' rename if the LTS pin moves).
   `make olddefconfig` reconciles dropped / renamed / added
   symbols automatically.
4. **Introspect what `olddefconfig` produced.** Temporarily
   expose `configfile` from `default.nix` as a flake output and
   `nix build` it.

When `mvm-builder-init` needs a new subsystem:

1. **Add the relevant Kconfig symbol** to `enables`. If it's
   already in the stock tinyconfig + dependent on a parent we
   already enable, `olddefconfig` will pull it in automatically.
2. **No code change in `mvm-builder-init`** — no modprobe to
   sequence, no race to manage. The subsystem is present from
   PID-1 entry.

## Risk register

- **R1 — Slim config rejects a feature `olddefconfig` should have
  pulled in.** Surfaces as a kernel compile error or boot panic
  with a clear message naming the missing symbol. Fix is one line
  in `enables`.
- **R2 — First-boot compile cost regresses the contributor
  experience past acceptable.** Mitigation: dev image release
  ships the prebuilt kernel artifact (already implied by the
  release pipeline). Contributors building from source pay the
  cost once per kernel-pin bump.
- **R3 — `linuxManualConfig` rejects the generated config.** It
  requires `version` / `modDirVersion` / `src` to agree; we pass
  all three from `pkgs.linux_6_12` so drift is impossible by
  construction.
- **R4 — IFD slows CI.** `allowImportFromDerivation = true` is
  required because nixpkgs needs the config text at eval time
  to call `linuxManualConfig`'s internals. Tradeoff accepted —
  the alternative is vendoring the config.
- **R5 — A required Kconfig symbol drifts between nixpkgs kernel
  versions.** Surfaces at the next kernel-pin bump as an
  `olddefconfig` warning or a boot failure. Fix is to update
  `enables`. Bounded blast radius.

## Revision history

- **2026-05-19** — initial draft: slim kernel via
  `linuxManualConfig`.
- **2026-05-20 (morning)** — user revised to "revert to stock
  `pkgs.linuxPackages.kernel`" to win the binary cache hit.
  Implementation landed; validated end-to-end through Stage 0 →
  steady-state boot on
  `worktree-plan-92-stock-kernel` commit `bbffc166`.
- **2026-05-20 (afternoon)** — user reverted the direction back
  to slim after observing the cascade of bugs the stock+modules
  abstraction produced (5 bandages in one session — see
  "Decision" §"Why slim, not stock + modules"). This is the
  current direction.

## Cross-references

- **Files touched on this branch:**
  - `nix/images/builder-vm/kernel/default.nix` (restored, slim
    via `linuxManualConfig`)
  - `nix/images/builder-vm/kernel/README.md` (restored)
  - `nix/images/builder-vm/flake.nix` (slim kernel import, no
    `builderKernelModules`, no `withKernel` parameter)
  - `nix/lib/mk-guest.nix` (sbin busybox symlinks — kept,
    kernel-independent)
  - `crates/mvm-builder-init/src/main.rs` (modprobe overlay+vsock
    additions removed; slim kernel has them `=y`)
  - `crates/mvm-cli/src/commands/env/apple_container.rs`
    (BUILDER_INIT_PATH needle + Stage 0 RAM bump — kept,
    kernel-independent)
  - `crates/mvm-build/src/stage0/init.sh` (tmpfs cap — kept,
    kernel-independent)
  - `crates/mvm-build/src/libkrun_builder.rs` (TSI doc-comment
    cleanup)
- **Deleted on this branch:**
  - 22 vendored TSI patch files under `kernel/patches/`
  - 2 stale `config-libkrunfw_{aarch64,x86_64}` files
- **Reverted on this branch:**
  - `e2cbf710` ("kernel: slim builder-VM kernel config to a
    microVM surface") — kept the patches, kept the cache miss.
- **Next plan:** Plan 93 — Alpine steady-state builder VM
  (eliminates the mkGuest curated rootfs for the builder VM,
  resolves the boost.so class of issue).
- **Security analysis:** ADR-058 — security model under the
  Alpine builder VM (Plan 93's security envelope; this plan's
  kernel choice is the integrity-backstop boundary that ADR-058
  relies on via dm-verity).
