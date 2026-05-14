# Plan 75 — Remove microsandbox via mvm-oci + libkrun (bidirectional OCI as a first-class feature)

> Status: proposed
> Owner: TBD
> Started: —
> Depends on: plan 57 (libkrun spike — shipped), plan 72 (libkrun builder VM — shipped 2026-05-13)
> Supersedes: ADR-046 §"Stage 0 for contributors without host Nix" (microsandbox is no longer **kept** behind `--features contributor-bootstrap`; this plan removes it outright)
> Implements: ADR-046 §"What we keep vs. drop from microsandbox" final row resolution (microsandbox → zero); subsumes plan 74 W1 ("OCI image ingest") into a single execution path that also serves Stage 0
> Tracking: regression of 2026-05-14 (commit `19919f6` added a second `rustPlatform.buildRustPackage` to `nix/images/builder-vm/flake.nix`, overflowing microsandbox's 4 GiB overlay during `mvmctl dev up`); long-standing tension between CLAUDE.md §"Firecracker-only: no Docker/containers" and the `contributor-bootstrap` feature still pulling a Docker-Hub OCI image via microsandbox

---

## Why

`mvmctl dev up` on a source checkout without host Nix still goes through `MicrosandboxBuilderVm` (Stage 0, gated behind `--features contributor-bootstrap`). That path is the **only** surviving reason `dep:microsandbox` lives in our Cargo closure. It exists to do one job: pull `docker.io/nixos/nix:2.24.10`, run `nix build` inside, and emit the Layer 1 builder VM rootfs.

This job has a name in our roadmap already: **OCI image ingest** (plan 74 W1). The user-facing claim "mvm can run any OCI image as a microvm" requires the exact same primitives — fetch a manifest from a registry, verify layer digests, unpack to a rootfs, hand the rootfs to libkrun/Firecracker. Today we have two paths to that primitive: microsandbox (Stage-0-only, broken under overlay pressure, contradicts CLAUDE.md §"Firecracker-only") and "not yet written" (plan 74 W1, unstarted).

Plan 75 collapses both into one: build `mvm-oci` once, use it for Stage 0 *and* for `mvmctl image pull`. The day Stage 0 is OCI-ingest-by-libkrun, microsandbox has no remaining justification in the workspace and gets deleted in full. Same day, the user-facing OCI ingest claim ships behind the same primitive that just got 100% Stage-0 production coverage.

Three properties make this the right framing:

1. **Dogfooded primitive.** OCI ingest is exercised on every `mvmctl dev up` from a source checkout. The bug-rate gap between "code that runs in CI smoke tests" and "code that runs on every contributor laptop daily" closes by construction.
2. **Single trust boundary.** Today contributors trust `docker.io/nixos/nix:2.24.10` via microsandbox; end users trust the mvm-published builder VM image. After plan 75: both populations use the same `mvm-oci` code, with the same SHA-256 verification path, against the same registry. Trust posture is uniform.
3. **The bidirectional story unlocks.** Once OCI → microvm is on the floor, OCI ← microvm (export a baked microvm with its signed plan + audit chain + sealed deps volume as a portable OCI image) becomes additive, not a separate effort. Plan 75 W7 captures the export half so the strategic feature is sequenced, not lost to "we'll get to it."

This is the first plan that materially **shrinks** the workspace's dep closure. ~40 transitive crates leave with microsandbox (already gated since plan 72 W6 — plan 75 deletes the feature flag itself).

---

## Prerequisites

- Plan 72 W0–W6 shipped: libkrun is the user-facing builder VM, `LibkrunBuilderVm::run_build` is the production launcher, builder VM image artifacts published per release.
- `mvm_libkrun::start_with_config` reliably boots an externally-provided ext4 rootfs on macOS Apple Silicon (already true; the dev image is built this way today).
- A macOS Apple Silicon laptop and a Linux KVM host for the W1–W6 dev loop. W4 (CI deletion) and W6 (cosign) add a CI job per platform.

---

## Scope guards

In:

- A new `crates/mvm-oci/` crate with HTTP-based OCI registry client (manifest + config + layer fetch, digest verification, layer unpack with full OCI semantics, ext4 rootfs emission).
- Replacement of `build_image_via_microsandbox` with an OCI-pull → libkrun launch path. Same `BuilderJob` / `BuilderMounts` / `BuilderArtifacts` contract; the consumer in `mvm-cli` doesn't change.
- Deletion of `dep:microsandbox`, the `contributor-bootstrap` Cargo feature, `MicrosandboxBuilderVm`, and every `#[cfg(feature = "contributor-bootstrap")]` gate.
- User-facing `mvmctl image pull/ls/rm` + `mvmctl up --image <ref>` (delivers plan 74 W1).
- microvm → OCI export (`mvmctl image export`), with explicit documentation of which security claims travel and which do not.
- CI gates: layer-unpack fuzz, digest-mismatch rejection, mutable-tag policy enforcement, registry-anonymous-auth path, reproducibility double-build for the OCI ingest path.
- An ADR (or amendment to ADR-046) recording the trust-boundary shift.

Out:

- A *daemon* (the `mvm-oci` crate is a library; CLI verbs are mvmctl-shaped).
- A registry of our own (we publish artifacts via GitHub Releases, not a registry server). Push-to-registry comes after W7 lands; out of plan 75.
- Image *signing* on import (we **verify** cosign signatures when present; we don't issue them. Mint-time signing lives in the release pipeline, not in mvmctl).
- `containerd` / `runc` / any OCI runtime spec compliance. We pull OCI **images**; we launch them via libkrun/Firecracker, not via the OCI runtime spec.
- Plan 74 W2–W6 (network policy, secret placeholders, SDK lifecycle, cold-start budgets, filesystem backends). Plan 75 delivers W1; the rest stays Plan 74's problem.
- All Docker daemon paths. The existing `crates/mvm-backend/src/docker.rs` Docker-load fallback is a **separate** feature (run a Nix-built rootfs as a Docker container). It is **not** part of mvm-oci; it stays untouched.

---

## Acceptance criteria (whole-plan)

When all of the following hold, plan 75 moves to `specs/backlog/`, microsandbox is gone, and the OCI ingest user-facing claim ships:

1. `mvmctl dev up` on a stock macOS Apple Silicon host with no host Nix, no Docker, no microsandbox, no host `nix` binary:
   - Pulls `docker.io/nixos/nix:2.24.10` via `mvm-oci` (anonymous auth, manifest + layer digest verification).
   - Unpacks the layers to an ext4 rootfs at `~/.cache/mvm/oci/stage0/<digest>/rootfs.ext4`.
   - Launches that rootfs via libkrun, runs `nix build` of the builder-vm flake inside.
   - Produces `vmlinux` + `rootfs.ext4` for the builder VM image; the rest of the existing dev-up flow proceeds unchanged.
   - Exits 0 with no manual intervention.
2. `cargo metadata --no-default-features --features ''` shows **no `microsandbox*` crate anywhere in the workspace closure**. `grep -r microsandbox` returns hits only in `specs/` (history) and CHANGELOG entries.
3. `mvmctl image pull docker.io/alpine:3.20` succeeds. Subsequent `mvmctl up --image docker.io/alpine:3.20` boots Alpine as a microvm and runs `/bin/sh -c 'echo hi'` to completion. (Linux KVM lane; macOS lane subject to base-image arch.)
4. `mvmctl image export <vm-id>` produces an OCI-spec-v1.1 tarball with the workload's rootfs, an `mvm-claims.json` annotation enumerating which security claims travel with the image, and a CycloneDX SBOM if the source had a sealed deps volume.
5. CI gates (in `.github/workflows/ci.yml`):
   - `oci-layer-unpack-fuzz` — cargo-fuzz target on `mvm_oci::layer::unpack`, ≥30 minutes per PR run; CIFuzz daily.
   - `oci-digest-mismatch-reject` — byte-flip a layer post-fetch, assert `mvmctl image pull` exits with `E_OCI_DIGEST_MISMATCH` and removes the partial artifact.
   - `oci-malformed-manifest` — manifest with unknown mediaType, manifest with missing layer, manifest with circular index — all rejected before layer fetch.
   - `oci-mutable-tag-prod-reject` — production policy (`--prod`) rejects `:latest` and any unpinned-by-digest reference; dev policy admits with warning.
   - `oci-reproducibility` — pull the same ref twice from a clean cache; assert byte-identical `rootfs.ext4` digests.
   - `oci-anonymous-auth-only` — pull from a fixture registry that requires no auth; assert no `~/.docker/config.json` read attempt (strace lane on Linux).
6. The amended ADR (or ADR-049) records: microsandbox removed, mvm-oci is the OCI-ingest primitive, trust boundary is the registry's content-addressed manifest plus optional cosign attestation.

---

## Security considerations (cross-cutting)

These apply to every workstream; each workstream's "tests" section lists the specific assertions that prove the property.

### Updated security claims in CLAUDE.md / ADR-002

CLAUDE.md §"Security model" claim 6 today reads: *"Pre-built dev image is hash-verified."* This plan extends it to a stronger form: **"Every OCI image — Stage 0 base, user-pulled image, and our own published builder VM artifact — is verified by content-addressed digest before any byte is consumed, and tampered layers fail closed."** The implementation is shared (`mvm_oci::digest::verify_streaming`), so the proof is one CI lane instead of three.

New CLAUDE.md claim 10 (proposed): **"OCI image provenance is recorded in the admission audit chain."** Every `mvmctl up --image <ref>` emits an audit entry with the registry, resolved digest, layer digest list, and (if present) cosign attestation. `mvmctl audit verify` proves the chain. Plan 75 W5 wires it.

### Trust boundaries

| Boundary | Today | After plan 75 |
|---|---|---|
| Stage 0 base image | `docker.io/nixos/nix:2.24.10` via microsandbox, anon auth, digest unenforced | `docker.io/nixos/nix:2.24.10` via mvm-oci, anon auth, digest **pinned in source** to a specific `sha256:…`, verified at fetch time. Bump-the-pin is a deliberate PR. |
| User-facing OCI pull | Not supported | `mvm-oci` HTTPS-only; system trust store; optional `~/.mvm/registry-ca.pem` for air-gapped / pinned-CA workflows; no `~/.docker/config.json` read by default. |
| Cosign | Not supported anywhere | Verified when present (W6); absence does not block (registries are partial coverage); presence + invalid signature blocks. Configurable per-registry policy. |
| Layer unpack | microsandbox internals (opaque, not auditable in our codebase) | `mvm_oci::layer::unpack` — explicitly handled whiteouts, symlinks (refuse `..` escape), hardlinks (within-layer only), xattrs (default-allow user.* + security.capability, deny everything else), device nodes (refuse outside `/dev/null` `/dev/zero` `/dev/random` `/dev/urandom` in any layer), setuid/setgid bits (warn under `--prod`, refuse if cosign fails). |

### Threat model additions to ADR-002

- **T-OCI-1: Compromised registry.** Mitigation: pinned digest for Stage 0; cosign verification for user pulls when configured; HTTPS pinning to system CA bundle.
- **T-OCI-2: Layer-unpack CVE class** (CVE-2019-14271-style, path traversal, symlink escape, hardlink-to-host, setuid escalation). Mitigation: deny-by-default unpacker, fuzz lane, explicit allow-lists for xattrs and device nodes; defense-in-depth via libkrun isolation (a unpacked-then-launched rootfs runs in a microVM, not on the host).
- **T-OCI-3: Tag mutation race.** Mitigation: `--prod` rejects unpinned references; dev mode resolves to a digest at first pull and caches by digest thereafter; re-resolution requires explicit `mvmctl image pull --refresh`.
- **T-OCI-4: TLS substitution / DNS rebinding at pull time.** Mitigation: HTTPS-only, system CA trust by default, no DoH/DoT bypass; SNI must match the requested hostname; redirects must stay within the same registry origin (configurable allow-list).
- **T-OCI-5: Supply-chain via Stage 0 base image.** Mitigation: pinned digest in source; CI runs a daily refresh job that opens a PR if upstream has a newer signed image. Bumping is a human decision with a diff.

### What does **not** change

- CLAUDE.md §"Host Nix is never used by mvmctl" stays load-bearing. mvm-oci has no nix dep; it talks to registries over HTTPS.
- CLAUDE.md §"No SSH in microVMs, ever" — mvm-oci does not ship sshd or any service into the rootfs it emits; it only unpacks what the registry returned.
- ADR-046's two acquisition paths stay intact (source checkout = build from in-repo flakes; installed binary = download released prebuilts). What changes is *Stage 0 in the source-checkout path*: previously microsandbox + nixos/nix OCI; after plan 75, mvm-oci + nixos/nix OCI + libkrun. Same trust root, different code path.

### What microvm → OCI export does **not** carry

W7 ships export, but the user must understand which security claims travel with the exported image:

| Claim | Travels in exported OCI image? | Why |
|---|---|---|
| dm-verity rootfs (claim 3) | No | OCI consumers' boot path doesn't honor dm-verity; the kernel cmdline isn't ours. |
| Signed ExecutionPlan (claim 8) | Metadata only (annotation), not enforced | Enforcement requires `mvm-supervisor` at the consumer side. Annotated for audit; not load-bearing. |
| Chain-signed audit log (claim 8) | No | Audit chain is host-state; doesn't fit in an image. The export annotation lists the chain head digest so a consumer can request the chain out-of-band. |
| Sealed deps volume (claim 9) | Yes (as separate annotated layer or sidecar tarball) | The sealed volume is content-addressed; it lives in the image as its own layer with the `meta.json` chain intact. Reproducible cross-platform. |
| Verified provenance (new claim 10) | Yes (cosign signature on the exported image, if the user opts in) | Exported images can be cosigned by the user; export does not auto-sign. |

The export emits an `mvm-claims.json` annotation that enumerates each claim's status. Plan 75 W7 ships the CI test that asserts the annotation is present and accurate.

---

## W0 — Decision and scope guards (ship before any code)

**Goal:** ratify the architectural choice and prevent overclaiming before primitives exist.

- [ ] Land this plan as `specs/plans/75-oci-stage0-microsandbox-removal.md`. (This document.)
- [ ] Land an ADR (proposed: ADR-049) recording the trust-boundary shift in section "Security considerations" above. Either supersede the relevant clauses of ADR-046 or add an addendum; do not leave ADR-046 contradicting plan 75.
- [ ] Add a `specs/claims/` gate file for new claim 10 ("OCI image provenance in audit chain"), default state **Planned**. Plan 74 W0 (claims hygiene) precedent — the gate file blocks docs from claiming the property until W5 ships and tests pass.
- [ ] Add `xtask check-no-overclaim` lints for the OCI surface — block docs strings, README, landing page from saying "any OCI image runs" until W3 acceptance closes.

### W0 ships as

A docs-only PR. No code changes outside `specs/` and `xtask/`. Reviewable in one sitting.

---

## W1 — `mvm-oci` crate: registry client + manifest + digest verification

**Goal:** a Rust library that talks HTTPS to an OCI Distribution v2 registry, fetches manifests + configs + layers, verifies every byte against the manifest's digest, and persists the result to a content-addressed cache.

### Interface

```rust
pub struct Client {
    pub registry: RegistryRef,         // host + optional port + protocol
    pub cache_root: PathBuf,           // ~/.cache/mvm/oci by default
    pub trust: TrustPolicy,            // anonymous | bearer-from-keyring | cosign-required
    pub network: NetworkPolicy,        // sni-pinned, redirect-same-origin, optional pinned-CA
}

pub struct ImageRef { pub registry: String, pub repo: String, pub reference: Reference }
pub enum Reference { Tag(String), Digest(Sha256) }

impl Client {
    pub fn pull(&self, image: &ImageRef) -> Result<PulledImage, Error>;
    pub fn resolve(&self, image: &ImageRef) -> Result<ResolvedRef, Error>;
}

pub struct PulledImage {
    pub manifest: oci_spec::ImageManifest,
    pub config: oci_spec::ImageConfiguration,
    pub layers: Vec<LayerRef>,         // content-addressed paths in cache
    pub resolved: ResolvedRef,         // host + repo + sha256 digest
}
```

### Behavior

`pull`:

1. Resolve the reference. If `Reference::Tag` and `TrustPolicy::production`, **refuse** with `E_OCI_MUTABLE_TAG` (must be pinned by digest). Otherwise GET the manifest at the tag, take its `Docker-Content-Digest` header (verified against the body's hash), and from here on treat the pull as digest-pinned.
2. GET the manifest body, hash-verify against the resolved digest, parse as `oci_spec::ImageManifest` (or the OCI image index variant; in which case pick the matching architecture and recurse on the picked manifest).
3. GET the image config, hash-verify against `manifest.config.digest`.
4. For each layer: GET if absent from cache, streaming SHA-256 in-flight, reject on mismatch (delete partial). Cache at `<cache_root>/blobs/sha256/<digest>` (immutable, never overwritten).
5. Persist a small `<cache_root>/refs/<host>/<repo>/<reference>.json` record mapping the ref to the resolved digest, manifest digest, config digest, and layer digest list, with a fetched-at timestamp.

`resolve` is `pull`'s first step without the layer-fetch — used by callers that only need the digest (e.g. CI's "is the pin still current?" job).

### Trust policy

- `TrustPolicy::Anonymous` — default. No credential lookup. Registries that 401 on anonymous requests fail closed with `E_OCI_AUTH_REQUIRED`.
- `TrustPolicy::Keyring { entry }` — pull bearer token from the host OS keyring under the named entry. Never reads `~/.docker/config.json` (which is a credential helper that can shell out — out of scope as a security surface).
- `TrustPolicy::CosignRequired` — wraps any other policy. After resolving and before unpacking, verify the cosign signature attached to the image; refuse on absence or invalid signature. Public key sources: a project-pinned key (`~/.mvm/cosign-pubkeys/*.pem`) or fulcio + rekor (transparency log) when configured. W6 wires this; W1 ships the type stub.

### Network policy

- HTTPS only. HTTP is refused outright (no `--insecure-registry`; if the user has a private registry without TLS, that's a fix for them, not a flag on us).
- System CA trust by default; optional `~/.mvm/registry-ca.pem` extra-CA bundle for air-gapped / pinned-CA workflows.
- SNI **must** match the request hostname.
- Redirects: same-origin only by default. Cross-origin redirect is opt-in per-registry via `~/.mvm/registry-policy.toml` — Docker Hub redirects to `production.cloudflare.docker.com` for layer blobs, so cross-origin is needed for Docker Hub specifically. Documented and explicit.
- No DoH/DoT bypass — use the system resolver.

### Tests (W1 acceptance)

- Unit: digest-mismatch on manifest, on config, on a layer. Each rejects + deletes partial cache state.
- Unit: malformed manifest (unknown mediaType, missing required fields, circular index reference) — rejected before any layer fetch.
- Unit: mutable-tag rejection under `TrustPolicy::production`; admitted with warn under dev.
- Integration: pull `docker.io/library/alpine:3.20@sha256:<pin>` against a local-fixture registry (in-process `axum` server, OCI-v2 routes, pre-loaded with the upstream manifest + layers). Assert byte-identical layer bytes after two consecutive pulls from a clean cache.
- Integration: TLS hostname-mismatch in the test fixture's certificate fails closed; not bypassable.
- Strace lane: confirm no `open(...".docker/config.json"...)` in default-trust-policy pulls.
- Fuzz: `cargo-fuzz` target on manifest parsing. ≥30 min per PR.

### W1 ships as

A new crate `crates/mvm-oci/` with one new dep group: `reqwest` (rustls only — no native-tls, no openssl), `sha2`, `oci-spec`, `serde_json`. Plus a dev-dep on `axum` for the test fixture. PR introduces the crate but **does not** wire it into mvm-cli yet. Reviewable as "is this OCI client correct and safe?" in isolation.

---

## W2 — Layer unpack with full OCI semantics → ext4 rootfs

**Goal:** turn the cached layers from W1 into an ext4 rootfs that libkrun can boot, with explicit and tested handling of every OCI layer-tar feature.

### Behavior

`mvm_oci::rootfs::build(pulled: &PulledImage, out: &Path) -> Result<RootfsArtifact, Error>`:

1. Allocate a sparse ext4 image at `out` sized as `sum(layer.uncompressed_size) * 1.5 + 64 MiB` floor.
2. `mkfs.ext4` (via the embedded `e2fsprogs` from the builder VM closure — *not* from the host; we don't shell out to host binaries from the unpacker). For W2, MVP shells out to the host's `mkfs.ext4`; W3 follows up by routing it through the libkrun VM. The host-shellout MVP is gated by an `MVM_OCI_HOST_MKFS=1` env var and never the default — see Risks §"R3 host-tool dependency."
3. Mount the image via `loop` (Linux) or `hdiutil attach` (macOS) — actually no, the unpack happens *inside* the builder VM (W3 wiring). For W2 MVP, the unpack target is a directory tree (not a mounted ext4); W3 fuses it with the libkrun mount path. This keeps W2's scope as "pure userspace tar handling."
4. For each layer in order (lowest first): stream the gzipped/zstd-compressed tar, applying:
   - **Whiteouts** — `.wh.<name>` removes the named entry from the assembled tree; `.wh..wh..opq` clears the directory. Refuse paths that escape (`/foo/../wh..`).
   - **Symlinks** — written verbatim, but the **target** is recorded and any subsequent path resolution that would traverse the symlink **does not** follow it during unpack. We reconstruct the tree by-path, not by-resolved-path. Targets that escape root are recorded as-is (they're inside the rootfs and resolve at *boot* time, not at unpack time); refused only if the symlink itself is at an escape path.
   - **Hardlinks** — within-layer-only; cross-layer hardlinks materialize as full copies. (CVE-2019-14271 class.)
   - **xattrs** — allow-list: `user.*`, `security.capability`, `security.selinux`. Everything else (including `system.*`) is dropped with a warning. Configurable per-policy.
   - **Device nodes** — refused unless they're the four "obvious" devices in `/dev` (null/zero/random/urandom). Other device nodes fail the unpack closed.
   - **Setuid/setgid bits** — preserved by default; under `TrustPolicy::CosignRequired` and signature invalid, the unpack itself was refused upstream (W1) so we don't reach here. Under prod policy with cosign present and valid: preserved with an audit annotation. Under dev policy: preserved with a CLI warning.
   - **Paths containing `..` or absolute paths** — refused; tar entries must be relative and traversal-free.
   - **Path length > 4096** — refused.
5. After all layers applied, emit a manifest at `<out>.manifest.json` recording every absorbed layer digest, every applied whiteout, every refused entry, and the final tree's content-addressed hash.

### Tests (W2 acceptance)

- Unit fixtures for every layer-tar feature: whiteouts (incl. opaque), symlinks (in-root and escape attempts), hardlinks (in-layer and cross-layer), file modes (incl. setuid/setgid), xattrs (each category), device nodes (allowed and refused), absolute paths, traversal paths, oversize paths.
- Reproducibility: unpack the same layers twice into clean dirs, assert byte-identical tree content-hash.
- Fuzz: `cargo-fuzz` target on `mvm_oci::layer::unpack_one`, feed it malformed tar inputs (truncated headers, header magic mismatch, size lies). ≥30 min per PR.
- Live: pull and unpack `docker.io/library/alpine:3.20@sha256:<pin>`; assert `/bin/sh` is present, executable, and resolves to busybox; assert the rootfs is mountable and `ls /` works.
- Negative: a synthesized adversarial image with `/etc/shadow` set 4755 and a setuid binary at `/usr/local/bin/escalate` — under dev policy, unpack succeeds with two warnings; under `--prod` without cosign, unpack refuses with `E_OCI_SETUID_UNSIGNED`.

### W2 ships as

A second PR in the `mvm-oci` crate, plus the test fixtures. Still **not** wired into mvm-cli. Reviewable as "is this unpacker correct under adversarial input?" in isolation.

---

## W3 — Wire libkrun Stage 0 to mvm-oci

**Goal:** replace `build_image_via_microsandbox` with `build_image_via_libkrun_oci` so the Stage 0 path uses mvm-oci end-to-end. After W3, `mvmctl dev up` on a stock macOS host with no host Nix and no microsandbox in the binary completes.

### Behavior

New file `crates/mvm-cli/src/commands/env/oci_stage0.rs` exposing `fn build_image_via_libkrun_oci(flake_dir: &str, out_dir: &str) -> Result<(String, String)>`. Internals:

1. **Resolve and pull the Stage 0 base** — `mvm_oci::Client::pull(&IMAGE_REF)` where `IMAGE_REF` is the pinned `docker.io/nixos/nix:2.24.10@sha256:<digest>` constant in the source (W5 has a pin-bump CI job).
2. **Build the Stage 0 rootfs** — call `mvm_oci::rootfs::build(&pulled, &stage0_root)` to assemble the unpacked tree.
3. **Launch via libkrun** — call `LibkrunBuilderVm::run_build` with `BuilderJob::Flake { flake_ref, attr_path }` and `BuilderMounts { flake_src = workspace, host_nix_store = None, artifact_out = out_dir, stage0_root: Some(stage0_root) }`. The new field tells libkrun to use the OCI-derived rootfs as the boot rootfs instead of the existing builder VM image (which is what Stage 0 is producing).
4. **Verify outputs** — same as today's `build_image_via_microsandbox` tail: assert `vmlinux` + `rootfs.ext4` exist in `out_dir`.

Calling pattern in `bootstrap_builder_vm_image` (`apple_container.rs:1938`) flips from `build_image_via_microsandbox` to `build_image_via_libkrun_oci`. The `#[cfg(feature = "contributor-bootstrap")]` gate stays in W3 (deletion is W4) so the build still compiles with the old code path available for emergency revert.

### Tests (W3 acceptance)

- Live: `cargo run -p mvmctl -- dev up` on macOS Apple Silicon completes against a clean cache. Stage 0 fetches `nixos/nix:2.24.10`, unpacks, boots via libkrun, `nix build`s the builder VM image, emits Layer 1 artifacts. Total wall-clock time documented in plan 75 W3 release notes (expected: 4–8 min cold, <30 s warm).
- Live: `cargo run -p mvmctl -- dev up` on Linux KVM — same flow via Firecracker.
- Integration: `MVM_OCI_STAGE0_BASE=registry:5000/nixos-nix-mirror:2.24.10@sha256:<pin>` env var override lets CI point Stage 0 at a local mirror; assert that's the only network destination hit during the test.
- Negative: tamper a layer in the cache between `pull` and `rootfs::build`; assert Stage 0 fails closed with `E_OCI_DIGEST_MISMATCH` and removes the partial state.
- Negative: pin-bump simulation — flip the in-source digest constant to a non-existent digest; assert Stage 0 fails closed with `E_OCI_NOT_FOUND` and the error message names the in-source constant.
- CI lane: `oci-reproducibility` from acceptance criterion 5 — run W3's Stage 0 twice from a clean cache; assert byte-identical `rootfs.ext4` output digests.

### W3 ships as

A PR that wires W1 + W2 into the existing Stage 0 path. Still preserves the `--features contributor-bootstrap` escape hatch for one release cycle. After W3 lands and bakes for a week, W4 deletes microsandbox.

---

## W4 — Delete microsandbox (no more contributor-bootstrap)

**Goal:** after W3 has shipped and proven stable for a release cycle, remove every microsandbox reference from the workspace.

### Concrete deletions

- `crates/mvm-backend/Cargo.toml`: drop `contributor-bootstrap = ["dep:microsandbox"]` and the `microsandbox` dep itself.
- Workspace root `Cargo.toml`: drop the `contributor-bootstrap` feature aggregation.
- `crates/mvm/Cargo.toml`, `crates/mvm-cli/Cargo.toml`, `crates/mvm-build/Cargo.toml`: drop `contributor-bootstrap` features.
- `crates/mvm-build/src/builder_vm.rs`: delete `MicrosandboxBuilderVm`, `BUILDER_DEFAULT_CPUS`, `BUILDER_DEFAULT_MEMORY_MIB`, `BUILDER_GUEST_*` constants (or move them under the libkrun path's namespace if any are still useful), `run_build_async` impl, and the four microsandbox-only tests.
- `crates/mvm-cli/src/commands/env/apple_container.rs`: delete `build_image_via_microsandbox` (~90 lines), the `#[cfg(feature = "contributor-bootstrap")]` block in `bootstrap_builder_vm_image`, every doc comment referencing microsandbox or the 4 GiB overlay.
- `nix/images/builder-vm/flake.nix`: scrub the W2-era comments that say "fits in 4 GiB overlay" / "no rustc, no cargo crates"; replace with a single sentence acknowledging the new launcher.
- `xtask/Cargo.toml`: drop the `features = ["contributor-bootstrap"]` on `mvm-build`.
- `Cargo.lock`: regenerate.
- `specs/`: leave ADR-046 in place (history), but add an ADR-049 amendment or update ADR-046's status to "Accepted, then superseded by plan 75 W4 on YYYY-MM-DD".

### Tests (W4 acceptance)

- `cargo metadata --no-default-features --features ''` — zero `microsandbox` hits.
- `grep -rni microsandbox crates/ src/ Cargo.toml` — zero hits outside `xtask/check-no-overclaim`'s allow-list of historical references.
- Workspace `cargo test`, `cargo clippy -- -D warnings`, `cargo xtask check-no-display-on-secret-types` all green.
- CI matrix (macOS + Linux): `mvmctl dev up` from a clean cache succeeds without any `--features` flag.

### W4 ships as

A single PR that's mostly deletions. Reviewable as a "did we miss any references?" pass. After W4 lands, microsandbox is **gone** from the workspace.

---

## W5 — User-facing OCI ingest (`mvmctl image pull`, `mvmctl up --image`)

**Goal:** ship plan 74 W1's user-facing surface on top of the mvm-oci primitive that's already in production via Stage 0.

### CLI surface

- `mvmctl image pull <ref> [--digest] [--cosign] [--prod]` — fetch + verify + cache. Records to template store at `~/.mvm/templates/oci/<resolved-digest>/`.
- `mvmctl image ls [--registry <host>]` — list cached OCI images by ref + resolved digest + fetched-at + size.
- `mvmctl image rm <ref|digest>` — drop a cached image (and any unused layers — reference-counted GC).
- `mvmctl image inspect <ref|digest>` — print manifest + config + layer digests + `mvm-claims.json` annotation if present.
- `mvmctl up --image <ref>` — boot the named image as a microvm. Internally: `image pull` if not cached; `rootfs::build` to materialize; `LibkrunBuilderVm::run_workload` (or the existing instance launcher — same trait surface).
- Templates produced by `image pull` integrate with the existing `template build/list/...` verbs — a pulled OCI image is a template with `kind = "oci"`.

### Audit chain integration (new CLAUDE.md claim 10)

Every `mvmctl up --image` admission emits an audit-chain entry with:

- Registry host, repo, reference (as supplied).
- Resolved manifest digest.
- Layer digest list.
- Cosign attestation summary (verified, unsigned, or refused).
- Trust policy in effect (`anonymous`, `keyring`, `cosign-required`).

`mvmctl audit verify` continues to detect drift on the chain; tampering with an OCI provenance entry breaks the chain HMAC.

### Production policy (`--prod`)

- Mutable tag refs (anything not pinned by digest) refuse closed with `E_OCI_MUTABLE_TAG_PROD`.
- Setuid binaries in any layer refuse closed unless cosign attests the image.
- Unsigned images refuse closed unless `~/.mvm/registry-policy.toml` explicitly admits the registry under `prod_unsigned_allow = [...]`.
- Audit entry's `trust_policy` field is `cosign-required` or refuse closed.

### Tests (W5 acceptance)

- `mvmctl image pull docker.io/library/alpine:3.20` succeeds (dev), records template, audit entry.
- `mvmctl image pull docker.io/library/alpine:latest --prod` refuses with `E_OCI_MUTABLE_TAG_PROD`.
- `mvmctl image pull docker.io/library/alpine:3.20@sha256:<pin> --prod` succeeds when the registry has cosign attestation (test fixture), refuses without.
- `mvmctl up --image alpine:3.20` boots the image and runs `sh -c 'echo hi; exit 0'`.
- `mvmctl audit verify` after a sequence of pulls + ups reports the chain valid; byte-flip an audit entry → chain invalid.

### W5 ships as

A PR that adds the CLI verbs and audit wiring. Closes plan 74 W1.

---

## W6 — Cosign verification, registry credentials hardening

**Goal:** ship the trust-establishment story so `--prod` is actually defensible.

### Cosign

- Verify cosign signatures against a project-pinned key list at `~/.mvm/cosign-pubkeys/` (PEM, one per file).
- Optional fulcio + rekor support for sigstore-attested images. Gated by `~/.mvm/registry-policy.toml::cosign_use_sigstore = true`.
- Plan 75 W6 ships the verification path; signing tooling is a *separate* downstream feature (W7 export pairs with it for the bidir story).

### Credentials

- `TrustPolicy::Keyring` — read bearer tokens from the host OS keyring (macOS Keychain, Linux Secret Service). No `~/.docker/config.json` read; if a user has Docker-formatted creds, they migrate them with `mvmctl image login <registry>` (new verb) which writes the keyring entry.
- Per-registry policy file at `~/.mvm/registry-policy.toml` controls per-registry trust posture, cosign requirements, allowed cross-origin redirects.

### Tests (W6 acceptance)

- Pull a cosigned image, verify against a project-pinned key — success.
- Pull a cosigned image, verify with no matching key — refuse, audit entry records "cosign-unmatched".
- Pull from a registry that requires a bearer token via `keyring` policy — token retrieved from the keyring fixture, request succeeds.
- Assert `~/.docker/config.json` is **not** opened in any test (strace lane on Linux).
- Sigstore: integration test against a sigstore-attested image in a CI-only fixture (gated by `MVM_COSIGN_SIGSTORE=1` to avoid network deps in default CI runs).

### W6 ships as

A PR that adds cosign + keyring + per-registry policy. Closes the trust-establishment story for plan 75.

---

## W7 — microvm → OCI export (bidirectional OCI story)

**Goal:** the second half of the strategic feature. `mvmctl image export <vm|template>` produces an OCI-spec-v1.1 image that other container runtimes can consume, with explicit and tested documentation of which security claims travel.

### CLI surface

- `mvmctl image export <vm|template> [--push <registry/repo:tag>] [--cosign] [--annotation key=value]`
- `mvmctl image push <registry/repo:tag>` (separate verb for explicit push step).

### Image contents

- **Base layer**: the rootfs.ext4 contents, unpacked into a tar layer. (We do *not* ship the ext4 image itself; we ship the contents.)
- **Sealed deps volume layer** (if the source had one): a tar layer containing the sealed `content/`, `sbom.cdx.json`, `fetch.log`, `cve.json`, `meta.json` — with the hash chain preserved.
- **OCI image config**: `entrypoint`, `cmd`, `env`, `workdir` derived from the workload's launch plan.
- **Annotations**:
  - `dev.mvm.claims` → URL or inline JSON of the `mvm-claims.json` document.
  - `dev.mvm.plan-digest` → the source ExecutionPlan's digest (admission chain root).
  - `dev.mvm.audit-head` → the audit chain head digest at export time.
  - `dev.mvm.sealed-volume-digest` → the deps volume's `meta.json` final hash, if present.
  - Standard OCI annotations: `org.opencontainers.image.title`, `…created`, `…source`.

### Claims-that-travel test

The PR ships a CI test `oci-export-claims-document` that:

1. Builds a baked microvm with a signed plan, audit chain, sealed deps volume, and dm-verity rootfs.
2. Exports it with `mvmctl image export --output /tmp/exported.tar`.
3. Inspects the exported image's `mvm-claims.json` annotation.
4. Asserts the annotation correctly reports: dm-verity = **does not travel**, signed-plan = **metadata only**, audit-chain = **does not travel**, sealed-volume = **travels**, cosign = travels if user signed export.
5. Re-imports the exported image via `mvmctl image pull` on a separate cache; asserts the deps volume verifies against its sealed `meta.json`.

### Tests (W7 acceptance)

- Round-trip: export a Nix-built microvm to OCI, reimport via `mvmctl image pull`, boot via `mvmctl up --image`. Compare original rootfs.ext4 content-hash to reimported-and-rebuilt rootfs.ext4 content-hash — equal modulo timestamps (OCI's standard reproducibility gotcha; mvm-oci has a "strip timestamps" mode).
- Push: export + push to a local fixture registry, then pull from a different cache root and reboot. End-to-end portability test.
- Claims annotation accuracy: every claim in the table is asserted by a CI gate.

### W7 ships as

A PR that completes the bidirectional OCI story. After W7, mvm is a first-class citizen in the container ecosystem — anything an OCI registry can consume, mvm can produce; anything mvm produces, an OCI registry can consume.

---

## Sequencing and cumulative deliverables

| Workstream | Lands | Cumulative state |
|---|---|---|
| W0 | This PR (specs only) | Plan + ADR ratified |
| W1 | Next PR | `mvm-oci` exists, not yet consumed |
| W2 | After W1 | mvm-oci can produce rootfs trees from images |
| W3 | After W2 | Stage 0 uses mvm-oci + libkrun. microsandbox still in tree but bypassed. |
| W4 | One release cycle after W3 ships | microsandbox **gone** from workspace |
| W5 | After W4 | User-facing OCI ingest shipped (closes plan 74 W1) |
| W6 | After W5 | Cosign + keyring trust shipped; `--prod` is defensible |
| W7 | After W6 | Bidirectional OCI shipped; mvm is a first-class container ecosystem citizen |

W0–W4 close the immediate problem (remove microsandbox). W5–W7 deliver the strategic feature (bidir OCI). The split is intentional: if W5–W7 stall, we still got rid of microsandbox; if W0–W4 takes longer than expected, W5–W7 don't slip waiting on infrastructure they don't need.

---

## Risks and mitigations

### R1 — Layer unpack CVE class
**Risk:** OCI layer unpack has a well-known CVE class (path traversal, symlink escape, hardlink-to-host, setuid surprise, xattr-based privilege carry). A subtle bug in `mvm_oci::layer::unpack` could let a malicious image escape the rootfs at unpack time.

**Mitigation:**
- Defense-in-depth: the unpacked tree is consumed inside libkrun, not on the host. Even a perfect rootfs-escape from inside the tar handler only escapes to a sandboxed scratch directory that the launcher discards.
- Dedicated fuzz lane on `mvm_oci::layer::unpack_one` (≥30 min per PR; daily long-running CIFuzz target).
- Adversarial fixture suite covering every documented CVE pattern.
- Default-deny policies for setuid, device nodes, xattrs.
- Plan 74 W1's risk table explicitly names this; plan 75 W2 ships the test gates that retire it.

### R2 — Trust-boundary widening if cosign is partial-coverage
**Risk:** Most registries don't ship cosign attestations. If we set `--prod` to require cosign, we lock users out of useful images. If we don't, `--prod` carries no integrity guarantee for unsigned images.

**Mitigation:**
- `--prod` requires cosign by default. Per-registry override at `~/.mvm/registry-policy.toml::prod_unsigned_allow = ["registry.example.com/team/*"]` lets ops explicitly admit unsigned registries with audit-chain recording.
- Documentation states clearly: an unsigned image admitted to prod is trusted by digest pin only; the registry's content-addressed digest is the integrity boundary.
- Default policy for `mvmctl up --image` is **dev**, not prod. Users opt into prod explicitly.

### R3 — Host-tool dependency at unpack time
**Risk:** W2's MVP shells out to host `mkfs.ext4` to allocate the rootfs image. That contradicts CLAUDE.md §"Host Nix is never used" in spirit (we're shelling out to a host binary for a build-time step).

**Mitigation:**
- W2's MVP is gated by `MVM_OCI_HOST_MKFS=1` and is **never** the default path.
- The default path is: unpack to a directory tree on the host, then **inside the libkrun Stage 0 VM** (which has `e2fsprogs` from its Nix closure already), run `mkfs.ext4` + `cp -a` to materialize the ext4 image. The host never invokes a filesystem tool.
- This adds one extra libkrun launch to Stage 0 (cheap; the VM is already running) but eliminates the host-tool dep.
- W3 ships the in-VM path; W2's MVP exists only for the W1+W2 integration tests that run before W3 is wired.

### R4 — Reproducibility of OCI imports
**Risk:** OCI layer tarballs are notoriously timestamp-dependent. Two pulls of the same image from a clean cache could produce non-bit-identical rootfs.ext4 outputs, masking integrity drift.

**Mitigation:**
- mvm-oci's unpacker has a "strip timestamps" mode (default-on) that zeroes mtime/atime/ctime on all unpacked entries.
- CI gate `oci-reproducibility` asserts byte-identical rootfs.ext4 on consecutive pulls.
- Content-address the rootfs.ext4 itself in the cache; consumers reference by hash, not by ref.

### R5 — Stage-0 base image pin maintenance
**Risk:** the pinned `nixos/nix:2.24.10@sha256:<digest>` constant in source is a security boundary. If it goes stale (upstream CVE in `nix`), we're vulnerable until someone notices and bumps. If we auto-bump, we bypass the explicit-PR control.

**Mitigation:**
- CI daily job `stage0-pin-refresh-check` queries Docker Hub for newer signed `nixos/nix` releases; opens a PR with the pin bump if a newer digest exists. The PR carries the upstream changelog and the diff.
- Human review of the PR before merge. Pin bump is never automatic on the trunk.
- Audit annotation on every Stage 0 launch records the in-use pin so a "this host is running an outdated Stage 0" check is one `mvmctl audit grep` away.

### R6 — Network egress at pull time vs. plan 73 Followup B.2.x
**Risk:** mvm-oci needs HTTPS egress to `registry.docker.io` and `production.cloudflare.docker.com` for Docker Hub. Plan 73 Followup B.2.x (mvm-egress-proxy) embeds a four-hostname allowlist (PyPI + npm + GitHub release objects). If we route Stage 0 through the same proxy, we need to extend the allowlist.

**Mitigation:**
- Stage 0 OCI pulls happen on the **host** (mvm-oci is a host-side library), not inside the builder VM. The egress proxy is a builder-VM-side artifact; it doesn't gate host-side egress.
- A host-side OCI registry allowlist lives at `~/.mvm/registry-policy.toml::allowed_registries = [...]` and is enforced by mvm-oci's network policy (W1). Default allowlist for source-checkout: `registry-1.docker.io`, `production.cloudflare.docker.com`, plus whatever the user adds.
- This is a separate policy surface from mvm-egress-proxy. They cover different layers (host pull-time vs. guest build-time).

### R7 — Dual-path during W3→W4 window
**Risk:** between W3 (mvm-oci wired, microsandbox still in tree) and W4 (microsandbox deleted), both code paths exist. A subtle regression in mvm-oci could silently fall back to microsandbox if a `cfg(feature = "contributor-bootstrap")` guard is wrong.

**Mitigation:**
- W3's `build_image_via_libkrun_oci` is the **default** path on every build, regardless of feature flag.
- The microsandbox path is gated behind `MVM_OCI_FALLBACK_MICROSANDBOX=1` (env var, not feature flag) — explicit opt-in for emergency revert. Not on by default. Not testable without the env var.
- CI lane asserts that with no env vars set, `cargo run -- dev up` strace shows zero hits on microsandbox's runtime files.
- W4 deletes the env var path along with the dep.

### R8 — cargo-fuzz cost on every PR
**Risk:** ≥30 min fuzz lanes per PR multiply CI cost. Either we ship more PRs more slowly, or we relax the gate.

**Mitigation:**
- The 30-min gate runs only on PRs that touch `crates/mvm-oci/**`. Other PRs run a quick (1 min) corpus replay against the existing fuzz corpus.
- A separate CIFuzz daily lane runs the long-form fuzz; new findings file issues automatically.
- This is the same pattern plan 28 W4.2 uses for vsock framing fuzz.

### R9 — Plan 74 W0/W1 trajectory
**Risk:** plan 74 W1 (OCI image ingest) was the original owner of user-facing OCI pull. Plan 75 absorbs it. If plan 74's other workstreams (W2 network policy, W3 secret placeholders) assume W1 is happening as plan 74 specifies, plan 75 needs to honor those assumptions.

**Mitigation:**
- Plan 75 W0 lands an explicit "supersedes plan 74 W1" note in both this plan and plan 74's status block. Plan 74's W2–W6 read mvm-oci as their OCI-ingest substrate.
- Plan 75 W5 closes plan 74 W1's acceptance criteria byte-for-byte (cited in W5's "Tests" section), so the supersession is verifiable.

---

## Open questions (for the implementation phase to answer)

1. **OCI image index handling.** Docker Hub multi-arch images return an `oci-index` manifest; plan 75 W1 picks the matching arch. Do we honor `os` only, or also `os.version` and `variant`? For `linux/arm64/v8` vs `linux/arm64` — W1 should land an explicit selector. (Recommend: match `os` + `architecture` exactly, refuse on `variant` ambiguity.)
2. **Layer mediaType coverage.** OCI v1.1 introduces new mediaTypes (`application/vnd.oci.image.layer.v1.tar+zstd` and friends). Which do we support at W2? (Recommend: support both `gzip` and `zstd` from day one; refuse unknown mediaTypes.)
3. **Cache GC policy.** OCI layers can be very large; the cache grows monotonically without GC. (Recommend: reference-count layers by which manifests still reference them; `mvmctl image rm <ref>` decrements; `mvmctl cache prune` GCs orphans.)
4. **Pin format for Stage 0 base.** Constant in Rust source, or a sidecar TOML in `nix/images/`? (Recommend: TOML, so the daily refresh CI job can bump it without parsing Rust.)
5. **Cosign sigstore vs. project-key parity.** Sigstore's fulcio + rekor model is the long-term answer for ecosystem-wide trust; project-pinned keys are the short-term answer for our own published artifacts. W6 ships both; which is the *default*? (Recommend: project-pinned for `dev.mvm.*` annotations; sigstore opt-in per registry.)

---

## Why this plan is worth doing now

Three things converge:

1. **The 4 GiB overlay is going to keep biting.** Every time someone adds a `rustPlatform.buildRustPackage`, a `python3Packages.*`, or a moderate-sized binary to the builder-vm flake, Stage 0 risks overflow. The 2026-05-14 regression is the second time it's happened (the first was the impetus for plan 72). Continuing to work around it is paying recurring rent on a structural cost.
2. **Plan 74 W1 wants the same primitive.** Building OCI ingest twice — once for Stage 0, once for user-facing pull — is engineering waste. Plan 75 builds it once and uses it for both.
3. **The bidirectional OCI story is strategic.** Today mvm is a self-contained universe — you install mvmctl, you build with our flakes, you run our images. After plan 75, mvm is a container-ecosystem peer: anything Docker Hub or GHCR or ECR can serve, you can run; anything you build, those registries can serve. That widens addressable workloads (Plan 74's user-facing claim) **and** widens addressable deployment targets (W7's export). Both directions are mvm being more useful to more people without rewriting its security or reproducibility properties.

The cost is real (multi-week, multi-PR, multi-CI-lane). The upside is structural — one less dependency that contradicts the project's stated identity, plus the strategic feature delivered as a side effect.
