# Plan 105 — Plan 100 W1: env-gated Linux builder VM dispatch

> **Status (2026-05-27):** unstarted. Implementation tracker for
> `specs/plans/100-symmetric-builder-vm-rollout.md` §W1 (and a small
> §W3 sub-slice — the `mvmctl doctor` `nested-kvm` line). Lands the
> dispatch flip that lets Linux contributors opt into the
> libkrun-builder-VM-on-Linux path Plan 100 ultimately makes the
> default. Plan 100 itself stays the parent spec; this plan exists
> only to track the implementation checkpoints for that first wave.
>
> Picks up after Plan 98 (Vz builder backend on macOS) shipped end-
> to-end. Plan 100 W1 is the natural next architectural slice: same
> `mvm_build::builder_backend_select::resolve_choice` priority
> resolver Plan 98 introduced, extended with an env-gated rollout
> switch for the Linux side.
>
> Pick-up command for fresh sessions: read this file top to bottom,
> then jump to the next unchecked item.

## Context

ADR-057 (`specs/adrs/057-symmetric-builder-vm.md`) argues that the
trust-claim story for `mvmctl` workloads is currently *asymmetric*:

- **macOS** runs every workload inside a libkrun builder VM, so the
  host userland is not in the workload TCB.
- **Linux** runs Firecracker directly on the host, so a host process
  can `ptrace` Firecracker or read its `/proc/<pid>/mem` without
  crossing a hypervisor boundary.

Plan 100 ultimately closes the gap by making the libkrun builder VM
the cross-platform substrate on Linux too (nested KVM). Plan 100 W6
retires the direct-Firecracker path entirely.

This plan covers Plan 100 §W1 — the smallest valuable first slice.
It does *not* default-on the new path; it adds an opt-in env gate so
contributors can measure cold-start latency, exercise the Linux
builder image build, and surface regressions before the default
flips in W6.

## Selection policy (locked)

- **Default unchanged.** On Linux, `mvmctl build` still dispatches to
  the direct-Firecracker path. No behaviour change for existing
  contributors.
- **Opt-in via `MVM_LINUX_BUILDER_VM=1`.** When set on Linux,
  `resolve_builder_backend()` returns a `LibkrunBuilderVm` instance
  instead of the Firecracker direct path. Existing `--builder` flag
  and `MVM_BUILDER_BACKEND` env priority is unchanged; the new env is
  layered on top.
- **Nested-KVM precondition.** The dispatch refuses cleanly when
  `MVM_LINUX_BUILDER_VM=1` is set on a host without nested KVM
  (`kvm-intel.nested=1` or `kvm-amd.nested=1`). Error message points
  at the kernel-module parameter the operator needs to enable.
- **macOS unchanged.** `MVM_LINUX_BUILDER_VM` is Linux-only — setting
  it on macOS is a no-op (libkrun is already the macOS default per
  Plan 98).

## Progress checklist

Phases are PR-sized slices. Each ends with `cargo test --workspace`
+ `just lint` green. The hardware-dependent measurement (W0) is a
throw-away branch whose numbers feed into the W1 PR description —
not its own PR.

### Phase W0 — feasibility prototype (off-branch, throw-away)

- [ ] **W0.1** On a Linux host with nested KVM enabled, throw together
      a minimal program that constructs `LibkrunBuilderVm::new(...)`
      and calls `run_build` against a known-good `examples/python/hello-app-with-deps`
      flake. Time cold boot + warm rebuild. No PR; numbers go in the
      W1 PR body so reviewers see the cold-start budget holds.
- [ ] **W0.2** Plan 100 names <30 s warm-cache rebuild as the
      acceptance bar. Record actual numbers and any host-tuning
      footnotes (huge pages, cgroup placement, etc.).

### Phase W1 — backend dispatch change

Single small PR. ~80 lines including hermetic unit tests.

- [ ] **W1.1** Add `Platform::has_nested_kvm() -> bool` to
      `crates/mvm-core/src/platform/platform.rs`. Linux-only probe of
      `/sys/module/kvm_intel/parameters/nested` (must read `Y`) or
      `/sys/module/kvm_amd/parameters/nested` (must read `1`). Hermetic
      unit tests with mocked sysfs paths or pure-fn variant that takes
      the path as an arg.
- [ ] **W1.2** Add `MVM_LINUX_BUILDER_VM` env constant to
      `crates/mvm-build/src/builder_backend_select.rs`.
- [ ] **W1.3** Extend `resolve_choice_with_override` (or add a sibling
      `resolve_choice_with_linux_builder_override`) so that on Linux,
      `MVM_LINUX_BUILDER_VM=1` resolves to `Libkrun` (which then routes
      through `LibkrunBuilderVm::new` since `LibkrunBuilderVm` is
      already cross-platform per `crates/mvm-build/src/libkrun_builder.rs:1688`).
- [ ] **W1.4** When `MVM_LINUX_BUILDER_VM=1` is set on Linux but
      `has_nested_kvm()` is false, return
      `BuilderVmError::VmmUnavailable { requested: "linux-builder-vm",
      reason: "<actionable hint about kvm-intel.nested / kvm-amd.nested>" }`.
      Refuses with a clear error rather than silently falling through.
- [ ] **W1.5** Hermetic unit tests (mirrors the Plan 98 Slice 2B
      pattern):
      - `linux_builder_vm_env_set_picks_libkrun_when_nested_kvm` —
        injected `(Platform::LinuxNative, has_nested_kvm=true)`
        resolves to libkrun.
      - `linux_builder_vm_env_set_refuses_without_nested_kvm` —
        injected `(Platform::LinuxNative, has_nested_kvm=false)`
        returns the clean error.
      - `linux_builder_vm_env_unset_unchanged` — the env-unset path
        on Linux falls through to whatever the existing resolver did
        (direct-Firecracker today).
      - `linux_builder_vm_env_on_macos_is_noop` — Linux-only env gate
        doesn't affect macOS dispatch.
      - `flag_overrides_linux_builder_vm_env` — explicit
        `--builder libkrun` flag still wins over the env.
