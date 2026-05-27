# ADR-057 — Symmetric builder VM across hosts

**Status:** Proposed
**Sprint:** 56 (W1)
**Plan:** [Plan 100](../plans/100-symmetric-builder-vm-rollout.md)

## Context

Today's execution paths are asymmetric across host operating systems:

- macOS: `Host → libkrun Linux VM (builder/runner) → Firecracker microVM`
- Linux: `Host → Firecracker microVM (direct on host KVM)`

Builder-backend dispatch lives at `crates/mvm-build/src/builder_backend_select.rs:86-91` (`resolve_builder_backend()`). On Linux, the host's userland sits in the TCB for every workload because workload microVMs share Linux's host kernel and a host process can `ptrace` Firecracker or read its `/proc/<pid>/mem` without ever crossing a hypervisor boundary. On macOS the same operations would first require breaching the libkrun Linux VM that sits between mvmctl and the workload — a different threat tier.

This asymmetry undermines the security claims listed in ADR-002. Claim 1 ("no host-fs access from a guest beyond explicit shares") is meaningful only relative to the host being trusted not to peek. ADR-002 explicitly carves out "a malicious host" as out-of-scope, granting the host the hypervisor and the private build keys — but no more. On Linux today the host has more capability than the carve-out grants it, by virtue of running the workload directly. On macOS, it doesn't. The claim should hold uniformly.

## Decision

Workload microVMs always run inside a builder VM, regardless of host OS:

- macOS keeps the libkrun-based builder VM it already has.
- Linux gains a libkrun-based builder VM with nested KVM. Execution becomes: `Host KVM → libkrun Linux builder VM (nested KVM) → Firecracker workload microVM`.

The signing identity is established inside the builder VM on both hosts. The host userland is no longer in the TCB on either; the host's role narrows to "owns the hypervisor process and the private build-key escrow, nothing else."

## Consequences

- **Boot-time cost.** A small Linux builder VM cold-starts on `mvmctl up` / first `mvmctl run` on Linux contributors. Reused across workloads in a single `mvmctl dev` session; the marginal cost across N workloads tends to zero. Plan 100 W0 measures the cold-start delta.
- **Trust-claim uplift.** Claim 1 ("no host-fs access from a guest beyond explicit shares") becomes true on both OSes via identical mechanism. Claims 2, 3, 4, 5, 8, 9 inherit the strengthened TCB.
- **Code simplification.** One execution model replaces two. The `mvm-backend/firecracker.rs` direct-launch path retires.
- **Performance.** Nested KVM on Linux is well-supported in mainline kernels (`kvm-intel.nested=1`, `kvm-amd.nested=1` — default-on on most distros since ~5.10). Overhead is single-digit percent on hot paths.
- **Doctor probe needed.** Some hosts (cloud Linux runners, container hosts, locked-down corporate workstations) disable nested virtualization. `mvmctl doctor` must detect and report.

## Rejected alternatives

- **Stay asymmetric.** Uneven trust story; can't make a uniform claim 1. Code paths diverge further over time.
- **Reintroduce Lima for Linux symmetry.** Reverses the 2026-05-14 Lima removal for cosmetic symmetry; brings back YAML lifecycle, ssh-only access, and image distribution complexity. The trust property is independent of Lima — `mvm-libkrun` on Linux gives the same property cleanly without the abstraction debt.

## Open questions

- Builder VM cold-start latency on Linux CI runners. Plan 100 W0 measures this against the current Firecracker-direct baseline.
- Nested KVM availability on cloud Linux hosts. Some cloud hypervisors disable nested virt by default (or expose it via per-VM capabilities). Doctor probe + clear failure mode required.

## Relationship to Plan 98 (Vz builder backend)

Plan 98 ships a second builder-VMM impl (Apple Virtualization.framework / Vz) on macOS 26+ Apple Silicon, parallel to libkrun. That work is **complementary** to this ADR's symmetric-builder uplift:

- **Plan 98** picks which host VMM runs the macOS builder VM (libkrun or Vz). It does not change which OSes have a builder VM at all.
- **This ADR (Plan 100)** adds a builder VM to Linux too, so workload microVMs always run nested.

Plan 98's macOS work narrows the asymmetric-trust gap *on macOS* (it stops requiring the third-party `slp/krun` Homebrew trio when Vz is the default), but Linux still runs Firecracker directly until Plan 100 W2 lands the nested libkrun-on-Linux path. The two efforts ship independently; their selection layers compose via `mvm_build::builder_backend_select::resolve_choice` (Plan 98 introduced) which already has a third arm reserved for future Linux-builder dispatch (Plan 100 W2 will populate it). Builder-backend parity discussion lives in **ADR-046 §"Vz as a second builder backend (Plan 98)"**.

## References

- [ADR-001](001-firecracker-only.md) — Firecracker-only execution (needs update for nested model)
- [ADR-002](002-microvm-security-posture.md) — microVM security posture (claim 1 reworded by Plan 100 W8)
- [ADR-046](046-builder-vm-via-libkrun.md) — builder VM via libkrun + Plan 98 Vz extension
- [Plan 100](../plans/100-symmetric-builder-vm-rollout.md) — implementation rollout
