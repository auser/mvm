---
title: "ADR-013: Pivot to microsandbox + libkrun + microvm.nix; drop Lima"
status: Proposed
date: 2026-05-07
related: ADR-002 (security posture), ADR-014 (VmBackend trait), plan 60-mvm-microsandbox-migration
---

## Status

Proposed. Implementation tracked in `specs/plans/60-mvm-microsandbox-migration.md`. Phase 0 + Phase 1 deliver the build/exec pivot; subsequent phases compose on top.

## Context

The previous iteration of `mvm` (at `../mvm`) used Lima as the macOS dev-VM hop and Firecracker as the production hypervisor on Linux. Two pain points motivated the pivot:

1. **macOS dev experience was indirect**: every guest action traversed `host → Lima Ubuntu → Firecracker microVM`. Boot times were dominated by Lima warm-up; first-launch UX was brittle.
2. **Build pipeline lacked portability**: Nix builds ran inside ephemeral Firecracker builder VMs, gated by KVM availability. macOS and Windows hosts had no clean path.

The new direction:

- **microsandbox** (Apache-2.0, libkrun-backed) becomes the **builder** and the macOS/Windows execution path. libkrun gives us native Hypervisor.framework on macOS and KVM on Linux without a wrapping Lima VM.
- **Firecracker** stays as the preferred Linux production execution path because of its smaller attack surface, faster cold boot, and existing security work (jailer, dm-verity, seccomp tier).
- **microvm.nix** (MIT) becomes the Nix-flake foundation for microVM image generation. It abstracts Firecracker / Cloud Hypervisor / QEMU / crosvm / kvmtool / stratovirt as a NixOS module — adding a new backend later is a config change, not a kernel rewrite. **Fallback path**: if the per-bump audit (`xtask audit-flake`) of microvm.nix surfaces a security regression we can't accept, we fall back to the previous iteration's hand-rolled NixOS modules in `../mvm/nix/`. The fallback is a **named, ready-to-execute escape hatch**, not just an ADR sentence.
- **Lima is dropped entirely.** The macOS path is microsandbox-direct; no intermediate Linux VM.

## Decision

1. **Builder**: microsandbox-backed Nix builds (`mvm-build/src/pipeline/microsandbox.rs`); persistent warm-pool per tenant (ADR-015).
2. **Execution backend selection** at runtime:
   - Linux + `/dev/kvm` available → Firecracker
   - macOS / Windows / Linux without KVM → microsandbox (libkrun)
3. **Image generation**: extend microvm.nix's NixOS module with our security overlay (W2.1 per-service uids, W2.4 seccomp tiers, W3 dm-verity, W2.2 read-only `/etc`).
4. **Drop Lima** from the codebase; no fallback path.

## Consequences

**Positive**:
- Single fewer hop on macOS (host → microsandbox → guest) — faster boot, cleaner UX.
- microvm.nix gives multi-hypervisor portability for free.
- Builder pipeline runs on every host class.
- Reduced surface: no more Lima-specific code paths.

**Negative**:
- Adds a third-party dep (microvm.nix) to the build trust boundary — pinned by hash and CI-audited (`xtask audit-flake`).
- Some Linux-specific guarantees (dm-verity at boot, seccomp tier "strict") only hold on the Firecracker path. The microsandbox path uses image-hash-on-load + HMAC chain instead. Documented in the per-backend tier matrix in ADR-002.
- Loss of the Lima dev-VM means macOS users without microsandbox installed get a clearer error instead of a working but slow path.

**Neutral**:
- mvmd's facade contract (`mvmctl::core`, `mvmctl::runtime::shell`, etc.) is unaffected — this is a backend swap, not a contract change.

## Non-goal: OCI / container images

**mvm is microVMs, not containers.** Even though microsandbox's API
exposes both — `RootfsSource::Oci(reference)` for OCI image pulls and
`RootfsSource::DiskImage { path, format, fstype }` for raw disk
images — we deliberately use **only the `DiskImage` path**.

Why this is a stated invariant, not a default:

1. **Architectural commitment.** The project's value prop is microVM
   isolation backed by Nix-built rootfs images. OCI brings registry
   pulls, layered images, image index resolution, and a different
   trust model — none of which we want in the trust boundary.
2. **Reproducibility.** Nix-built rootfs images are byte-reproducible
   given the same flake inputs (we gate this in CI). OCI images
   resolve through a registry, can be re-tagged, and don't carry the
   same guarantees by construction.
3. **Trust boundary minimalism.** Pulling from an OCI registry adds
   an external network dependency to the boot path. The microVM
   path is offline-by-default once the rootfs is built.
4. **Runtime path consistency.** The bridge between our `.ext4`
   rootfs files and microsandbox's `.disk()` builder (a sibling
   `.raw` hard-link with explicit `fstype("ext4")`) keeps the disk
   path entirely host-local. No registry, no auth, no pull cache.

**What this means for code review:** any PR that introduces
`RootfsSource::Oci`, `microsandbox::RegistryAuth`, OCI image
references, or related types is reviewed against this invariant.
The exception is the future `mvm-cve` crate (plan 60 §"Roadmap
support") which may parse OCI artifact metadata as input to the
CVE roller — that's a metadata path, not a runtime path.

## Alternatives considered

- **Keep Lima as a fallback**: rejected. Maintains a code path that doesn't get exercised in the pivot's primary use case. Either Lima is good enough to be the macOS path (it isn't, per UX measurements) or it's dead code.
- **Cloud Hypervisor as primary**: rejected for now. CH is heavier than Firecracker and lacks the existing security work; revisit when GPU passthrough (VFIO) is needed (ADR-030).
- **Hand-rolled Nix flake (no microvm.nix)**: rejected. The previous iteration's hand-rolled flake was ~5000 LOC of NixOS module work; microvm.nix replaces most of that and is actively maintained.

## Threat model impact

- **microvm.nix** as a third-party dep widens the supply-chain surface. Mitigated by hash-pinning in `flake.lock`, CI re-audit on every bump, and reproducibility double-build.
- **microsandbox 0.4.5** is itself a third-party dep. Same mitigation.
- The per-backend tier matrix from ADR-002 is updated: Firecracker tier remains "strict"; microsandbox tier is "standard" until parity work lands (post-Phase 6).

## Compliance impact

- SOC 2: positive — narrower scope (one fewer trust boundary on macOS).
- PCI: neutral — neither backend is PCI-certified out of the box.
- HIPAA: neutral.
- FedRAMP/FIPS: future — neither backend ships FIPS 140-3 crypto today.
