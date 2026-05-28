# Plan 106 — Plan 100 W6 brainstorm: workload-path dispatch flip

> **Status (2026-05-27):** brainstorm doc. Designs the architectural
> dispatch flip that completes Plan 100 — Linux workload microVMs
> nest through a libkrun host VM instead of running Firecracker
> directly on the Linux host. This plan is **planning only** —
> implementation slices spawn from here as Plan 107+ once an approach
> is locked.
>
> Picks up after Plan 105 (W1 prep + W2 nix-build CI lane + W3 doctor
> probe) landed in PR #479. W1's `MVM_LINUX_BUILDER_VM` env constant
> and `linux_builder_vm_readiness()` predicate are the rollout signal
> this plan flips on.
>
> Pick-up command for fresh sessions: read this file top to bottom,
> then jump to the approach-selection question in the §"Decision
> needed" section.

## Context

ADR-057 argues that mvm's execution paths are asymmetric across host
OSes:

- **macOS** runs workload microVMs through `LibkrunBackend` directly
  (`crates/mvm-backend/src/libkrun.rs::LibkrunBackend::start`). The
  libkrun VM IS the workload. No Firecracker nesting on the workload
  path — Firecracker only appears in the *builder* path
  (`LibkrunBuilderVm` for Nix builds).
- **Linux** runs workload microVMs through `FirecrackerBackend`
  directly on the host
  (`crates/mvm-backend/src/backend.rs::FirecrackerBackend::start` →
  `microvm::run_from_build`). A host process can `ptrace`
  Firecracker or read `/proc/<pid>/mem` without crossing a
  hypervisor boundary.

Plan 100 wants Linux to look like macOS at the trust boundary:
workload microVMs always run inside a host VM, with the host
userland never in the workload TCB. The macOS-style libkrun host
VM is the chosen substrate.

W1-W5 of Plan 100 are foundations:

