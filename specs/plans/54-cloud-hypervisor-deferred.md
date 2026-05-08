# Plan 54 — Cloud Hypervisor backend (deferred — rejected for posture reasons)

> Status: deferred
> Decision recorded: 2026-05-07 (plan 53)
> Trigger to revisit: see "Trigger conditions" below

## Why this plan exists

Plan 53 (cross-platform roadmap) considered Cloud Hypervisor (CH) as a fifth backend in `AnyBackend`. After weighing the security tradeoffs, we **rejected** it. This file documents the rationale so a future contributor or reviewer doesn't have to re-derive the decision.

## What CH would have given us

Cloud Hypervisor is a Rust-based, KVM-on-Linux + WHPX-on-Windows VMM (~106K LOC) maintained by Intel and Microsoft. Adding it would unlock:

- **Nested KVM inside the guest** — Docker-in-Docker, Android emulators, anything that needs `/dev/kvm` from inside an mvm guest.
- **GPU passthrough** — VFIO devices for ML workloads.
- **Larger emulated device set** — virtio devices Firecracker doesn't ship.
- **Windows guest support** — running Windows VMs (not just Linux) inside an mvm-managed VM.
- **WHPX host on Windows** — a path toward native-Windows microVMs without WSL2.

## Why we rejected it

Every advantage above is a feature **Firecracker deliberately excluded for attack-surface reasons**. The Firecracker team's design philosophy is "remove devices and code paths until you can audit what's left." Cloud Hypervisor's philosophy is broader: "include the features people need for general-purpose virtualization with minimal-spirit defaults." Both are reasonable; they're not the same.

If we shipped CH alongside Firecracker, our security narrative would split:

> *"mvm uses Firecracker for security. If you need DinD or GPU passthrough, you can opt into Cloud Hypervisor — but its TCB is ~28% larger, its device model is ~4× larger, and the seven CI-enforced claims that hold for Firecracker need separate per-claim audits for CH."*

That's a *forked* security story, and it pulls Firecracker's status from "the secure backend" to "the secure-by-default backend, unless you opt out." The forking is what we're avoiding.

The user-facing ADR-002 is built on the premise that *all* in-tree backends carry the seven claims (Tier 1) or document explicit, named exceptions (Tier 2). CH would either need full Tier 1 audit work — which we'd have to redo for each release — or it would land as Tier 2 with claims 1, 2, 3, and 5 marked as "TBD pending CH-specific verification." Neither is appealing.

The cleaner answer is: Firecracker is the security baseline, and any need that pushes outside Firecracker's deliberate exclusions is a sign that mvm isn't the right tool for that workload. Use Cloud Hypervisor directly via `cloud-hypervisor`'s CLI; mvm doesn't try to be a universal VMM frontend.

## Trigger conditions to revisit

Both conditions must hold:

1. **A user demonstrates a need that *cannot* be satisfied by Firecracker, libkrun, or Apple Container.** Examples that would qualify: a security-research workload that requires nested KVM (rare); a GPU-accelerated ML inference workload that can't be served by the host (very rare); a Windows-guest requirement (not in scope for our user base today).
2. **We're prepared to update ADR-002 to acknowledge a forked security model** with explicit per-claim notes for CH and a `mvmctl doctor` warning whenever the active backend is CH ("Tier 1.5 — six of seven claims hold; claim X is partial because Y").

The first condition alone is not enough — convenience wants are not posture-changing decisions.

## What "implementation if pulled" would look like

For estimation only; not commitments.

- **Files to create**:
  - `crates/mvm-runtime/src/vm/cloud_hypervisor.rs` — `CloudHypervisorBackend` impl over CH's HTTP-API-over-unix-socket.
  - `specs/plans/<NN>-cloud-hypervisor-backend.md` — full plan with phases and tests.
- **Files to modify**:
  - `crates/mvm-runtime/src/vm/backend.rs` — add `CloudHypervisor` variant + `auto_select` rules.
  - `crates/mvm-cli/src/commands/vm/up.rs` — `--hypervisor cloud-hypervisor` (alias `chv`).
  - `crates/mvm-core/src/protocol/vm_backend.rs` — extend `VmCapabilities` with `nested_kvm`, `gpu_passthrough`, `non_linux_guest`.
  - `crates/mvm-cli/src/bootstrap.rs` — install `cloud-hypervisor` binary (Nix package, distro packages, GitHub release).
  - `crates/mvm-cli/src/doctor.rs` — CH availability check + posture banner.
  - `specs/adrs/002-microvm-security-posture.md` — new tier row, per-claim notes for CH.
- **Effort**: ~1 sprint to implement the backend, plus the ADR update.

## References

- Plan 53 §"Security posture decision" — the fork test.
- ADR-002 — the seven claims that any new backend must respect.
- emirb 2026 microVM blog post — *"If yes [DinD/GPU/Windows]: Cloud Hypervisor. If no: Firecracker."* This plan picks "no."
