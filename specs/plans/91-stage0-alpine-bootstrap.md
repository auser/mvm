# Plan 91 — Stage 0 bootstrap via Alpine minirootfs (replaces nix-portable)

**Status:** drafted 2026-05-19 (replaces an earlier draft of Plan 91
that proposed mvm-controlled `nix-portable` release artifacts;
rejected by the user — see §"Earlier draft" below). Awaiting
prioritization.
**Tracks:** [#416](https://github.com/tinylabscom/mvm/issues/416).
**Follows:** Plan 72 (`specs/plans/72-builder-vm-via-libkrun.md`),
the Stage 0 root-dir dispatch shipped in #414 / #415.

## Problem

The Stage 0 root-dir fallback that #414 wired in cannot complete
a Nix build. Repro is reliable:

```sh
mv ~/.mvm/dev/current ~/.mvm/dev/current.shelved   # force fallback
cargo run -- dev up                                # → exit 127
```

Root cause: `crates/mvm-build/src/stage0.rs` pins
`https://github.com/DavHau/nix-portable/releases/download/v012/nix-portable-aarch64`.
That URL serves a **macOS Mach-O arm64 binary**, not a Linux ELF.
The bytes hash exactly to the pinned digest, so verification
passes — and then the in-VM `exec` returns 127 because the kernel
refuses the wrong-platform binary. Cross-checked across v010–v013;
the upstream `nix-portable-aarch64` asset is consistently macOS.
(The `nix-portable-x86_64` asset is a Linux bash self-extractor;
it would work, just not on Apple Silicon.)

## Why we are not "fixing the asset" and moving on

The natural reflex is "build the right binary, pin that." Two
options surface, both rejected:

1. **mvm-published release artifact.** Build `nix-portable`
   ourselves in `release.yml`, publish next to `dev-image`. Has the
   right hermetic shape but contradicts the source-checkout
   invariant (CLAUDE.md §"Source-checkout builds never depend on
   mvm-published artifacts") for any contributor whose
   `~/.mvm/dev/current/` is missing. Also requires a release cut
   before any fix lands.
2. **Vendor a binary in-tree.** ~74 MiB per arch. Avoidable.

The user articulated the underlying principle:

> "I want it to be built locally when in dev. I do NOT want to
> depend on github actions and a download — that's totally wrong
> — that's not what I want at all — we need to have it working
> locally."

The Stage 0 bootstrap binary should come from a neutral upstream
the Nix build path is going to talk to anyway (same network trust
boundary `nix build` uses for `cache.nixos.org`), not from a
mvm-specific artifact pipeline.

## Decision

**Replace `nix-portable` with an Alpine Linux minirootfs.** Stage
0 fetches Alpine's official minirootfs tarball for the host arch
(~4 MiB compressed) on first `dev up`, hash-verifies + PGP-verifies
against an embedded Alpine release-signing key, extracts it into
the materialized guest root, boots libkrun against it via
`krun_set_root`, and the in-VM init runs:

```sh
apk update
apk add nix-bin
nix build /work/nix/images/builder-vm#packages.<arch>-linux.default
```

Net effect: the builder VM image is built **locally** inside
libkrun on the contributor's host. Nothing mvm-controlled is
downloaded. The Alpine tarball + `apk add nix-bin` are the
bootstrap, both gated by Alpine's signing infrastructure.

### Why Alpine over busybox-only

Both could plausibly host Stage 0. The deciding factor is
cryptographic posture:

| Property | Busybox + ad-hoc fetch | Alpine + apk |
|---|---|---|
| Bootstrap binary signed | No (HTTPS only) | Yes (Alpine release-signing PGP key) |
| Per-install signature verification | Hand-roll | `apk-tools` automatic (Alpine RSA-signed APK index + signed packages) |
| Trust surface | "we got the bytes right" | "Alpine got the bytes right + verified by tooling we don't own" |
| Tarball size | ~1.6 MiB (busybox-static) | ~4 MiB (minirootfs) |
| Trust hardening on subsequent fetches | none | every `apk add` verified |

Alpine's `apk-tools` is the lever. It cryptographically verifies
the package index and every installed package against keys
shipped in the rootfs at `/etc/apk/keys/`. Once Stage 0 is inside
Alpine, **everything else `apk add` installs inherits Alpine's
verification chain.** That's a stronger property than us bolting
verification onto a busybox-driven curl pipeline.

The size difference is paid in compressed bytes during the
one-time fetch; it doesn't show up at run time after the
extraction step.

### Why not pin a working Linux nix-portable

We considered hunting for a working aarch64 Linux nix-portable
build (forks, alternate releases, Hydra outputs). Even if found,
the underlying problem is structural: a single binary with no
package-manager semantics gives us no path to install additional
Linux tooling later without bolting on ad-hoc verification. The
Alpine path solves the long-term shape of Stage 0, not just the
immediate symptom.

## Sourcing the Alpine minirootfs

Alpine publishes per-arch minirootfs tarballs at well-known URLs
under `https://dl-cdn.alpinelinux.org/alpine/v<major.minor>/releases/<arch>/`,
each shipped alongside:

- `alpine-minirootfs-<version>-<arch>.tar.gz` — the rootfs
- `alpine-minirootfs-<version>-<arch>.tar.gz.asc` — PGP signature
- A SHA256 manifest in the release directory

Trust bootstrap:

- Pin the tarball URL + SHA256 in `crates/mvm-build/src/stage0.rs`
  (same `BootstrapAsset` table shape used today).
- Embed Alpine's release-signing public key in the mvm source tree
  (`crates/mvm-build/src/stage0/alpine-release-key.asc`, ~few KB).
  Verify the `.asc` signature against this key on every fetch.
- A re-fetch from a tampered mirror fails one of two checks: the
  SHA256 (network MITM with stale bytes) or the GPG verification
  (mirror with new tarball, no Alpine private key).
- Cache the verified tarball at `~/.cache/mvm/stage0/alpine-minirootfs-<arch>.tar.gz`.
  Subsequent boots re-verify the cached SHA256 and short-circuit
  the fetch.

This re-uses the existing `BootstrapAsset` + `prepare_assets`
machinery; the only new bit is PGP verification, for which we'll
pick a minimal Rust impl — see §"Open questions" below.

## Scope

Three workstreams.

### W1 — Replace the Stage 0 asset table and materialize step

- Rename `NIX_PORTABLE_AARCH64` → `ALPINE_MINIROOTFS_AARCH64`,
  add `ALPINE_MINIROOTFS_X86_64`. Point each `url` at the
  Alpine official mirror; pin SHA256.
- New `BootstrapAsset` field: `signature_url: &'static str`
  (the `.asc` URL).
- Embed Alpine's release-signing PGP key:
  `crates/mvm-build/src/stage0/alpine-release-key.asc` via
  `include_bytes!`.
- Extend `prepare_assets` to fetch both the tarball and the
  signature, verify the SHA256, then verify the PGP signature
  against the embedded key.
- Rewrite `materialize_root_dir(dest)` to extract the verified
  tarball into `dest` instead of copying a single binary. Tar
  extraction can use the existing `tar` crate (already a
  workspace dep via `crates/mvm-build/src/oci_to_rootfs/unpack.rs`).
- Drop the busybox vendor (`stage0/busybox-aarch64-linux-musl`,
  `BUSYBOX_AARCH64_BYTES`, `BUSYBOX_AARCH64_SHA256`). Alpine's
  minirootfs includes busybox; no need to layer ours on top.

**W1 acceptance:**

- `cargo test -p mvm-build --lib stage0::` passes (existing
  tests adapted to the new shape; new tests for tarball+sig
  verification and tampered-input rejection).
- A fresh `mvmctl dev up` on a host with no `~/.mvm/dev/current/`
  reaches the in-VM phase with `/etc/alpine-release` present.

### W2 — Rewrite `init.sh` for Alpine + apk

- Drop the `nix-portable` invocation. Replace with:
  - `mountpoint -q /proc || mount -t proc proc /proc` etc.
    (Alpine's init pre-mounts most of these; keep the guards
    that #414 added, see [[reference_libkrun_gotchas]] §"set_root
    mode".)
  - `ip link set eth0 up; udhcpc -i eth0 -n -q`
  - `apk update`
  - `apk add nix-bin git ca-certificates`
  - `nix --extra-experimental-features 'nix-command flakes' build "path:/work/nix/images/builder-vm#packages.$(uname -m)-linux.default" --no-link --no-write-lock-file --impure --print-out-paths --print-build-logs > /tmp/store-path 2> /out/nix-stderr.log`
  - Copy `vmlinux` + `rootfs.ext4` + `cmdline.txt` from the store
    path to `/out/`. Same shape as today.
- Surface `apk add` failures the same way we surface nix-build
  failures (tail of stderr written to `/out/apk-stderr.log` and
  echoed to the console).
- Keep the `--option connect-timeout 30` lesson from #413 — the
  Alpine apk fetcher has its own connect-timeout knob
  (`apk --timeout=30 add …`) which we'll mirror.

**W2 acceptance:**

- The init script runs to completion against a real Alpine
  rootfs, against the in-repo builder VM flake, and emits the
  expected artifacts in `/out/`.
- A network-disabled host (passt/gvproxy with no upstream
  reachability) fails at `apk update` with a clear
  "no internet" message in the console log, not a kernel panic
  or a silent hang.

### W3 — End-to-end Stage 0 CI smoke

Same as the original Plan 91 W2: a CI job that runs `mvmctl dev
up` against a minimal fixture flake (≤ 5 derivations) with no
`~/.mvm/dev/current/` present, asserts exit 0 + produced
artifacts. Captures the regression class that let #416 ship.

- Job lives in `.github/workflows/ci.yml`. macOS aarch64 lane
  only — GH-hosted macOS doesn't expose Hypervisor.framework, so
  this is local-only (`MVM_LIBKRUN_E2E=1`) until a self-hosted
  runner lands. Until then, the smoke runs on every PR via a
  pre-merge guidance check ("did you run `MVM_LIBKRUN_E2E=1
  cargo test --test stage0_smoke`?") + a periodic full-fidelity
  run on a contributor box.
- Time budget: ≤ 20 min per run on warm `cache.nixos.org`.

**W3 acceptance:**

- The smoke runs against a fixture flake and exits 0 on a
  healthy bootstrap path.
- A regression like #416 (wrong-platform tarball, broken init
  script, missing `apk` key, network gateway misconfig) fails
  the job with the console log printed in the CI output.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Alpine's release URL or signing key rotates. | The pinned SHA256 catches URL drift; the embedded PGP key is the source of truth and we re-vendor when Alpine rotates (rare; Alpine rotates the release key on multi-year cadence). `mvmctl doctor` reports if Stage 0 is using a stale Alpine version. |
| Alpine repos go down or refuse `apk update`. | Same mitigation as today's `cache.nixos.org` outage — `dev up` fails with a clear network error. Document the workaround. |
| PGP verification adds a dependency that bloats `mvmctl`. | Pick the smallest viable PGP impl (`pgp` crate is ~200 KB compiled; `sequoia` is much larger and avoided). Shelling out to host `gpg` is rejected because Stage 0 must work without contributor-side prerequisites. |
| `apk add nix-bin` pulls an old / broken Nix from Alpine's repos. | The Alpine package set tracks Nix releases on a known cadence; we pin the Alpine *version* (e.g. `v3.20`) so the available `nix-bin` is reproducible. Re-pin on each plan revision. |
| The Alpine tarball's busybox lacks an applet we relied on (we previously vendored a custom busybox). | Alpine's busybox is the standard build with the full applet set. Audit the init script against Alpine's busybox config when we land W1 — known gaps are unlikely. |
| `tar` extraction on macOS handles a Linux tarball oddly (perms, sparse, …). | Use the Rust `tar` crate, not host `tar(1)`. We already do this elsewhere (`crates/mvm-build/src/oci_to_rootfs/unpack.rs`). |

## Open questions

1. **Which PGP crate.** `pgp` (the rpgp project) is the leading
   contender — pure Rust, minimal deps, supports detached
   signature verification. Verify it's audited / actively
   maintained before pulling in. Fallback: shell out to `gpg`
   if it's on the host (with a clear error if it isn't).
2. **Alpine version pin cadence.** Once we pin `v3.20`, the
   apk repo URL drifts at Alpine's release pace. Decide on a
   re-pin rhythm — likely "whenever someone touches Plan 91 or
   the Stage 0 init script."
3. **Should `apk add` use HTTP or HTTPS?** Alpine's APK signing
   makes HTTP technically safe (signed content), but HTTPS gives
   us defense-in-depth against a mirror that's compromised at
   the index-signing layer. Default to HTTPS; document the
   trade-off.

## Earlier draft (rejected approach)

The first draft of Plan 91 proposed building `nix-portable` in
GitHub Actions and shipping it as an mvm release artifact. The
user rejected it:

> "I do NOT want to depend on github actions and a download —
> that's totally wrong — that's not what I want at all — we
> need to have it working locally."

The relevant principle is "Source-checkout builds never depend
on mvm-published artifacts" (CLAUDE.md). Bootstrap binaries come
from neutral upstreams (Alpine, cache.nixos.org), not from us.

## Out of scope

- The dev_image Stage 0 path. That dispatch still works when
  `~/.mvm/dev/current/` is populated and is unaffected by this
  plan. Plan 91 fixes the *fallback* (cold-bootstrap) path.
- Replacing libkrunfw or moving off the libkrun set_root mode.
  The boot architecture from #414 stays.
- Persistent builder VM dispatch (Plan 89). Stage 0 still emits
  the boot-per-job builder VM artifact today; Plan 89 consumes
  it. The two plans don't interact.

## Related

- Issue #416 — bug report this plan resolves.
- PR #414 — original Stage 0 root-dir dispatch (the path this
  plan repairs).
- PR #415 — init.sh docstring follow-up.
- PR #417 — this plan file (and the SPRINT follow-up entry).
- ADR-046 (`specs/adrs/046-builder-vm-via-libkrun.md`) — the
  two-artifact-layer rule. The Alpine tarball is cleanly a
  Layer-0 bootstrap asset.
- ADR-002 §W5.1 — hash-verify-on-download contract that the
  Alpine fetch inherits, extended with PGP verification.

## Acceptance criteria (whole plan)

1. `mvmctl dev up` on a host with no `~/.mvm/dev/current/`
   succeeds end-to-end, producing a builder VM image in
   `~/.cache/mvm/builder-vm/<arch>/`.
2. The bootstrap chain is: vendored mvm sources → Alpine
   tarball (hash + PGP verified) → `apk add nix-bin` (Alpine
   RSA-signature verified) → nix build (cache.nixos.org
   narinfo verified) → builder VM image.
3. No mvm-published release artifact appears anywhere in that
   chain.
4. The CI smoke (W3) gates regressions in the bootstrap
   shape — a wrong-platform tarball, a tampered signature, or a
   broken init script all fail the lane with a readable
   console-log excerpt.
