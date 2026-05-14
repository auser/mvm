# ADR 049 — `mvm-oci` supersedes microsandbox; bidirectional OCI is a first-class feature

**Status**: Proposed (ratifies plan 75; flips to Accepted when plan 75 W4 lands and `cargo metadata` shows zero microsandbox crates in the workspace closure)
**Date**: 2026-05-14
**Supersedes**: ADR-046 §"Stage 0 for contributors without host Nix" (microsandbox is no longer kept behind `--features contributor-bootstrap`; it is removed outright)
**Cross-refs**: ADR-002 (microvm security posture), ADR-046 (builder VM via libkrun), Plan 74 W1 (OCI image ingest — subsumed by plan 75 W5), Plan 75 (this ADR's implementation plan), `specs/gap-analysis-vs-microsandbox.md`

## Context

ADR-046 settled the user-facing builder VM path: libkrun (macOS) / Firecracker (Linux), with microsandbox demoted to a contributor-only Stage 0 escape hatch behind `--features contributor-bootstrap`. That demotion was the smallest move that unblocked plan 72's main thrust, but it left a residual structural problem:

1. **The microsandbox dep contradicts CLAUDE.md.** CLAUDE.md §"Firecracker-only" reads: *"no Docker/containers. Builds run Nix inside the Lima VM."* The retained microsandbox path pulls `docker.io/nixos/nix:2.24.10` to run `nix build` inside a Docker-style overlay; the contradiction is documented but not resolved.
2. **The 4 GiB overlay keeps biting.** Plan 72's "the Stage 0 closure is small enough to fit" assumption depends on the builder VM rootfs never gaining a Rust binary, a Python tool, or anything that vendors a cargo lockfile. That assumption broke 2026-05-13 (plan 73 Followup B.2 added `uv` + `pnpm` + `pip-audit`) and broke again 2026-05-14 (plan 73 Followup B.2.x added a second `rustPlatform.buildRustPackage`). Each fix is a trim-the-flake exercise; each fix invites the next regression.
3. **Plan 74 W1 wants the same primitive.** "`mvmctl image pull <ref>` materializes OCI images into microvm artifacts" requires exactly what Stage 0 needs: HTTPS-to-a-registry, manifest fetch, digest verification, layer unpack to ext4, hand-off to a launcher. Building that primitive twice (once inside microsandbox for Stage 0, once again for user-facing pull) is engineering waste.
4. **The bidirectional OCI story is strategic.** OCI → microvm widens addressable workloads; microvm → OCI widens addressable deployment targets (Kubernetes, ECS, Cloud Run, plain Docker). Together they make mvm a peer in the container ecosystem rather than a parallel universe. Microsandbox is on the wrong side of this — it's a *consumer* of OCI, not a producer.

The right shape is: build `mvm-oci` once as a host-side Rust library, use it for Stage 0 *and* for `mvmctl image pull`, and ship `mvmctl image export` for the export half. After that, microsandbox has no remaining justification.

## Decision

### 1. Introduce `crates/mvm-oci/` as the OCI ingest primitive

A new crate that:

- Talks OCI Distribution v2 over HTTPS (rustls-only — no native-tls, no openssl).
- Fetches manifests, configs, and layers; verifies every byte against the manifest's content-addressed digest.
- Unpacks layers to a directory tree with explicit handling of every OCI tar feature (whiteouts, symlinks with escape refusal, hardlinks within-layer-only, xattr allow-list, device-node refuse-list, setuid/setgid policy).
- Emits an ext4 rootfs (inside the builder VM, not on the host — no host `mkfs.ext4` dependency on the default path).
- Exposes a typed `Client::pull` / `Client::resolve` API that callers (Stage 0 boot, `mvmctl image pull`, future `mvmctl image export` round-trip tests) all share.

`mvm-oci` is a host-side library. It is **not** a runtime, **not** a daemon, **not** an OCI runtime spec implementation. It pulls OCI images; libkrun/Firecracker launches the unpacked rootfs.

### 2. Stage 0 becomes `mvm-oci` + libkrun

`build_image_via_microsandbox` (`crates/mvm-cli/src/commands/env/apple_container.rs:1736`) is replaced by `build_image_via_libkrun_oci`. New flow:

```
mvm_oci::Client::pull(docker.io/nixos/nix:2.24.10@sha256:<pin>)
    → unpacked layers in ~/.cache/mvm/oci/blobs/sha256/
mvm_oci::rootfs::build(pulled, ~/.cache/mvm/oci/stage0/<digest>/)
    → ext4 rootfs (built inside libkrun, not on host)
LibkrunBuilderVm::run_build(BuilderJob::Flake { ... }, BuilderMounts { stage0_root: Some(...), ... })
    → vmlinux + rootfs.ext4 for the builder VM image
```

Same `BuilderJob` / `BuilderMounts` / `BuilderArtifacts` types as today; the `mvm-cli` consumer doesn't change shape.

### 3. Microsandbox is removed in full

After `build_image_via_libkrun_oci` bakes for one release cycle, plan 75 W4 deletes:

- `dep:microsandbox` in `crates/mvm-backend/Cargo.toml`.
- The `contributor-bootstrap` Cargo feature across the workspace.
- `MicrosandboxBuilderVm` and its tests.
- `build_image_via_microsandbox` and the `#[cfg(feature = "contributor-bootstrap")]` blocks that call it.
- Every doc string that references microsandbox or the 4 GiB overlay (replaced with current-truth language).

The deletion is verifiable: `cargo metadata --no-default-features --features ''` shows zero `microsandbox*` crates, and `grep -rni microsandbox` returns hits only in `specs/` (history) and CHANGELOG entries.

### 4. User-facing OCI ingest ships behind the same primitive (closes plan 74 W1)

`mvmctl image pull <ref>`, `mvmctl image ls`, `mvmctl image rm`, `mvmctl image inspect`, `mvmctl up --image <ref>` are CLI verbs over `mvm_oci::Client`. Templates produced by `image pull` integrate with the existing template store under `kind = "oci"`.

### 5. Bidirectional OCI ships as a sequenced workstream

`mvmctl image export <vm|template> [--push <registry/repo:tag>] [--cosign]` produces an OCI-spec-v1.1 image with explicit and tested documentation of which mvm security claims travel with the exported artifact and which do not. The "claims-that-travel" table is shipped as a CI-asserted annotation, not just docs.

## Security implications

### Claims that update

CLAUDE.md §"Security model" claim 6 (today: *"Pre-built dev image is hash-verified"*) extends to **every** OCI image — Stage 0 base, user-pulled image, mvm-published builder VM artifact — verified by content-addressed digest before any byte is consumed, with tampered layers failing closed. Implementation is shared (`mvm_oci::digest::verify_streaming`); proof is one CI lane instead of three.

### New claim 10

**"OCI image provenance is recorded in the admission audit chain."** Every `mvmctl up --image <ref>` admission emits an audit-chain entry with registry, resolved digest, layer digest list, and (if present) cosign attestation. `mvmctl audit verify` proves the chain.

### Trust-boundary changes

| Boundary | Today | After plan 75 |
|---|---|---|
| Stage 0 base image trust root | `docker.io/nixos/nix:2.24.10` via microsandbox, anon auth, digest unenforced | `docker.io/nixos/nix:2.24.10@sha256:<pin>` via mvm-oci, anon auth, digest pinned in source, verified at fetch time. Bump-the-pin is a deliberate PR. |
| User-facing OCI pull trust root | Not supported | `mvm-oci` HTTPS-only; system trust store; optional `~/.mvm/registry-ca.pem` extra-CA; no `~/.docker/config.json` read by default. |
| Cosign verification | Not supported anywhere | Verified when present (plan 75 W6); absence does not block (registries are partial coverage); invalid signature refuses. Configurable per-registry policy. |
| Layer unpack security surface | microsandbox internals (opaque, not in our codebase) | `mvm_oci::layer::unpack` — explicit handling for every OCI tar feature, fuzz lane, adversarial fixture suite |
| Host filesystem at unpack time | OCI image layers extracted by microsandbox into its own overlay (not directly visible to us) | Unpacked to a directory tree on the host; ext4 image assembled **inside the libkrun Stage 0 VM** so no host `mkfs.ext4` dep on default path |
| Audit chain coverage | Plans, dep volumes, ExecutionPlan | Plans, dep volumes, ExecutionPlan, **plus OCI image provenance (claim 10)** |

### Threat model additions

These extend ADR-002's threat model.

- **T-OCI-1: Compromised upstream registry.** Stage 0 pin is in source; cosign verification for user pulls when configured; HTTPS pinning to system CA bundle.
- **T-OCI-2: Layer-unpack CVE class** (CVE-2019-14271-style — path traversal, symlink escape, hardlink-to-host, setuid escalation, xattr privilege carry). Deny-by-default unpacker; cargo-fuzz lane; adversarial fixtures covering each pattern; defense-in-depth via libkrun isolation (the unpacked rootfs runs in a microVM, not on the host).
- **T-OCI-3: Tag mutation race.** Production policy (`--prod`) rejects unpinned-by-digest references; dev mode resolves to a digest at first pull and caches by digest.
- **T-OCI-4: TLS substitution / DNS rebinding at pull time.** HTTPS-only; system CA trust by default; SNI must match request hostname; same-origin redirect policy with per-registry opt-in for cross-origin (Docker Hub's `production.cloudflare.docker.com` redirect requires explicit allow); no DoH/DoT bypass.
- **T-OCI-5: Supply-chain via Stage 0 base image.** Pinned digest in source; CI daily refresh job opens a PR with the bump and upstream changelog; human review before merge; audit annotation records the in-use pin on every Stage 0 launch.

### Defense-in-depth at the launcher

Even if `mvm-oci`'s unpacker has a CVE, the unpacked rootfs is consumed by libkrun/Firecracker — it boots in a microVM, not on the host. A rootfs-level escape from a malicious tar still has to escape libkrun before reaching the host. This is the standard mvm posture for guest workloads; plan 75 inherits it.

### What does **not** change

- CLAUDE.md §"Host Nix is never used by mvmctl" stays load-bearing. mvm-oci has zero nix dep; it speaks HTTPS to registries.
- CLAUDE.md §"No SSH in microVMs, ever" — mvm-oci does not synthesize sshd or any service in the rootfs it emits; it unpacks what the registry returned.
- ADR-046's two acquisition paths (source checkout vs. installed binary) remain intact. What changes is Stage 0 in the source-checkout path: previously `microsandbox + nixos/nix OCI`, after plan 75 `mvm-oci + nixos/nix OCI + libkrun`. Same trust root; different code path; one fewer dep that contradicts CLAUDE.md.
- ADR-002 §W5.1 (image hash verification) applies to OCI ingest with the same streaming SHA-256 path used today for `download_dev_image`.

### Claims that travel with exported OCI images (microvm → OCI)

This is the half of the bidir story that the docs need to be precise about. Plan 75 W7 ships a CI gate asserting the `mvm-claims.json` annotation correctly reports each row:

| Claim | Travels in exported OCI? | Why |
|---|---|---|
| dm-verity rootfs (claim 3) | No | OCI consumers' boot path doesn't honor dm-verity; the kernel cmdline isn't ours |
| Signed ExecutionPlan (claim 8) | Metadata only (annotation), not enforced | Enforcement requires `mvm-supervisor` at the consumer; annotation enables audit |
| Chain-signed audit log (claim 8) | No | Audit chain is host-state; the export annotation lists the chain head digest so a consumer can request the chain out-of-band |
| Sealed deps volume (claim 9) | Yes (as separate annotated layer or sidecar) | Sealed volume is content-addressed; layer ships with `meta.json` chain intact |
| OCI provenance (new claim 10) | N/A — this is an ingest-side claim | Export emits annotations; consumer's audit chain (if any) records its own provenance entry on import |
| Verified provenance (cosign on export) | Yes (if user opts in) | `mvmctl image export --cosign` signs the exported image; export does not auto-sign |

## Consequences

### Positive

- **One dep removed from the contradiction with CLAUDE.md.** No path inside mvm pulls Docker-style OCI for non-OCI-feature purposes after plan 75 W4.
- **One primitive built once, used twice.** Stage 0 and user-facing OCI ingest share `mvm_oci::Client`. Bugs found in one path are fixed for both.
- **The bidirectional OCI feature ships as a side effect.** Plan 75 W5 closes plan 74 W1; plan 75 W7 closes the export half. mvm becomes a container-ecosystem peer without a separate effort.
- **The 4 GiB overlay stops biting.** Stage 0's rootfs is built inside libkrun, where disk size is host-configurable per-build, not library-internal.
- **Workspace dep closure shrinks.** Microsandbox's ~40 transitive crates leave with W4.
- **New security claim ships with CI proof.** Claim 10 (OCI provenance in audit chain) closes the audit gap for the new ingest surface.
- **Defense-in-depth at the unpacker.** Adversarial fixtures + cargo-fuzz + explicit policies for every OCI tar feature reduce the layer-unpack CVE class to a documented, tested surface.

### Negative

- **Multi-week effort.** Plan 75 sequences seven workstreams; realistic estimate is 4–8 weeks of focused work. Plan 75 §"Why this plan is worth doing now" defends the cost.
- **One new dep family.** rustls + reqwest + oci-spec + sha2 + a test-time axum fixture. All Rust, all in the existing dependency ecosystem; none of them contradict CLAUDE.md.
- **Plan 74 W1 trajectory shifts.** Plan 74's other workstreams (W2 network policy, W3 secret placeholders, W4 SDK lifecycle, W5 cold-start budgets, W6 filesystem backends) read mvm-oci as their OCI-ingest substrate. Plan 75 W0 lands an explicit "plan 75 supersedes plan 74 W1" note so the dependency direction is unambiguous.
- **Trust boundary widens to include cosign tooling.** Plan 75 W6 adds cosign verification + project-pinned-key parsing. The pubkey storage at `~/.mvm/cosign-pubkeys/` and per-registry policy at `~/.mvm/registry-policy.toml` are new on-disk security surfaces; both are mode-0600-or-stricter and live under the existing `~/.mvm` posture (W1.5).
- **OCI mediaType drift risk.** OCI v1.1 introduced new mediaTypes; we support `gzip` + `zstd` from day one and refuse unknown. New mediaTypes that ship later require an explicit code change. This is the right default — silent admission of unknown encodings is exactly the layer-unpack CVE pattern.

### Neutral

- ADR-013 §"Execution backend selection" is unchanged. Linux + KVM → Firecracker; macOS / Windows / no-KVM → libkrun. microsandbox stops being an execution backend at all (it was already optional after plan 72; plan 75 removes the dep).
- ADR-002 §W5.1 (image hash verification) applies to OCI ingest with no shape change — same streaming SHA-256 path.
- The existing `crates/mvm-backend/src/docker.rs` Docker-load fallback (run a Nix-built rootfs as a Docker container) is **not** part of plan 75. It's a separate feature with a separate purpose; it stays as-is.

## Fallback / escape hatch

Plan 75 W3 ships `build_image_via_libkrun_oci` as the **default** Stage 0 path while microsandbox is still in the tree. The microsandbox path is reachable only via `MVM_OCI_FALLBACK_MICROSANDBOX=1` (env var, not feature flag) for one release cycle of emergency revert capability. After that cycle, plan 75 W4 deletes the env var path along with the dep.

If `mvm-oci` (W1+W2) hits a fundamental blocker — e.g. a layer-unpack CVE class we can't tractably defend against in pure Rust — the fallback is a *vendored* OCI layer unpacker derived from a small audited library (e.g. `oci-distribution`'s unpacker, or a minimal vendored cut of containerd's `archive/tar` adapter). This is a code-shape fallback, not a strategy fallback; plan 75's direction holds regardless.

## Open questions (resolved by plan 75 implementation phases)

1. **OCI image index handling** — Docker Hub multi-arch images return an `oci-index` manifest. Plan 75 W1 picks the matching arch; specifics in plan 75 §"Open questions" Q1.
2. **Layer mediaType coverage** — `gzip` + `zstd` from day one; refuse unknown. Plan 75 §"Open questions" Q2.
3. **Cache GC policy** — reference-count layers by manifest references; `mvmctl cache prune` GCs orphans. Plan 75 §"Open questions" Q3.
4. **Pin format for Stage 0 base** — sidecar TOML in `nix/images/` so the daily refresh CI job can bump it without parsing Rust. Plan 75 §"Open questions" Q4.
5. **Cosign sigstore vs. project-key parity** — project-pinned for `dev.mvm.*` annotations; sigstore opt-in per registry. Plan 75 §"Open questions" Q5.
