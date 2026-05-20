# Plan 91 — Stage 0 root-dir fallback: replace broken nix-portable asset + add end-to-end CI gate

**Status:** drafted 2026-05-19, awaiting prioritization (no scheduled sprint).
**Tracks:** [#416](https://github.com/tinylabscom/mvm/issues/416).
**Follows:** Plan 72 (`specs/plans/72-builder-vm-via-libkrun.md`),
the Stage 0 root-dir dispatch shipped in #414 / #415.

## Problem

The Stage 0 root-dir fallback that #414 wired in is structurally
sound — kernel boots, `krun_set_root` works, network and virtio-fs
mounts are correct — but it cannot complete a Nix build, because
the upstream asset it pins is the wrong platform.

`crates/mvm-build/src/stage0.rs` pins:

```rust
pub const NIX_PORTABLE_AARCH64: BootstrapAsset = BootstrapAsset {
    cache_filename: "nix-portable",
    url: "https://github.com/DavHau/nix-portable/releases/download/v012/nix-portable-aarch64",
    sha256_hex: "af41d8defdb9fa17ee361220ee05a0c758d3e6231384a3f969a314f9133744ea",
    mode: 0o755,
};
```

The bytes at that URL hash exactly to the pinned digest, **and the
bytes are a macOS Mach-O arm64 binary, not a Linux aarch64 ELF**.
Cross-checked across v010–v013; DavHau's release consistently
uploads a macOS binary under the `nix-portable-aarch64` name. The
Linux VM tries to `exec` the binary and the shell returns `exit
127` ("not an executable"). The dispatch then bails with `Stage 0
supervisor exited with status 127`.

This bug is dormant on hosts where `~/.mvm/dev/current/` is
populated — the dev_image Stage 0 dispatch is the default and works
unaffected. It activates whenever that directory is missing, which
is every fresh contributor host and any host where the dev image
has been cleared (e.g. `mvmctl cleanup` aggressive, `rm -rf
~/.mvm/dev`, restoring from a fresh laptop).

Repro:

```sh
mv ~/.mvm/dev/current ~/.mvm/dev/current.shelved   # force fallback
cargo run -- dev up                                # → exit 127
mv ~/.mvm/dev/current.shelved ~/.mvm/dev/current   # restore
```

## Why this is more than swapping a URL

Three coupled issues:

1. **There is no known trustworthy Linux aarch64 `nix-portable`
   release.** The DavHau release is the only one we've located,
   and inspecting v010–v013 the situation looks consistent across
   tags. We need an alternative provenance — either build it from
   source ourselves, or identify a different upstream that ships
   actually-Linux binaries.
2. **`x86_64` may have the same issue.** We only verified the
   `aarch64` asset because that's the macOS Apple Silicon path.
   The Linux x86_64 host story routes through the same constant
   table (`ASSETS_X86_64` doesn't exist yet; Plan 72 W2 noted the
   intent to add it). Whatever we pin for aarch64 needs an x86_64
   sibling.
3. **The Stage 0 contract assumes a daemon-less Nix.** `init.sh`
   invokes `nix-portable nix build …`. If we move away from
   nix-portable (e.g. ship a full nix + nix-daemon in the Stage 0
   root dir) the trade-off is image size and complexity vs. asset
   provenance. nix-portable is the obvious choice because its
   whole pitch is "no daemon, single static binary," but the
   provenance gap is real.

## Decision criteria

A fix must satisfy all of:

- **Reproducible byte-identity**: the asset we ship has a hash
  pinned in source. `mvm_build::stage0::verify_sha256` already
  checks this; the fix doesn't change that contract.
- **Source of trust**: either an mvm-controlled artifact (build it
  in CI, sign it, publish it) or a third party whose release
  hygiene we can audit (must include a Linux ELF under a
  Linux-specific name).
- **No host Nix dependency**: per CLAUDE.md §"Host Nix is never
  used by mvmctl", the Stage 0 *bootstrap* itself cannot lean on
  host Nix. That rules out "build nix-portable as a Nix
  derivation inside `nix/images/builder-vm/`" for the bootstrap
  case — that's circular.
- **CI gate**: a new test must prove the asset can actually run
  inside a Linux VM. The validation gap that let the bug land
  ("nix-portable starts" was tested; "nix-portable exits 0" was
  not) closes here.

## Options

### Option A — Build nix-portable from source on a Linux CI runner

- Add a CI job (`build-nix-portable`) that runs on `ubuntu-latest`
  + `ubuntu-24.04-arm`, clones `github.com/DavHau/nix-portable` at
  a pinned tag, builds it inside the runner, uploads the artifacts
  as an mvm release asset alongside `dev-image`.
- `crates/mvm-build/src/stage0.rs` pins the **mvm-published**
  artifact URL + hash, not DavHau's.
- Per-arch entries (`NIX_PORTABLE_AARCH64`, `NIX_PORTABLE_X86_64`).
- Trust boundary: mvm release pipeline + signing manifest, same
  as `download_dev_image` (ADR-002 §W5.1).

Cost: one new CI job (~5–10 min per release), two new release
artifacts. Recurring maintenance: track nix-portable upstream
for security updates.

### Option B — Replace nix-portable with a fully-vendored static Nix

- Bundle a static-linked `nix` + minimal nix-daemon harness in
  the Stage 0 root dir directly (similar shape to the busybox
  vendor today).
- Eliminates the daemon-less constraint that motivated
  nix-portable, replaces it with a fully-controlled `nix` build.
- Larger root dir (nix-portable is ~74 MiB; a full static Nix +
  closure subset is comparable but with different trade-offs).

Cost: significantly more work than Option A. The Stage 0 closure
audit and reproducibility story would need re-derivation from
scratch. Recurring maintenance: track Nix upstream.

### Option C — Revert the fallback wiring

- Drop `bootstrap_builder_vm_image_via_root_dir_stage0` and the
  `BuilderVmImage::RootDir` variant.
- Stage 0 returns a clear "import a dev image to bootstrap" error
  when `~/.mvm/dev/current/` is missing.
- Cost: removes the structural improvement #414 landed. Fresh
  contributor hosts have no automatic bootstrap path.

### Recommendation

**Option A** for the asset fix. It preserves the architectural
shape #414 introduced, keeps daemon-less Nix as the bootstrap
contract, and reuses the existing release-artifact + hash-verify
machinery. Option B is a deeper rewrite that may be worth doing
later but shouldn't gate fixing the immediate breakage. Option C
is a credible escape hatch only if Option A hits an unforeseen
upstream blocker.

## Scope

Two workstreams, both small.

### W1 — Build + publish mvm-controlled nix-portable

- Add `build-nix-portable` job to `.github/workflows/release.yml`
  paralleling the `dev-image` job. Matrix: `aarch64-linux`,
  `x86_64-linux`.
- Clone `github.com/DavHau/nix-portable` at a pinned commit (start
  with their latest stable tag; verify it builds reproducibly).
  Build with the runner's host toolchain; upload the resulting
  binary as `nix-portable-<arch>-linux`.
- Extend the release `*-checksums-sha256.txt` manifest to include
  the new artifacts.
- Update `crates/mvm-build/src/stage0.rs`:
  - Rename `NIX_PORTABLE_AARCH64` → `NIX_PORTABLE_AARCH64_LINUX`
    for clarity.
  - Add `NIX_PORTABLE_X86_64_LINUX`.
  - Point both URLs at the mvm release artifacts; pin sha256.
  - Update `ASSETS_AARCH64`. Add `ASSETS_X86_64`.
- Update `materialize_root_dir` (if needed) to pick the right
  asset per `cfg!(target_arch)`. Today the table is aarch64-only.

**W1 acceptance:**

- A fresh `mvmctl dev up` on a host with no `~/.mvm/dev/current/`
  pulls the mvm-published nix-portable, hash-verifies, and the VM
  reaches `stage0-init: building path:/work/...` without exit 127.
- `file ~/.cache/mvm/stage0/nix-portable` reports `ELF` on
  Linux-bound builds.

### W2 — End-to-end Stage 0 CI smoke

- New CI job `stage0-root-dir-smoke` on macOS aarch64 (where
  libkrun + libkrunfw + gvproxy are installable). Runs:
  1. Ensure `~/.mvm/dev/current/` does **not** exist.
  2. `mvmctl dev up` against a minimal fixture flake (≤ 5
     derivations, no large closure pulls).
  3. Assert exit 0, assert `~/.cache/mvm/builder-vm/aarch64/`
     contains a non-empty `vmlinux` + `rootfs.ext4`.
- Time budget: ≤ 20 min per run (mostly dominated by the in-VM
  nix build, but a 5-derivation fixture should keep that under
  ~5 min on a warm `cache.nixos.org`).
- This job is the CI gate Plan 91's existence is meant to install.
  Stage 0 root-dir is fragile enough that we should have a
  smoke that runs `dev up` end-to-end before any PR that touches
  the dispatch can merge.

**W2 acceptance:**

- The smoke runs on every PR that touches
  `crates/mvm-build/src/stage0*` or
  `crates/mvm-cli/src/commands/env/apple_container.rs`.
- A regression equivalent to #416 (wrong-platform asset, missing
  binary, broken init.sh) fails the job with a clear "Stage 0
  supervisor exited with status N; console log at …" message in
  the CI log.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| DavHau's source repo isn't reproducibly buildable on a GitHub runner. | Pin to a known-good tag; if reproducibility fails, fork the source into `tinylabscom/nix-portable-vendored` and build from there. |
