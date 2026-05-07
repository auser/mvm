# Plan 55 — crosvm backend (deferred)

> Status: deferred
> Decision recorded: 2026-05-07 (plan 53)
> Trigger to revisit: see "Trigger conditions" below

## Why this plan exists

Plan 53 (cross-platform roadmap) evaluated crosvm as a possible new backend. After weighing the fit against our user base, we **deferred** it. This file documents the assessment so a future contributor doesn't have to redo the analysis.

## What crosvm is

crosvm is Google's KVM-based VMM, written in Rust, BSD-3-Clause licensed, maintained primarily for Chrome OS (where it runs Linux apps via the Crostini container) and the Android emulator (post-QEMU replacement). Like Firecracker and Cloud Hypervisor, it builds on the rust-vmm crate ecosystem (`vm-memory`, `kvm-ioctls`, `virtio-queue`, etc.).

Mature, production-quality, well-tested. No question about whether it works.

## Why we deferred it

Crosvm is a *good* VMM. The question isn't quality; it's whether it solves a problem mvm currently has. Three filters:

1. **Does it cover a host platform we don't already cover?**
   No. crosvm is KVM-on-Linux only. Plan 53's libkrun (Plan E) covers the cross-platform niche (Linux KVM + macOS Hypervisor.framework), and Firecracker already covers Linux+KVM at Tier 1.

2. **Does it solve a feature gap that Firecracker can't?**
   Marginal. crosvm has good wayland/audio/GPU virtualization for Chrome OS workloads — none of which mvm targets. Cloud Hypervisor (Plan F, also deferred) covers nested KVM and GPU passthrough more explicitly when those are needed.

3. **Does it have a passionate user base asking for it?**
   Not today. No issues, no community asks.

Adding crosvm without one of those filters being met means more code to maintain, more CI lanes to keep green, more docs to write — for no measurable gain.

## Trigger conditions to revisit

Any one of these would warrant a re-evaluation:

1. **A user opens an issue specifically requesting crosvm support** (likely tied to a Chrome OS or Android-emulator workload).
2. **mvm wants to support Chrome OS as a host platform.** crosvm is the canonical VMM there.
3. **Both Firecracker and libkrun become unmaintained** (extremely unlikely; both have multiple corporate backers).

## What "implementation if pulled" would look like

For estimation only; not commitments.

- **Files**: same shape as Plan F (Cloud Hypervisor) — HTTP-API-over-socket lifecycle wrapper, new `AnyBackend` variant, CLI flag, bootstrap install, doctor check.
- **Effort**: ~1 sprint.
- **Security posture**: comparable to Cloud Hypervisor (rust-vmm based, similar TCB size). Would land as Tier 2 with the same per-claim notes as CH if accepted.

## References

- Plan 53 §"Implementation plans → Plan G" — the high-level summary.
- Plan E (libkrun) — the embeddable cross-platform niche we *did* fill.
- Plan F (`54-cloud-hypervisor-deferred.md`) — the related "considered, rejected" decision for the feature-rich-VMM niche.
