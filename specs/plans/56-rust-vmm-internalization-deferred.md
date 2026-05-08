# Plan 56 — rust-vmm internalization (deferred — rejected for now)

> Status: deferred
> Decision recorded: 2026-05-07 (plan 53)
> Trigger to revisit: see "Trigger conditions" below

## Why this plan exists

Plan 53 (cross-platform roadmap) considered replacing our shell-out-to-the-`firecracker`-binary approach with direct linkage to the rust-vmm crate ecosystem. After weighing the costs and the lack of a concrete pull factor, we **rejected** it for now. This file documents the reasoning.

## What "rust-vmm internalization" means

rust-vmm is a collection of Rust crates (`kvm-ioctls`, `vm-memory`, `virtio-queue`, `vhost`, `linux-loader`, `vfio-ioctls`, `vm-superio`, etc.) that VMMs are built from. Firecracker, Cloud Hypervisor, libkrun, crosvm, and Dragonball all share these crates. An improvement to `vm-memory` (e.g. Kani formal verification, IOMMU support, guest_memfd) benefits all of them simultaneously.

"Internalization" would mean: instead of `mvm-runtime` shell-out-and-talk-to-the-`firecracker`-binary, we'd link rust-vmm crates directly into mvmctl and *be* a VMM.

## Why we rejected it for now

Three reasons.

### 1. Composing rust-vmm crates into a working VMM is *building a VMM*

rust-vmm provides primitives. Turning primitives into a working VMM means handling:

- Boot sequence (kernel loading, cmdline, initrd handoff).
- Device model wiring (which devices, in what order, with what features negotiated).
- Memory layout (mmap, KVM slots, IOMMU when relevant).
- vCPU lifecycle and scheduling.
- Snapshot and restore (Firecracker has a particularly mature implementation).
- Seccomp jail for the VMM process itself.
- Crash handling and graceful shutdown.

That's Firecracker's job, and the Firecracker team does it well. Doing it ourselves would mean signing up for ~50K LOC of net-new code and ongoing maintenance for behavior we already get for free.

### 2. The shell-out boundary is a feature, not a cost

Today `mvmctl run` spawns `firecracker` as a subprocess. The boundary has costs (process startup overhead, requires `firecracker` on PATH, version coupling between mvmctl and the `firecracker` release we test against), but it has benefits we'd lose by linking:

- A Firecracker lifecycle bug doesn't crash mvmctl.
- Firecracker's seccomp jail applies to the Firecracker process, not the entire mvmctl process.
- We can swap `firecracker` versions without rebuilding mvmctl.
- The boundary is a clean place for telemetry and observability.

For libkrun (Plan E) the situation is different — libkrun is *designed* to be linked, and the "single-binary, no external dependency" UX is one of the pull factors. That makes libkrun a special case, not a precedent.

### 3. There's no concrete feature gain we'd unlock

The blog-post argument for rust-vmm internalization (emirb 2026: *"rust-vmm is the real revolution, not any single VMM"*) is right *for VMM authors*. mvm is a layer up: we orchestrate VMMs. We benefit from rust-vmm via Firecracker and libkrun, but we don't need to compose the crates ourselves to capture that benefit.

If we ever need an mvm-specific isolation property no upstream VMM offers, that's the trigger. Until then, the cost is real and the benefit is hypothetical.

## Trigger conditions to revisit

Any one of these would warrant reopening the question:

1. **We need a custom VMM for an mvm-specific isolation property no upstream VMM offers.** Examples: an mvm-specific snapshot format with a security property neither Firecracker nor libkrun ships; an mvm-specific seccomp filter set that requires linking; a custom virtio device unique to mvm.
2. **We're shipping a single-binary distribution and the firecracker-binary-on-PATH dependency becomes a real distribution problem.** (Plan E — libkrun — mostly addresses this since libkrun *is* linked.)
3. **The Firecracker upstream stops maintaining a stable release cadence we can rely on.** Currently AWS keeps Firecracker on a steady cadence; this is unlikely to change.

## References

- Plan 53 §"Implementation plans → Plan H" — the high-level summary.
- Plan E (libkrun) — the case where linkage *is* the right answer because the VMM is designed for it.
- emirb 2026 microVM blog post — the argument for rust-vmm. Right for VMM authors; less compelling for orchestrators.
