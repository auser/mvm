# ADR-054 — Ur-seed Stage –1 bootstrap layer

**Status:** accepted 2026-05-18, implements Plan 86.

## Context

Plan 77 W5 added a hard seed contract to Stage 0
(`bootstrap_builder_vm_image_via_dev_image_stage0`): the dev image
serving as the Stage 0 seed must contain `/sbin/mvm-builder-init` and
declare it in its `manifest.json`. The contract closes a real
kernel-panic class (Plan 77 §"Why W5 + W6 were added") but creates a
catch-22 for contributor hosts:

- The dev image is built **by** Stage 0 (`mvmctl dev up` → Plan 77 W1
  via the dev-image-as-seed path).
- The dev image needs `/sbin/mvm-builder-init` to satisfy the W5
  contract for **future** Stage 0 runs.
- Contributors who upgraded `mvmctl` after the W5 commit landed have
  a pre-W5 dev image in their cache. That image lacks
  `/sbin/mvm-builder-init`, so the W5 check rejects it. Building a
  W5-compliant image requires Stage 0, which requires a W5-compliant
  seed image. No path forward.

Plan 77 W4 correctly gates the `download_builder_vm_image` fallback
behind an off-by-default feature so contributor builds can't pull a
prebuilt — this matches the
`feedback_no_prebuilt_builder_vm_artifact.md` invariant. The result is
that on a contributor host with only a pre-W5 dev image,
`mvmctl dev up` is permanently dead-ended.

ADR-046 §"Two artifact layers, two acquisition paths" explicitly
authorises this stance: the contributor path must not depend on
mvm-published artifacts, and a contributor edit to
`nix/images/builder-vm/flake.nix` must reflect in the next `dev up`.
The W5 catch-22 violates that invariant in practice — there is no
"next `dev up`" possible from the stuck state.

## Decision

Introduce a **Stage –1** bootstrap rootfs, the **ur-seed**, that sits
upstream of the builder VM and is independent of every other flake in
the repo. The chain becomes:

```
ur-seed (built once, installed explicitly)
    ↓
builder VM (built locally from nix/images/builder-vm/flake.nix)
    ↓
dev image (built locally from nix/images/builder/flake.nix)
```

### Ur-seed shape

`nix/ur-seed/flake.nix` produces a single tarball per arch
(`ur-seed-<arch>-linux.tar.gz`) containing:

| Artifact         | Source                                                        |
| ---------------- | ------------------------------------------------------------- |
| `rootfs.ext4`    | mkfs.ext4-formatted image built in the flake                  |
| `vmlinux`        | TSI-patched kernel from `nix/images/builder-vm/kernel/`       |
| `manifest.json`  | Stage 0 seed contract metadata (`image_kind=ur-seed`)         |
| `cmdline.txt`    | Kernel cmdline (informational; Stage 0 uses its own)          |

The rootfs ships:
- `busybox-static` (POSIX shell + utilities).
- `mvm-builder-init` cross-compiled to `aarch64-unknown-linux-musl`
  at `/sbin/mvm-builder-init` (the W5 contract path).
- The same runtime package closure the steady-state builder VM uses
  (`bash`, `coreutils`, `nix`, `e2fsprogs`, `iptables`, `util-linux`,
  …) staged under `/nix/store` and symlinked into `/usr/local/bin`
  + `/sbin`.
- The kernel module tree at `/lib/modules/<kver>/` (virtio-fs, fuse).

The rootfs and kernel are bundled into the tarball alongside the
manifest + cmdline; the host extracts them atomically to
`~/.cache/mvm/ur-seed/<arch>/`.

### Acquisition policy (Shape C — explicit only)

Per `feedback_no_prebuilt_builder_vm_artifact.md`, **`mvmctl dev up`
NEVER fetches the ur-seed automatically**. Two explicit paths:

1. **`mvmctl dev fetch-ur-seed [--arch …] [--mirror …]`** —
   download from the documented release mirror (GitHub release for
   the running `mvmctl` version by default). SHA-256 verified before
   atomic-install.
2. **`mvmctl dev import-ur-seed --from <tarball> [--sha256 <path>]`** —
   air-gapped install from a local file (release CI output, a
   sibling machine, or a manually-built tarball).

The release mirror is populated only when a release is explicitly
cut. Until then, contributors with no prior mvm state use
`import-ur-seed` against a manually-built tarball.

### Stage 0 fallback order

`bootstrap_builder_vm_image_via_*_stage0` selection:

1. Builder-VM cache hit → no Stage 0 needed.
2. Contract-compliant dev image at `~/.mvm/dev/{current,prebuilt/v*,builds/*}/`
   → dev-image Stage 0 (existing Plan 77 W1 path).
