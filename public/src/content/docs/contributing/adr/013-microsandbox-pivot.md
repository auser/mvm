---
title: "ADR-013: Pivot to microsandbox + libkrun + microvm.nix"
description: Architecture Decision Record for the cross-platform microVM pivot — microsandbox as the macOS/Windows execution path, Firecracker preferred on Linux, microvm.nix as the Nix foundation.
---

## Status

Proposed. Implementation tracked in [Plan 60](https://github.com/auser/mvm/blob/main/specs/plans/60-mvm-microsandbox-migration.md). Phase 0 + Phase 1 deliver the build/exec pivot; subsequent phases compose on top.

## Context

The previous iteration of mvm used Lima as the macOS dev-VM hop and Firecracker as the production hypervisor on Linux. Two pain points motivated the pivot:

1. **macOS dev experience was indirect** — every guest action traversed `host → Lima Ubuntu → Firecracker microVM`. Boot times were dominated by Lima warm-up; first-launch UX was brittle.
2. **Build pipeline lacked portability** — Nix builds ran inside ephemeral Firecracker builder VMs, gated by KVM availability. macOS and Windows hosts had no clean path.

## Decision

Three coupled choices:

1. **microsandbox** (Apache-2.0, libkrun-backed) becomes the **builder** and the **macOS/Windows execution path**. libkrun gives us native Hypervisor.framework on macOS and KVM on Linux without a wrapping Lima VM.
2. **Firecracker** stays as the preferred Linux production execution path because of its smaller attack surface, faster cold boot, and existing security work (jailer, dm-verity, seccomp tier).
3. **microvm.nix** (MIT) becomes the Nix-flake foundation for microVM image generation. It abstracts Firecracker / Cloud Hypervisor / QEMU / crosvm / kvmtool / stratovirt as a NixOS module — adding a new backend later is a config change, not a kernel rewrite.

**Lima is dropped entirely.** The macOS path is microsandbox-direct; no intermediate Linux VM.

### Backend selection ladder

```
1. KVM available           → Firecracker (Linux production target)
2. has_microsandbox()      → MicrosandboxBackend (macOS + non-KVM Linux)
3. macOS Container         → AppleContainerBackend (legacy; scheduled for removal)
4. raw libkrun             → LibkrunBackend (legacy; scheduled for removal)
5. Docker                  → DockerBackend (Tier 3 fallback; banner emitted)
6. Firecracker via Lima    → legacy macOS fallback
```

`mvmctl run --hypervisor microsandbox <flake>` always selects microsandbox explicitly regardless of the host's KVM status.

## Non-goal: OCI / container images

**mvm is microVMs, not containers.** Even though microsandbox's API exposes both — `RootfsSource::Oci(reference)` for OCI image pulls and `RootfsSource::DiskImage { path, format, fstype }` for raw disk images — we deliberately use **only the `DiskImage` path**.

Why this is a stated invariant, not a default:

1. **Architectural commitment.** The project's value prop is microVM isolation backed by Nix-built rootfs images. OCI brings registry pulls, layered images, image index resolution, and a different trust model — none of which we want in the trust boundary.
2. **Reproducibility.** Nix-built rootfs images are byte-reproducible given the same flake inputs (gated in CI). OCI images resolve through a registry, can be re-tagged, and don't carry the same guarantees by construction.
3. **Trust boundary minimalism.** Pulling from an OCI registry adds an external network dependency to the boot path. The microVM path is offline-by-default once the rootfs is built.
4. **Runtime path consistency.** The bridge between our `.ext4` rootfs files and microsandbox's `.disk()` builder (a sibling `.raw` hard-link with explicit `fstype("ext4")`) keeps the disk path entirely host-local. No registry, no auth, no pull cache.

## Consequences

### Positive

- Single fewer hop on macOS (`host → microsandbox → guest`) — faster boot, cleaner UX.
- microvm.nix gives multi-hypervisor portability for free.
- Builder pipeline runs on every host class.
- Reduced surface: no more Lima-specific code paths.

### Negative

- Adds a third-party dep (microvm.nix) to the build trust boundary — pinned by hash and CI-audited (`xtask audit-flake`).
- Some Linux-specific guarantees (dm-verity at boot, seccomp tier "strict") only hold on the Firecracker path. The microsandbox path uses image-hash-on-load + HMAC chain instead. Documented in the per-backend tier matrix in [ADR-002](https://github.com/auser/mvm/blob/main/specs/adrs/002-microvm-security-posture.md).

### Fallback (named explicitly)

If a microvm.nix per-bump audit (`xtask audit-flake`) surfaces a security regression we can't accept, fall back to the previous iteration's hand-rolled NixOS modules. The fallback is a **named, ready-to-execute escape hatch**, not just a sentence in this document. Cost: ~5K LOC of NixOS-module maintenance returns to our scope. Benefit: smaller trust boundary.

## Alternatives considered

- **Keep Lima as a fallback** — rejected. Maintains a code path that doesn't get exercised in the pivot's primary use case. Either Lima is good enough to be the macOS path (it isn't, per UX measurements) or it's dead code.
- **Cloud Hypervisor as primary** — rejected for now. CH is heavier than Firecracker and lacks the existing security work; revisit when GPU passthrough (VFIO) is needed (covered in ADR-030 in the spec tree).
- **Hand-rolled Nix flake (no microvm.nix)** — rejected. The previous iteration's hand-rolled flake was ~5000 LOC of NixOS module work; microvm.nix replaces most of that and is actively maintained.

## Threat model impact

- **microvm.nix** as a third-party dep widens the supply-chain surface. Mitigated by hash-pinning in `flake.lock`, CI re-audit on every bump, and reproducibility double-build.
- **microsandbox** is itself a third-party dep. Same mitigation.
- The per-backend tier matrix from ADR-002 is updated: Firecracker tier remains "strict"; microsandbox tier is "standard" until parity work lands (post-Phase 6).

## Related

- [Plan 60: mvm-microsandbox migration](https://github.com/auser/mvm/blob/main/specs/plans/60-mvm-microsandbox-migration.md) — full implementation roadmap
- [ADR-002: microVM security posture](https://github.com/auser/mvm/blob/main/specs/adrs/002-microvm-security-posture.md) — per-backend tier matrix
- [ADR-014: VmBackend single trait](https://github.com/auser/mvm/blob/main/specs/adrs/014-vmbackend-single-trait.md) — the trait surface microsandbox implements
- [ADR-031: Cross-platform strategy](https://github.com/auser/mvm/blob/main/specs/adrs/031-cross-platform-strategy.md) — Linux native, macOS native, Windows Tauri-only