- **W1** (Plan 105, PR #479) — `MVM_LINUX_BUILDER_VM` env constant +
  `has_nested_kvm()` predicate + `linux_builder_vm_readiness()`
  refusal gate. Operators can validate their host today.
- **W2** (PR #479) — `builder-vm-image-linux` CI lane proves the
  `nix/images/builder-vm/` flake builds end-to-end on Ubuntu.
- **W3** (PR #479) — `mvmctl doctor` `nested-kvm` line surfaces
  readiness in operator-visible form.
- **W4** — Live-KVM smoke. Gated on W6 because the nested workload
  path doesn't exist yet to smoke.
- **W5** — Audit-chain parity through the nested layer. Gated on W6
  for the same reason.

W6 is the architectural surgery: replace the direct-Firecracker
launch on Linux with a nested chain
`Host → libkrun Linux host VM → Firecracker workload microVM`.

## Decision needed: which nesting architecture?

Three viable approaches; each with measurable cold-start, isolation,
and code-reuse tradeoffs. The decision-of-record lives in **this
section** and feeds the executable plans below.

### Approach A — Shared libkrun host VM, Firecracker per workload

One libkrun host VM per `mvmctl dev` session, started once and held
warm for the session's lifetime. Each `mvmctl up` / `mvmctl run`
spawns its own Firecracker workload microVM **inside** the host VM
as a guest-side process. The libkrun VM acts as the symmetric
substrate; Firecracker provides per-workload isolation.

**Wiring:**

- Plan 89's `LibkrunPersistentBuilderVm` is generalised to
  `LibkrunPersistentHostVm` — same long-lived libkrun VM, but the
  guest-side dispatch loop spawns Firecracker workloads (not just
  Nix builds).
- New control protocol: host sends `WorkloadRequest::Start { plan }`
  over the existing vsock dispatch port; guest-side
  `mvm-host-vm-init` (renamed from `mvm-builder-init`) launches
  Firecracker with the workload's kernel + rootfs + virtio config.
- Host-side `FirecrackerBackend::start` on Linux: instead of
  launching Firecracker directly, it sends the dispatch request to
  the persistent host VM and proxies the workload's vsock back to
  the host caller.
- Vsock proxy gains a hop: `host → host-VM vsock proxy → workload
  Firecracker vsock`.

**Pros:**

- Maximum warm-start advantage. First workload pays the libkrun
  cold boot (~3-5s per W0 estimate); subsequent workloads pay only
  the Firecracker boot (~50ms).
- Closest to Plan 89's already-proven persistent-builder protocol.
  Reuse `BuilderRequest` framing with a new `WorkloadRequest`
  variant.
- Maps directly onto Plan 100's "marginal cost tends to zero on
  subsequent workloads" exit criterion.

**Cons:**

- Cross-workload isolation inside the host VM. One bad workload
  could DoS the host VM and take down sibling workloads. Acceptable
  per ADR-002 §"Multi-tenant guests are out of scope (one guest = one
  workload)" — but the *host VM* is now a shared trust boundary.
- Persistent-VM lifecycle ownership is harder. Today `LibkrunPersistentBuilderVm`
  ties to `mvmctl dev`. W6 needs the host VM to outlive any single
  `mvmctl up` and clean up on `mvmctl dev down` — or on session
  termination.
- Slightly more code change. Plan 89's dispatch loop only knows
  how to run Nix builds today.

### Approach B — One libkrun VM per workload, Firecracker as guest PID 1

Each `mvmctl up` / `mvmctl run` spawns its own libkrun VM whose
guest's PID 1 IS Firecracker. The libkrun VM has no other purpose
than to host that one Firecracker workload.

**Wiring:**

- New `LibkrunWorkloadHostVm` — one-shot libkrun VM that boots a
  minimal rootfs containing only Firecracker + the workload's
  kernel/rootfs.
- Host-side `FirecrackerBackend::start` on Linux: spawns
  `mvm-libkrun-supervisor` per workload; libkrun guest boots
  Firecracker as init; Firecracker boots the workload microVM
  inside.
- Vsock proxy gains a hop (same as A).

**Pros:**

- Cleanest isolation. Each workload has its own host VM; sibling
  workloads can't affect each other.
- Lifecycle is straightforward — host VM dies with the workload.
- Smaller protocol surface — no new dispatch messages, just
  per-workload libkrun spawn.

**Cons:**

- Cold-start cost per workload. Every `mvmctl up` pays the libkrun
  boot (~3-5s) on top of the Firecracker boot. Plan 100's "tends to
  zero on subsequent workloads" goal fails.
- Resource overhead. N concurrent workloads = N libkrun VMs each
  reserving cpu/memory for their guest host kernel.
- Different code path from macOS, where `LibkrunBackend` is the
  workload itself. Approach B introduces a third pattern on Linux.

### Approach C — Libkrun-only on Linux (no Firecracker nesting)

Drop Firecracker on Linux entirely. Replace the workload path with
`LibkrunBackend` like macOS does. One libkrun VM per workload,
libkrun IS the workload's hypervisor.

**Wiring:**

- Linux `AnyBackend::auto_select` returns `LibkrunBackend` instead
  of `FirecrackerBackend` when `MVM_LINUX_BUILDER_VM=1`.
- Reuse the entire macOS path. No nesting, no per-VM control
  protocol changes.
- `crates/mvm-backend/src/firecracker.rs` direct-launch path
  retires entirely (Plan 100 W6's stated endgame).

**Pros:**

- Symmetry achieved at the lowest possible abstraction cost. macOS
  and Linux take the same path.
- Major code deletion. Firecracker-direct on Linux goes away;
  one fewer hypervisor backend to maintain.
- No new protocol surface; no nested vsock proxy.

**Cons:**

- Loses Firecracker's narrative: minimal attack surface, audited
  jailer, narrow virtio set. libkrun's surface is broader (more
  virtio devices, no jailer-equivalent confinement).
- Trades the existing ADR-002 Claim 3 (dm-verity rootfs) evidence
  for whatever libkrun's analogous posture is. Re-deriving the
  claim under libkrun is a separate evidence pass.
- mvmd's runtime layer assumes Firecracker today (cross-repo
  coupling). Coordinating mvmd to drop Firecracker on Linux too is
  out of mvm's unilateral scope.
- Breaks every existing Linux test that exercises Firecracker
  directly. Not all are within mvm's repo.

### Recommendation: **Approach A** (shared libkrun host VM, Firecracker per workload)

Approach A best matches the named goals: symmetric trust posture
(Plan 100 / ADR-057), warm-start cost (Plan 100 exit criterion),
maximum reuse of Plan 89's already-proven dispatch protocol. The
cross-workload-DoS risk is bounded by ADR-002's one-guest-per-host
posture and can be mitigated by a per-workload Firecracker (so a
crashing workload kills its own Firecracker, not the host VM).

Approach B is the safe fallback if A's lifecycle-management
complexity is judged too high. The cold-start regression is the
primary cost.

Approach C is the cleanest long-term endpoint but requires
cross-repo (mvmd) coordination and a re-derivation of ADR-002
claims under libkrun-as-workload. Out of scope for this plan; a
future ADR may revisit once libkrun's attack surface is independently
audited.

## Execution sketch (assumes Approach A)

Slices below are sized for one PR each. The implementation plan
spawns from this brainstorm as Plan 107+ once the user approves
Approach A.

### Phase A1 — Generalise the persistent-builder dispatch protocol

- [ ] **A1.1** Extract a `HostVmRequest` / `HostVmResponse` framing
      layer from `crates/mvm-build/src/builder_protocol.rs`. Today's
      `BuilderRequest::Run { job }` becomes one variant of a
      backend-agnostic message. New variants: `WorkloadStart`,
      `WorkloadStop`, `WorkloadStatus`.
- [ ] **A1.2** Generalise `LibkrunPersistentBuilderVm` →
      `LibkrunPersistentHostVm` (probably a new type in
      `mvm-build` or a new crate `mvm-host-vm`; revisit during
      implementation). The builder use-case becomes one of N
      dispatch consumers.
- [ ] **A1.3** Rename `mvm-builder-init` → `mvm-host-vm-init`. The
      guest-side dispatch loop now branches on request kind: Nix
      builds (existing) vs Firecracker workload spawn (new).
- [ ] **A1.4** Hermetic protocol tests — same shape as Plan 89's
      builder-protocol round-trip tests, extended for the workload
      variants.

### Phase A2 — Firecracker-in-guest launch path

- [ ] **A2.1** Bake Firecracker into the builder-vm rootfs
      (`nix/images/builder-vm/`). Today the rootfs contains
      busybox + Nix + build tools; W6 adds Firecracker binary.
- [ ] **A2.2** Guest-side: `mvm-host-vm-init`'s `WorkloadStart`
      arm spawns Firecracker with the workload's kernel + rootfs +
      virtio config (passed in the request payload).
- [ ] **A2.3** Per-workload state dir inside the host VM:
      `/var/lib/mvm/workloads/<workload_id>/` for Firecracker
      sockets, PID files, console logs. Cleaned up on
      `WorkloadStop`.
- [ ] **A2.4** Live smoke on a Linux + nested-KVM CI runner: send
      `WorkloadStart` to the host VM, assert Firecracker boots
      the workload inside.

### Phase A3 — Vsock proxy: add the nesting hop

- [ ] **A3.1** Today's vsock proxy (host → libkrun guest, port
      5252) gains a hop: host → host-VM vsock → workload Firecracker
      vsock. The proxy plumbing probably lives in
      `crates/mvm-supervisor/` — confirm during impl.