- [ ] **W1.6** `cargo test --workspace` + `just lint` green.
- [ ] **W1.7** Open PR as non-draft (Plan 98 Phase 1 locked decision
      #4 pattern). Title: `feat(mvm-build): Plan 100 W1 — env-gated
      Linux builder VM dispatch`. Body includes the W0 cold-start
      numbers as evidence the cold-start budget holds.

### Phase W3-doctor sub-slice — `mvmctl doctor` surfaces nested-KVM

Small standalone follow-up PR; can land in parallel with W1 once W1's
predicate exists.

- [ ] **W3-D.1** Add a `nested-kvm` check function to
      `crates/mvm-cli/src/doctor.rs` parallel to the existing `kvm`
      line. `#[cfg(target_os = "linux")]` only; on macOS the check
      reports `n/a (Linux-only — libkrun is the macOS default)`.
- [ ] **W3-D.2** Extend the `builder backend` line (Plan 98 §1.5) to
      mention `MVM_LINUX_BUILDER_VM` when set on Linux. Format:
      `<backend> — <source> — <availability>` stays; source can now
      be `env (MVM_LINUX_BUILDER_VM=1)` on Linux.
- [ ] **W3-D.3** Hermetic unit tests for the `nested-kvm` Linux line
      output, mirroring the existing `vz_check` test pattern.
- [ ] **W3-D.4** `cargo test --workspace` + `just lint` green.
- [ ] **W3-D.5** PR opened, review requested.

### Out of scope (deferred to Plan 100 W2-W6)

- **W2** — Linux image build validation (Nix flake produces the same
  `vmlinux` + `rootfs.ext4` artifacts the macOS path consumes).
  Separate PR; needs a CI lane that actually exercises the Linux
  builder VM boot.
- **W4** — Nested-KVM CI lane in `.github/workflows/ci.yml`. Linux
  job that opt-in sets `MVM_LINUX_BUILDER_VM=1` and runs a flake-build
  smoke. Easier to land once W1's measured numbers exist.
- **W5** — Persistent-builder variant on Linux. Mirrors Plan 98 Slice
  2A's `VzPersistentBuilderVm` shape, only with libkrun-on-Linux as
  the backing VMM. Distinct PR; depends on W1 dispatch being stable.
- **W6** — Retire the direct-Firecracker code path
  (`crates/mvm-backend/src/firecracker.rs`). Gated on W4 CI proof +
  Plan 101 Leg 1 (volume encryption) readiness so the trust uplift
  ADR-057 promises actually lands at flip-time, not before.
- **W7/W8** — ADR-001 update for the nested-execution model + ADR-002
  Claim 1 rewording. Prose follow-ups.

---

## Verification

End-to-end on a Linux + nested-KVM host:

1. `MVM_LINUX_BUILDER_VM=1 cargo run -- build --flake . --profile minimal --role worker`
   — cold-boot completes inside the W0 budget, builder VM produces
   `vmlinux` + `rootfs.ext4`, same output as the direct-Firecracker
   path. `mvmctl audit verify` round-trips clean.
2. `cargo run -- build ...` (no env) — direct-Firecracker path
   unchanged. Regression check.
3. `cargo run -- doctor` — reports nested-KVM availability + which
   builder backend would be selected.
4. On Linux without nested KVM: `MVM_LINUX_BUILDER_VM=1 cargo run
   -- build ...` exits non-zero with the actionable
   `kvm-intel.nested` / `kvm-amd.nested` hint.

End-to-end on macOS:

5. `MVM_LINUX_BUILDER_VM=1 cargo run -- build --flake . ...` — no
   behaviour change; the env is Linux-only and is silently ignored
   on macOS (same path the existing flag/env-already-picks-libkrun
   would take).

Automated:

- `cargo test -p mvm-build --features builder-vm builder_backend_select`
  covers the new env-gated branch + 5 hermetic test cases.
- `cargo test -p mvm-core platform` covers `has_nested_kvm()` with
  injected sysfs paths.
- `cargo test --workspace` + `just lint` green.
- Existing `app-deps-audit` CI lane unchanged — Install jobs flow
  through whichever backend dispatch resolves.

## Files (summary)

### New
- (none — this plan is implementation-only)

### Modified
- `crates/mvm-build/src/builder_backend_select.rs` — new
  `MVM_LINUX_BUILDER_VM` env + dispatch branch + 5 unit tests.
- `crates/mvm-core/src/platform/platform.rs` — `has_nested_kvm()`
  predicate.
- `crates/mvm-cli/src/doctor.rs` — `nested-kvm` line + extended
  `builder backend` line (W3-D sub-slice).
- `specs/SPRINT.md` — Sprint 56 W1 progress note flipping to in-flight
  once W1 PR opens, then to merged once it lands.

### Reused (do not modify; reference)
- `crates/mvm-build/src/libkrun_builder.rs::LibkrunBuilderVm::new`
  — already cross-platform; works on Linux out of the box (line ~1688).
- `mvm_core::platform::Platform::has_kvm` — sister predicate for the
  nested-KVM probe.
- `mvm_build::builder_backend_select::resolve_choice` — Plan 98 Phase 1's
  priority resolver. The new env-gate slots in as a Linux-only
  precondition on the libkrun arm.
- `mvm_core::vm_backend::VmBackend` trait + `VmStartConfig` — runtime
  contract the dispatch returns into.

### Deleted
- (none)
