# Plan 107 — Plan 100 W6 Approach A: nested workload-microVM dispatch

**Sprint:** 56 (W1)
**ADR:** [ADR-057](../adrs/057-symmetric-builder-vm.md)
**Brainstorm:** [Plan 106](106-plan-100-w6-dispatch-flip.md)
**Status:** Proposed
**Decision:** Locked 2026-05-27 — Approach A (shared libkrun host VM, Firecracker per workload)

## Goal

Land Plan 100 W6 on Linux: workload microVMs run inside a
long-lived libkrun host VM instead of launching Firecracker
directly on the host. Closes the asymmetry ADR-057 names between
macOS (libkrun-as-workload) and Linux (Firecracker-on-host) and
tightens ADR-002 Claim 1 by removing the host userland from the
workload TCB.

The architectural choice (Approach A from Plan 106 brainstorm)
is locked. This plan sequences the implementation into six phases
(A1–A6), each sized for one PR.

## Pick-up command (for fresh sessions)

Read `specs/plans/106-plan-100-w6-dispatch-flip.md` first for the
rationale and rejected alternatives, then resume here from the
first unchecked phase below.

## Prerequisites (already shipped)

- **Plan 105 / Plan 100 W1** (PR #479) — `MVM_LINUX_BUILDER_VM`
  env constant + `Platform::has_nested_kvm()` predicate +
  `linux_builder_vm_readiness()` refusal gate.
- **Plan 100 W2** (PR #479) — `builder-vm-image-linux` CI lane
  validates the `nix/images/builder-vm/` flake on Ubuntu.
- **Plan 100 W3-doctor** (PR #479) — `mvmctl doctor` `nested-kvm`
  line surfaces operator readiness.
- **Plan 89** — persistent-builder dispatch protocol
  (`crates/mvm-build/src/builder_protocol.rs`,
  `LibkrunPersistentBuilderVm`) is the template Approach A
  generalises.

## Phase breakdown

### Phase A1 — Generalise the dispatch protocol

Goal: split the persistent-VM framing layer so the same long-lived
libkrun VM can serve Nix builds (existing) and Firecracker workload
spawns (new).

Split into two PRs (discovered during A1 implementation — the crate
rename in A1.3 turned out to drag the Stage 0 byte-scan + kernel
cmdline path migration along with it, materially heavier than the
protocol rename. A1a ships the cheap half today; A1b ships the
Stage 0 migration on its own with full attention).

#### A1a — Protocol type rename + struct rename

- [x] **A1.1** Extract a `HostVmRequest` / `HostVmResponse` framing
      layer from `crates/mvm-build/src/builder_protocol.rs`. Today's
      `BuilderRequest::Run { job }` becomes one variant of a
      backend-agnostic message; add `WorkloadStart`, `WorkloadStop`,
      `WorkloadStatus` siblings (payload shapes deferred to A2.2).
      Adds a `WorkloadId` newtype mirroring `JobId`.
- [x] **A1.2** Generalise `LibkrunPersistentBuilderVm` →
      `LibkrunPersistentHostVm`. Location: keep in `mvm-build` for
      this slice (cheapest); revisit splitting into a dedicated
      `mvm-host-vm` crate in A5 if the API surface grows beyond
      what fits next to the builder code. Hard rename per the
      no-backcompat memory — no `LibkrunPersistentBuilderVm`
      alias.
- [x] **A1.4** Hermetic protocol tests — same shape as Plan 89's
      builder-protocol round-trip tests, extended for the workload
      variants. Tampered-frame rejection paths included
      (`#[serde(deny_unknown_fields)]` carries over). 9 new tests
      covering `WorkloadId` serialization, all 6 new
      request/response variants round-tripping, wire-kind tag
      stability, and `deny_unknown_fields` rejection on new
      variants.

Exit (A1a): `cargo test --workspace` green; existing Nix-build
flow round-trips identically through the renamed protocol; new
workload variants reachable through the type system. Guest-side
dispatch arm for workloads lands with A1b's crate rename.

#### A1b — Crate rename `mvm-builder-init` → `mvm-host-vm-init` <a id="a1b-rationale"></a>

The crate's binary name `mvm-builder-init` is baked into the Stage
0 rootfs at `/sbin/mvm-builder-init`. Several host code paths scan
the rootfs for that exact byte sequence to validate Stage 0
compatibility (`crates/mvm-cli/src/commands/env/apple_container.rs`
lines 24, 1047, 1122, 2268, 2317, 2562, 2744). Renaming the binary
in lockstep means:

- Kernel cmdline `init=/sbin/mvm-builder-init` changes everywhere
  it appears.
- Host rootfs-scan byte strings change at all seven call sites
  above.
- All cached rootfs images become Stage 0-incompatible until
  rebuilt — `mvmctl dev up` users see a fail-closed rebuild prompt
  on first run after upgrade.

- [x] **A1.3.crate** Rename crate dir + `Cargo.toml` `package.name`
      + `[[bin]] name` + workspace members entry. Update
      `mvm_builder_init` → `mvm_host_vm_init` across all Rust
      imports.
- [x] **A1.3.stage0** Update the Stage 0 byte-scan constants +
      kernel cmdline + rootfs path everywhere — 15 sites in
      `apple_container.rs`, plus `BUILDER_INIT_PATH` const renamed
      to `HOST_VM_INIT_PATH`, plus `stage0/init.sh`.
- [x] **A1.3.nix** Update Nix flakes that copy the binary into
      the rootfs: `nix/lib/mk-guest.nix`,
      `nix/images/builder/flake.nix`,
      `nix/images/builder-vm/flake.nix`,
      `nix/images/builder-vm/kernel/`.
- [x] **A1.3.ci** Update `.github/workflows/ci.yml` paths.
- [x] **A1.3.docs** Update `CLAUDE.md`, public docs
      (`builder-vm.md`, `guest-agent.md`), scripts
      (`plan-89-baseline.sh`, `test-app-deps-ci-gate.sh`), and the
      `plan-89-baseline` fixture flake. Historical spec docs left
      untouched intentionally — they describe behaviour at the
      time they were written.
- [x] **A1.3.guest-arm** Guest-side dispatch loop branches on
      request kind: Nix builds (existing arm, unchanged behaviour)
      vs. workload spawn — `WorkloadStart`/`WorkloadStop`/
      `WorkloadStatus` parse via the local `HostVmRequest` mirror
      in `mvm-host-vm-init/src/builder_request.rs`; dispatch loop
      arms in `main.rs` panic with `unimplemented!()` carrying the
      received `workload_id`, so a future host-side regression
      that accidentally sends a Workload* before A2.2 lands is
      caught loudly at boot rather than silently dropped.

Exit (A1b): `cargo test --workspace` green; `mvmctl dev up` boots
a freshly-built rootfs through the renamed binary; cached
pre-rename rootfs images fail-closed with an actionable rebuild
prompt; live-KVM smoke (cold) green.

### Phase A2 — Firecracker-in-guest launch path

Goal: the host VM can spawn a Firecracker workload microVM inside
itself on dispatch.

- [x] **A2.1** Bake Firecracker into the builder-vm rootfs
      (`nix/images/builder-vm/`). Today the rootfs contains
      busybox + Nix + build tools; W6 adds the Firecracker binary
      from a verified upstream (Nix-checksummed per ADR-046's
      "source-checkout builds never depend on mvm-published
      artifacts" rule). Shipped: `firecracker` added to
      `builderPackages` + an `extraFiles` symlink pins the exact
      `/usr/bin/firecracker` path the guest spawns.
- [x] **A2.2** Guest-side: `mvm-host-vm-init`'s `WorkloadStart`
      arm spawns a workload microVM with the workload's kernel +
      rootfs + virtio config (passed in the request payload). The
      payload carries kernel path, rootfs path, vsock socket dir,
      vcpus, memory, kernel cmdline extras (generic microVM
      concepts, not Firecracker-shaped). **No VMM lock-in**: a
      `WorkloadVmm` trait (`workload.rs`) isolates the config-file
      format / binary / argv; `FirecrackerVmm` is the only
      Firecracker-aware type, so a second backend is a pure
      addition. `WorkloadFailed` added to the response enum as the
      fail-closed negative path. Spawns via `firecracker
      --config-file …`.
- [x] **A2.3** Per-workload state dir inside the host VM:
      `/var/lib/mvm/workloads/<workload_id>/` holding `config.json`,
      `fc.pid`, `fc.stdout.log`, `fc.stderr.log`, and the `v.sock`
      vsock UDS. Collision-detecting create (fail-closed on a
      duplicate id), `Drop` cleanup so a panic mid-spawn doesn't
      leak, explicit cleanup on `WorkloadStop`.
- [x] **A2.4** Live smoke (runner-direct variant). The
      `workload-spawn-smoke-linux` CI lane (`ci.yml`) drives the real
      A2.2/A2.3 path: `workload::start_workload` + `FirecrackerVmm`
      boot a *live* Firecracker workload microVM and the env-gated
      `workload::tests::live_firecracker_boot_smoke` test asserts the
      kernel executed (boot marker on the guest serial), then tears
      it down. **Scope clarification (was over-claimed in the
      original line):** the GHA runner stands in for the host VM, so
      Firecracker uses the runner's `/dev/kvm` *directly* (L1) — no
      nested KVM needed (the original "nested KVM enabled by default
      on GHA" assumption doesn't hold for stock runners; L2 nesting
      requires a self-hosted / nested-virt runner). This proves the
      *spawn path* with a real VMM. The genuine libkrun-host-VM
      *nesting* (L2 — Firecracker inside a libkrun guest, the
      no-`ptrace` trust uplift) is validated by **A4.5**'s live-KVM
      smoke, not here.
- [x] **A2.5** Reproducibility check: `builder-vm-image-reproducibility`
      lane in `security.yml` double-builds `nix/images/builder-vm/`
      (`--rebuild`) and asserts the four outputs (vmlinux, rootfs.ext4,
      cmdline.txt, manifest.json) are byte-identical. Plan 25 W5.3
      pattern; guards A2.1's firecracker closure against
      non-determinism. PR-skipped (heavy) — runs on release tags,
      nightly cron, and `workflow_dispatch`, like the existing
      builder-image reproducibility lane.

Exit: the `workload-spawn-smoke-linux` lane proves `start_workload`
produces a booting Firecracker workload at L1 on a stock runner. No
`mvmctl up` plumbing yet — the test calls the spawn path directly.
The full host-VM-nested boot is deferred to A4.5.

### Phase A3 — Vsock proxy: add the nesting hop

Goal: the workload microVM's vsock surfaces to the host via the
host-VM as a proxy hop.

- [ ] **A3.1** Today's vsock proxy (host → libkrun guest, port
      5252) gains a hop: host → host-VM vsock → workload Firecracker
      vsock. The proxy plumbing lives in `crates/mvm-supervisor/`
      (confirm during impl; if it's split across crates the slice
      grows). New socket pair per workload, keyed by workload_id.
- [ ] **A3.2** `mvm-guest-agent` (inside workload microVM) stays
      unchanged. It still connects to vsock CID 3 port 5252; that
      vsock is now Firecracker's, inside the host VM, instead of
      the host's directly. No protocol changes inside the workload.
- [ ] **A3.3** Audit-chain entries flow through the proxy
      unchanged — Plan 100 W5's parity assertion. Test:
      `plan.admitted` / `plan.launched` / `plan.failed` round-trip
      identically through nested vs. direct paths.
- [ ] **A3.4** Per-flow guardrails: rate limit, max concurrent
      proxied vsock streams. If Plan 102 W6.A.5's bridge is merged
      by A3 start, inherit those guardrails; otherwise add a
      bounded `tokio::sync::Semaphore` for the proxy and a TODO
      pointing to Plan 102.
- [ ] **A3.5** Hermetic tests — vsock framing round-trip across
      the new hop, tampered-frame rejection, oversized-payload
      drop, ratelimit kicks in.

Exit: a workload-vsock round-trip test passes end-to-end through
the new nesting hop with bit-equivalent payloads vs. direct.

### Phase A4 — Linux `mvmctl up` wires the new path

Goal: when `MVM_LINUX_BUILDER_VM=1` is set,
`FirecrackerBackend::start` on Linux dispatches to the persistent
host VM instead of launching Firecracker directly.

- [ ] **A4.1** Modify `crates/mvm-backend/src/firecracker.rs::
      FirecrackerBackend::start` so that on Linux, when
      `linux_builder_vm_requested()` (Plan 105 W1) returns true,
      the backend obtains a `LibkrunPersistentHostVm` handle from
      the session (creates one if absent) and sends `WorkloadStart`
      instead of forking Firecracker directly.
- [ ] **A4.2** When the env is unset, the direct-Firecracker path
      stays unchanged (rollout safety; matches Plan 100 W1's
      env-gate intent).
- [ ] **A4.3** Host VM lifecycle: the `LibkrunPersistentHostVm`
      handle is session-scoped — created lazily on first
      `mvmctl up` / `mvmctl run`, torn down on `mvmctl dev down`
      or session exit (whichever fires first). Crash recovery:
      if the host VM dies mid-session, the next `mvmctl up`
      restarts it cleanly (no half-state in state dir).
- [ ] **A4.4** Doctor probe extended: `builder backend` line gains
      a "Plan 100 W6 active — host VM running, pid <N>" status
      when the host VM is up.
- [ ] **A4.5** Live-KVM smoke (Plan 100 W4 finally lands): the
      workload boots end-to-end, signed plan + dm-verity rootfs
      + audit chain entries all flow through the nested layer.
      Assert byte-equivalent `ExecutionPlan` admission outcomes
      vs. the direct path.

Exit: `MVM_LINUX_BUILDER_VM=1 cargo run -- run <workload>` on a
Linux + nested-KVM host produces a working workload microVM via
the host-VM path. Plan 100 W4 + W5 close.

### Phase A5 — Retire the direct-Firecracker path

Goal: the env-gate flips to default-on; the direct path retires.

- [ ] **A5.1** Flip `MVM_LINUX_BUILDER_VM` default to "on" (or,
      preferably, retire the env entirely — see A5.3). Env becomes
      the opt-OUT for a one-release rollback window (`=0` for the
      legacy direct path), analogous to how libkrun became the
      macOS default.
- [ ] **A5.2** Delete the direct-Firecracker launch in
      `crates/mvm-backend/src/firecracker.rs` once the new path is
      proven across one release cycle without regression. Plan 100
      W6's stated endpoint.
- [ ] **A5.3** Remove the `MVM_LINUX_BUILDER_VM` env entirely once
      the legacy path is gone. Plan 100 W6 close-out. The
      `linux_builder_vm_requested()` predicate retires; doctor's
      `nested-kvm` line stays but reports unconditionally.
- [ ] **A5.4** Decide whether `LibkrunPersistentHostVm` deserves
      its own `mvm-host-vm` crate. Trigger: if A1–A4 grew
      `mvm-build` past a reviewer-friendly size, split here.
      Otherwise keep co-located.

Exit: Linux has one workload path. Direct-Firecracker on Linux is
gone. Plan 100 W6 closes.

### Phase A6 — Docs + claim rewording

Goal: documentation matches the new posture.

- [ ] **A6.1** Update `CLAUDE.md` architecture diagram: Linux row
      becomes `Host → libkrun Linux VM → Firecracker microVM`.
      Update the "Linux host dependencies" section to note that
      libkrun is now required on Linux contributor hosts (passt
      stays the Linux gateway).
- [ ] **A6.2** Reword ADR-002 Claim 1 per Plan 100 W8:
      "...via identical builder-VM TCB on macOS and Linux." Add a
      live-KVM test reference proving host userland can't `ptrace`
      the workload Firecracker.
- [ ] **A6.3** ADR-001 cross-reference: the new nested model
      replaces the "Firecracker-only on Linux host" framing.
      Cross-link ADR-057 from ADR-001 and ADR-002.
- [ ] **A6.4** CI gate (Plan 100 W8): grep the integration test
      output for a builder-VM ancestor in the Firecracker process
      tree. Fails the build if Linux ever launches Firecracker
      without a libkrun parent post-A5.
- [ ] **A6.5** SPRINT.md update: tick Plan 100 W4/W5/W6/W7/W8
      checkboxes; mark the symmetric-builder-VM rollout as
      shipped.

Exit: Plan 100 closes entirely. ADR-002 Claim 1 carries the new
evidence; ADR-057's intent is realised in CI.

## Out of scope

- Apple Container backend (Plan 97).
- Vz backend (Plan 98).
- Plan 101/102 gateway audit work (in flight, separate sprint).
- Plan 104 host services broker (in flight, separate sprint).
- mvmd cross-repo runtime contract changes — W6's host-VM signal
  is host-local; mvmd doesn't see the dispatch layer. **A4.1
  validates this assumption** before merging; if mvmd has
  Firecracker-path assumptions (e.g. reading `/proc/<fc_pid>/...`
  from the host), the contract needs a cross-repo update tracked
  in a separate plan.
- macOS workload nesting: today macOS uses `LibkrunBackend`
  directly without Firecracker; Approach A doesn't change that.

## Risks + mitigations

- **R1 — Cold-start regression on the first workload.** Plan 100
  W0 measures the libkrun host VM cold-boot budget. A4.5 records
  observed numbers in the PR description. If first-workload
  latency is unacceptable, A5 stays gated until the warm-pool
  benefit demonstrably outweighs it.
- **R2 — Cross-workload DoS inside the host VM.** A bad workload
  can't crash siblings (each has its own Firecracker), but a
  kernel-panic in the host VM itself takes everything down.
  Mitigation: aggressive host-VM health monitoring + per-session
  recovery in A4.3.
- **R3 — Nested-KVM availability on cloud Linux runners.** Some
  cloud hypervisors disable nested virt by default. Plan 105 W1's
  `linux_builder_vm_readiness()` already refuses with an
  actionable error; this stays the operator-facing failure mode.
- **R4 — Firecracker-in-rootfs binary distribution.** Today
  Firecracker is host-installed on Linux. Baking it into the
  builder-vm rootfs (A2.1) is a new artifact distribution path —
  must source from a verified upstream Nix package or build from
  source per ADR-046's source-checkout-only rule.
- **R5 — mvmd integration.** mvmd's runtime contract assumes
  Firecracker-on-host. Validate before A4 ships; if mvmd has
  Firecracker-path assumptions (e.g. reading `/proc/<fc_pid>/...`
  from the host), open a tracking issue.

## Critical files

### Existing surface to extend

- `crates/mvm-build/src/libkrun_builder.rs::LibkrunPersistentBuilderVm`
  — the persistent-VM pattern A1.2 generalises (hard rename to
  `LibkrunPersistentHostVm`).
- `crates/mvm-build/src/builder_protocol.rs` — `BuilderRequest` /
  `BuilderResponse` framing; A1.1 extracts the generalised
  `HostVmRequest` / `HostVmResponse` layer.
- `crates/mvm-build/src/persistent_builder.rs` — host-side
  dispatch supervisor; A1.2 + A4.1 extend it.
- `crates/mvm-builder-init/` — guest-side dispatch loop; A1.3
  renames to `mvm-host-vm-init`.
- `crates/mvm-backend/src/firecracker.rs::FirecrackerBackend::start`
  — A4.1's redirect target.
- `crates/mvm-build/src/builder_backend_select.rs::linux_builder_vm_requested`
  — Plan 105 W1's signal A4.1 consumes.
- `crates/mvm-supervisor/` — vsock proxy layer A3 extends.
- `crates/mvm-cli/src/doctor.rs` — A4.4's status line extension.
- `crates/mvm-core/src/platform/platform.rs::has_nested_kvm` —
  Plan 105 W1's predicate, consumed by A4.1.

### New surface

- `LibkrunPersistentHostVm` type (location: `mvm-build` per A1.2
  default; revisit in A5.4).
- `HostVmRequest::WorkloadStart` / `WorkloadStop` /
  `WorkloadStatus` protocol variants.
- Firecracker binary in `nix/images/builder-vm/` (A2.1).
- `/var/lib/mvm/workloads/<workload_id>/` state-dir layout (A2.3).
- Vsock-proxy nesting-hop plumbing (A3.1).

### Reused untouched

- `crates/mvm-vz/`, `crates/mvm-vz-supervisor/` — macOS Vz path
  unchanged.
- `crates/mvm-cli/src/commands/env/dev.rs` — `mvmctl dev` already
  selects the right backend via Plan 98's selection layer.
- `mvm_sdk::compile::deps_audit::verify_sealed_volume` — Claim 9
  evidence; flows through unchanged because workload artifacts
  are unchanged.
- `mvm-guest-agent` — inside workload microVM, sees vsock CID 3
  port 5252 regardless of nesting.
- Plan 64 `ExecutionPlan` + audit chain — A3.3 asserts identical
  entries across direct vs. nested paths.

## Verification

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
ADR-002 Claim 1 cites post-A6.2.

Per-phase exit criteria are stated above; each phase ships as
one PR with `cargo test --workspace` + `just lint` green.