- [ ] **A3.2** `mvm-guest-agent` (inside workload microVM) stays
      unchanged. It still connects to vsock CID 3 port 5252; that
      vsock is now Firecracker's, inside the host VM, instead of
      the host's directly.
- [ ] **A3.3** Audit-chain entries flow through the proxy
      unchanged — Plan 100 W5's parity assertion.
- [ ] **A3.4** Per-flow guardrails: rate limit, max concurrent
      proxied vsock streams. Inherited from Plan 102 W6.A.5's
      bridge if that's merged by then.

### Phase A4 — Linux `mvmctl up` wires the new path

- [ ] **A4.1** When `MVM_LINUX_BUILDER_VM=1` is set on Linux,
      `FirecrackerBackend::start` no longer launches Firecracker
      directly. Instead it sends `WorkloadStart` to the
      `LibkrunPersistentHostVm` instance the session owns.
- [ ] **A4.2** When the env is unset, the direct-Firecracker path
      stays (rollout safety).
- [ ] **A4.3** Doctor probe extended: `builder backend` line gains
      a "Plan 100 W6 active" status when the host VM is up.
- [ ] **A4.4** Live-KVM smoke (Plan 100 W4 finally lands): the
      workload boots end-to-end, signed plan + dm-verity rootfs
      + audit chain entries all flow.