| nix-portable upstream rots between releases. | The mvm release cadence is the gate; we only re-base when we explicitly cut a release. The CI smoke catches breakage at upgrade time. |
| The Stage 0 root-dir-smoke job runs out of time budget on a cold `cache.nixos.org`. | Use a fixture flake whose closure is small enough that the warm cache covers the entire build (no large stdenv pulls). |
| The fix lands but contributors on bricked installs (no `~/.mvm/dev/current/`) still hit issue #416 until they pull a release with the new asset. | Document the workaround in the issue: import an existing dev image, or wait for the release that ships W1. |

## Out of scope

- Any change to the dev_image Stage 0 path. That path uses an
  existing dev image as the seed and doesn't touch nix-portable;
  it's unaffected by this bug.
- A persistent-builder-VM-aware Stage 0 (Plan 89). Stage 0 still
  emits the boot-per-job builder VM artifact today; Plan 89
  consumes that artifact and then never re-runs Stage 0. The two
  plans don't interact.
- Replacing nix-portable wholesale (Option B above). Tracked as a
  potential future plan.

## Related

- Issue #416 — bug report this plan resolves.
- PR #414 — original Stage 0 root-dir dispatch (the path this
  plan repairs).
- PR #415 — init.sh docstring follow-up.
- ADR-046 (`specs/adrs/046-builder-vm-via-libkrun.md`) — the
  two-artifact-layer rule. The new nix-portable artifact is
  cleanly a Layer-0 bootstrap asset.
- ADR-002 §W5.1 — hash-verify-on-download contract that the
  W1 mvm-published artifact path inherits.