3. **NEW:** ur-seed at `~/.cache/mvm/ur-seed/<arch>/` → ur-seed Stage 0.
4. Hard fail with actionable `fetch-ur-seed`/`import-ur-seed` hint.

The ur-seed kernel is preferred over any other local kernel because
it is guaranteed TSI-patched (libkrun's AF_INET sockets require it —
Plan 72 W5.D bullet 10) and version-matched to the rootfs's module
tree.

### Trade-offs

- **The ur-seed's `mvm-builder-init` is release-frozen.** Contributor
  edits to `crates/mvm-builder-init/` reflect in the steady-state
  builder VM (rebuilt every `dev up`) but **not** in the ur-seed.
  This is a small ergonomic cost; in exchange the bootstrap path is
  trivially reproducible from a fixed artifact set.
- **The ur-seed's kernel is shared with the builder VM's TSI kernel
  package** (`nix/images/builder-vm/kernel/default.nix`). Contributor
  edits to the kernel package invalidate the ur-seed too — same
  rebuild lever as the builder VM cache. That alignment is desirable;
  the kernel is the load-bearing piece for libkrun network parity.
- **Tarball size ~190 MiB per arch (compressed).** Kernel modules
  dominate. Acceptable for an explicit-fetch artifact; not acceptable
  for a vendored-in-repo blob (Shape B in the Plan 86 discussion was
  rejected on this basis).
- **Adds `nix-portable` and `proot` as transitive concepts.** Plan 86's
  v1 used nix-portable; v2 dropped it in favour of the full runtime
  package closure for simplicity. The bounded-bridge memory note still
  applies — if we ever decide to ship a slimmer ur-seed, nix-portable
  is the natural pivot.

## Alternatives considered

- **Shape A: vendor the ur-seed via git-lfs.** Rejected: 85+ MiB per
  arch in repo history is a permanent tax. Contributors without
  git-lfs hit a partial-fetch failure mode that's hard to diagnose.
- **Shape B: vendor the ur-seed as raw bytes in-tree.** Rejected for
  the same reason as A, minus the git-lfs dep. Repo bloat is
  permanent.
- **Modify the W5 contract check to accept pre-W5 dev images.**
  Rejected: re-opens the kernel-panic class Plan 77 W5 closed
  (a contract-stale seed silently violates the boot contract). The
  ur-seed satisfies the W5 contract by carrying
  `/sbin/mvm-builder-init` directly.
- **Implement `extract_bundled_kernel()` to pull a TSI kernel from
  `libkrunfw.dylib`'s `.rodata`** (referenced in the
  `reference_libkrun_gotchas.md` memory). Tabled until the kernel
  patches in `nix/images/builder-vm/kernel/patches/` need to drift
  meaningfully from libkrunfw upstream; for now sharing the in-repo
  TSI package is simpler and version-coherent with the builder VM
  output.

## Consequences

- **Closes the Plan 77 W5 catch-22.** New contributors install the
  ur-seed once (`mvmctl dev fetch-ur-seed` or `mvmctl dev import-ur-seed`),
  then `mvmctl dev up` works end-to-end.
- **Adds a release-artifact obligation.** Each cut release must
  publish `ur-seed-<arch>-linux.tar.gz` + `.sha256` alongside the
  existing dev-image artifacts. Until that pipeline lands, the
  `--from <tarball>` path is the only acquisition route.
- **No change to the `mvmctl dev up` happy path** once the
  builder-VM cache is populated. The ur-seed is a one-shot bootstrap
  asset; it's not consulted on subsequent runs.

## Security model addendum

Security claim 6 (CLAUDE.md §"Security model" — "Pre-built dev image
is hash-verified") is extended to cover ur-seed acquisition:
`fetch-ur-seed` and `import-ur-seed` both verify a SHA-256 sidecar
before atomic-install. The mirror URL is the same GitHub releases
trust root as the dev image. The release CI must produce the
`ur-seed-*.tar.gz.sha256` sidecar alongside the tarball.

## References

- Plan 86 — `specs/plans/86-ur-seed-stage0-bootstrap.md`
- Plan 77 — `specs/plans/77-stage0-bootstrap-via-dev-image.md` (the
  W5 contract this addresses)
- Plan 72 — `specs/plans/72-builder-vm-via-libkrun.md` (libkrun
  builder VM, W5.D fix list — the catalog of "what breaks at each
  layer" that informed the ur-seed contents)
- ADR-046 — `specs/adrs/046-builder-vm-via-libkrun.md` (the
  two-artifact-layers invariant)
- ADR-002 — `specs/adrs/002-microvm-security-posture.md` (Claim 6)
- Memory `feedback_no_prebuilt_builder_vm_artifact.md` — the
  contributor-host policy this ADR honours.