### Phase A5 — Retire the direct-Firecracker code path

- [ ] **A5.1** Flip `MVM_LINUX_BUILDER_VM=1` to default-on. Env
      becomes the opt-OUT (`=0` for the legacy direct path),
      analogous to how libkrun became the macOS default.
- [ ] **A5.2** Delete the direct-Firecracker launch in
      `crates/mvm-backend/src/firecracker.rs` once the new path is
      proven across N CI runs without regression. Plan 100 W6's
      stated endpoint.
- [ ] **A5.3** Remove the `MVM_LINUX_BUILDER_VM` env entirely once
      the legacy path is gone. Becomes Plan 100's W6 close-out.

### Phase A6 — Docs + claim rewording

- [ ] **A6.1** Update `CLAUDE.md` architecture diagram: Linux row
      becomes `Host → libkrun Linux VM → Firecracker microVM`.
- [ ] **A6.2** Reword ADR-002 Claim 1 per Plan 100 W8:
      "...via identical builder-VM TCB on macOS and Linux."
- [ ] **A6.3** ADR-001 cross-reference: the new nested model
      replaces "Firecracker-only" framing on Linux.
- [ ] **A6.4** CI gate (Plan 100 W8): grep the integration test
      output for a builder-VM ancestor in the Firecracker process
      tree. Fails the build if Linux ever launches Firecracker
      without a libkrun parent post-A5.

## Out of scope (this plan)

