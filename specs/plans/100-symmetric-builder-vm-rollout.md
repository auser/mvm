# Plan 100 — Symmetric builder VM rollout

**Sprint:** 56 (W1)
**ADR:** [ADR-057](../adrs/057-symmetric-builder-vm.md)
**Status:** Proposed

## Goal

Bring Linux up to the macOS execution model: workload microVMs always run inside a libkrun-launched Linux builder VM, regardless of host OS. Retire the direct-Firecracker-on-Linux path so claim 1 ("no host-fs access from a guest beyond explicit shares") holds uniformly.

## Wave breakdown

- [ ] **W0 — Feasibility prototype.** libkrun-on-Linux boot, nested-KVM smoke, measured cold-start time vs current Firecracker-direct path. Throw-away branch. Exit criteria: a workload microVM boots end-to-end inside a libkrun builder VM on a Linux runner; cold-start latency measured and within budget (target: <2× current direct-launch on the first workload, ~0 marginal on subsequent workloads in the same `mvmctl dev` session).

- [ ] **W1 — Backend dispatch.** Extend `resolve_builder_backend()` (`crates/mvm-build/src/builder_backend_select.rs:86-91`) so Linux returns `LibkrunBuilderVm` instead of falling through to direct Firecracker. Gate behind `MVM_LINUX_BUILDER_VM=1` env var for the rollout window so we can flip it on/off without a recompile.

- [ ] **W2 — Image distribution on Linux.** Ensure `nix/images/builder-vm/` builds on Linux contributor hosts (currently exercised mostly via macOS). No mvm-published prebuilt artifact — source-checkout-only per CLAUDE.md ("source-checkout builds never depend on mvm-published artifacts"). Validate `nix build .#builder-vm` from a Linux contributor host.

- [ ] **W3 — Doctor probe.** Add `linux_nested_kvm_available()` to `mvmctl doctor`. Detect via `/sys/module/kvm_intel/parameters/nested` or `/sys/module/kvm_amd/parameters/nested`; report degraded mode if either is `0` / `N`. Emit install hint pointing to the kernel-module parameter fix.

- [ ] **W4 — Integration tests.** Live-KVM smoke on Linux exercises the builder-VM path, not the direct-Firecracker path. Both must boot a workload microVM end-to-end (signed ExecutionPlan, dm-verity rootfs, audit chain entries). Add a regression test that fails if `MVM_LINUX_BUILDER_VM=1` ever silently falls back to direct.

- [ ] **W5 — Cross-cut: ExecutionPlan/supervisor.** Confirm signed-plan flow and audit chain work identically through the nested layer. Plan signature verification, audit chain append, nonce replay-store — all must work when the supervisor runs inside the builder VM rather than on the host. No new event types needed; same `plan.admitted` / `plan.launched` / `plan.failed` chain. Add a test that asserts identical audit-event payloads on macOS vs Linux for the same workload spec.

- [ ] **W6 — Migration: retire direct-Firecracker path on Linux.** Delete the host-side direct-launch code in `crates/mvm-backend/src/firecracker.rs` once W4 is green and `MVM_LINUX_BUILDER_VM=1` is the default. The supervisor + workload microVM still use Firecracker — just launched from inside the builder VM, not from the host. Flip the env-var default to "on" and remove the gate.

- [ ] **W7 — Docs.** Update the architecture diagram in `CLAUDE.md` (currently shows `Linux Host -> Firecracker microVM`; becomes `Linux Host -> libkrun Linux VM -> Firecracker microVM`). Update `specs/adrs/001-firecracker-only.md` to reflect nested execution. Cross-reference ADR-057 from ADR-002.

- [ ] **W8 — Claim posture update.** Reword ADR-002 claim 1: "...via identical builder-VM TCB on macOS and Linux." Add a CI gate in `.github/workflows/security.yml` that asserts no code path on Linux launches Firecracker without a builder VM ancestor process (grep-based or process-tree check inside the integration test).

## Critical files

- `crates/mvm-build/src/builder_backend_select.rs` (W1: backend dispatch)
- `crates/mvm-backend/src/firecracker.rs` (W6: direct-launch path retires)
- `crates/mvm-backend/src/libkrun.rs` (path used by Linux post-rollout)
- `crates/mvm-libkrun/` (W0/W4: nested KVM enablement)
- `crates/mvm-cli/src/commands/doctor.rs` (W3: probe)
- `nix/images/builder-vm/flake.nix` (W2: Linux build validation)
- `CLAUDE.md` (W7: architecture diagram + Linux host dependencies section)
- `specs/adrs/002-microvm-security-posture.md` (W8: claim 1 reword)
- `specs/adrs/001-firecracker-only.md` (W7: cross-ref nested model)
- `.github/workflows/security.yml` (W8: CI gate)

## Out of scope

- Apple Container backend (separate ADR-056 / Sprint 55 work).
- Vz backend (separate Sprint 55 work).
- Hardware-virt detection on cloud runners beyond the W3 doctor probe.

## Verification

- W0 prototype attaches measured cold-start numbers to this plan as a follow-up comment.
- W4 integration tests pass on the `linux` and `macos-14` CI lanes with `MVM_LINUX_BUILDER_VM=1`.
- W6 deletes >0 lines from `mvm-backend/src/firecracker.rs` direct-launch path; `cargo build --workspace` still green.
- W8 CI gate green on a clean PR; goes red if a developer adds a code path that bypasses the builder VM.