- Apple Container backend (separate ADR-056 / Plan 97 work).
- Vz backend (Plan 98).
- Plan 101/102 gateway audit work (in flight, separate sprint).
- Plan 104 host services broker (in flight, separate sprint).
- mvmd cross-repo runtime contract changes (W6's host-VM signal
  is host-local; mvmd doesn't see the dispatch layer).

## Risks + open questions

- **R1 — Cold-start regression on the first workload.** Plan 100
  W0 needs measured numbers before A4 ships. If first-workload
  latency is unacceptable, A5 stays gated until the warm-pool
  benefit demonstrably outweighs it.
- **R2 — Cross-workload DoS inside the host VM.** A bad workload
  can't crash siblings if each has its own Firecracker (Approach
  A's per-workload Firecracker), but a kernel-panic in the host
  VM itself takes everything down. Mitigation: aggressive host-VM
  health monitoring + per-session recovery.
- **R3 — Nested-KVM availability on cloud Linux runners.** Some
  cloud hypervisors disable nested virt by default. Plan 105 W1's
  `linux_builder_vm_readiness()` already refuses with an
  actionable error; this stays the operator-facing failure mode.
- **R4 — Firecracker-in-rootfs binary distribution.** Today
  Firecracker is host-installed on Linux. Baking it into the
  builder-vm rootfs (A2.1) is a new artifact distribution path —
  must source from a verified upstream or build from source per
  ADR-046.
- **R5 — mvmd integration.** mvmd's runtime contract assumes
  Firecracker-on-host. The W6 dispatch flip is local to mvm; mvmd
  sees the same VmStartConfig → VmBackend interface. **Validate
  this assumption** before A4 ships; if mvmd has Firecracker-path
  assumptions (e.g. reading `/proc/<fc_pid>/...` from the host),
  the contract needs a cross-repo update.

## What this plan does NOT decide

- Whether `LibkrunPersistentHostVm` lives in `mvm-build` or a new
  crate (e.g. `mvm-host-vm`). Implementation decision; first PR
  picks.
- Exact wire format for `WorkloadRequest` / `WorkloadResponse`.
  Implementation decision; Plan 89's `BuilderRequest` framing is
  the template.
- Whether macOS workloads also nest. Today macOS uses
  `LibkrunBackend` directly without Firecracker; Approach A
  doesn't change that. A future ADR may unify if mvmd-coordinated.

## Verification (when the time comes)

A workload booted via the new path is byte-equivalent (in the
artifact-and-audit-chain sense) to the same workload booted via
the direct-Firecracker path. Concretely:

- Same `ExecutionPlan` → same admitted plan, same audit chain
  entries (`plan.admitted`, `plan.launched`).
- Same workload-rootfs hash, same dm-verity roothash, same
  cmdline. The boot environment differs only in *who launched
  Firecracker*, not in *what Firecracker saw*.
- `mvmctl audit verify` round-trips clean across both paths.

Verification of the trust uplift (the actual reason for W6) is
qualitative: the host userland no longer sees the workload's
process tree. Tested by `ptrace`-ing the host-VM process — must
fail to see the workload's Firecracker. This is the new evidence
ADR-002 Claim 1 will cite post-A6.2.

## Critical files (when implementation starts)

### Existing surface to extend

- `crates/mvm-build/src/libkrun_builder.rs::LibkrunPersistentBuilderVm`
  — the persistent-VM pattern Approach A generalises.
- `crates/mvm-build/src/builder_protocol.rs` — `BuilderRequest` /
  `BuilderResponse` framing; the new `WorkloadRequest` family
  follows its shape.
- `crates/mvm-build/src/persistent_builder.rs` — host-side
  dispatch supervisor; Approach A adds workload variants.
- `crates/mvm-builder-init/` — guest-side dispatch loop; renames
  to `mvm-host-vm-init` per A1.3.
- `crates/mvm-backend/src/backend.rs::AnyBackend::auto_select` —
  Linux selection branches.
- `crates/mvm-backend/src/firecracker.rs::FirecrackerBackend::start`
  — A4.1's redirect target.
- `crates/mvm-build/src/builder_backend_select.rs::linux_builder_vm_requested`
  — Plan 105 W1's signal that A4.1 consumes.
- `crates/mvm-supervisor/` — vsock proxy layer that A3 extends.

### New surface

- `LibkrunPersistentHostVm` (location TBD per A1.2).
- Firecracker binary in `nix/images/builder-vm/` (A2.1).
- Workload control protocol (A1.1 + A3).

### Reused untouched

- `crates/mvm-vz/`, `crates/mvm-vz-supervisor/` — macOS Vz path
  unchanged.
- `crates/mvm-cli/src/commands/env/dev.rs` — `mvmctl dev` already
  selects the right backend via Plan 98's selection layer.
- `mvm_core::platform::Platform::has_nested_kvm` — Plan 105 W1's
  predicate, consumed by A4.1.
- `mvm_sdk::compile::deps_audit::verify_sealed_volume` — Claim 9
  evidence; flows through unchanged because workload artifacts
  are unchanged.

---

## Decision point for the user

**Approach A, B, or C?** Recommendation: A. Locked decision feeds
into the executable Plan 107 (the first implementation slice — A1
protocol generalisation). If the user picks B or C, the executable
plan structure changes substantively; this brainstorm is the place
to redirect.
