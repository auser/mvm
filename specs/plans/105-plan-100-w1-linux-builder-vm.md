# Plan 105 — Plan 100 W1 prep: env constant + nested-KVM readiness + doctor probe

> **Status (2026-05-27):** W1 preparatory slice ready for review.
> Lands the env constant, readiness predicate, and `mvmctl doctor`
> `nested-kvm` line so operators can validate their host ahead of
> Plan 100 W6 (the actual dispatch flip — workload path nests through
> a libkrun builder VM on Linux instead of running Firecracker
> directly).
>
> **Scope clarification (2026-05-27):** Plan 100 W1 is described as a
> dispatch flip to `resolve_builder_backend()`, but
> `resolve_builder_backend()` is the *builder VM image build* path
> (Nix-build-runner choice) — it already returns `LibkrunBuilderVm`
> on Linux. The actual Plan 100 architectural change — making the
> *workload* path nest through a libkrun host VM on Linux — touches
> the runtime backend (`crates/mvm-backend/src/backend.rs`), not the
> builder backend, and is properly Plan 100 W6's surface.
>
> Plan 105 (this slice) therefore lands the **preparatory plumbing**:
> a canonical env constant, a readiness predicate that refuses cleanly
> when the operator sets the env on a non-Linux host or without
> nested KVM, and doctor visibility. Plan 100 W6 reads this same
> predicate when it lands the dispatch flip — single source of truth
> for the rollout signal.
>
> Picks up after Plan 98 (Vz builder backend on macOS) shipped end-
> to-end.
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

### Phase W1 — preparatory plumbing (env + readiness predicate)

Single PR; ~370 lines including hermetic unit tests.

- [x] **W1.1** Add `Platform::has_nested_kvm() -> bool` to
      `crates/mvm-core/src/platform/platform.rs`. Linux-only probe of
      `/sys/module/kvm_intel/parameters/nested` (`Y`) or
      `/sys/module/kvm_amd/parameters/nested` (`1`). Lifted out into
      a `has_nested_kvm_at(intel_path, amd_path)` pure helper for
      hermetic tests (six cases — Intel-Y, AMD-1, Intel-N, AMD-0,
      both-missing, lowercase-y).
- [x] **W1.2** Add `MVM_LINUX_BUILDER_VM_ENV` constant to
      `crates/mvm-build/src/builder_backend_select.rs`.
- [x] **W1.3** Add `linux_builder_vm_requested()` predicate (live
      runtime read) + `linux_builder_vm_requested_for(raw)` pure
      predicate. Truthy values: `1`/`true`/`yes`/`on`,
      case-insensitive, whitespace-trimmed. Anything else (including
      `0`, `false`, `no`, `off`, empty, missing) is false. Plan 100
      W6 reads this to decide whether the workload path nests.
- [x] **W1.4** Add `linux_builder_vm_readiness_for(plat, has_nested_kvm)`
      + `linux_builder_vm_readiness()` returning
      `Result<(), BuilderVmError>`. Non-Linux platforms and Linux
      without nested KVM return `BuilderVmError::VmmUnavailable
      { requested: "linux-builder-vm", reason: "<actionable kernel-module hint>" }`.
      New `BuilderVmError::VmmUnavailable { requested, reason }`
      variant added to `crates/mvm-build/src/builder_vm.rs`.
- [x] **W1.5** Hermetic unit tests (9 in `builder_backend_select::tests`):
      - `linux_builder_vm_requested_truthy_values` — 8 truthy variants.
      - `linux_builder_vm_requested_falsey_values` — 9 falsey variants.
      - `linux_builder_vm_requested_none_is_false`.
      - `linux_builder_vm_readiness_ok_when_linux_native_with_nested_kvm`.
      - `linux_builder_vm_readiness_refuses_without_nested_kvm` —
        asserts message mentions `MVM_LINUX_BUILDER_VM`, the
        `kvm_intel`/`kvm_amd` module, and the `nested` parameter.
      - `linux_builder_vm_readiness_refuses_on_macos`.
      - `linux_builder_vm_readiness_refuses_on_wsl2`.
      - `linux_builder_vm_readiness_refuses_on_linux_no_kvm`.
      - `linux_builder_vm_env_constant_is_canonical`.
- [x] **W1.6** `cargo test -p mvm-core platform` + `cargo test -p mvm-build
      builder_backend_select` + `just lint` green.
- [ ] **W1.7** PR opened as non-draft (Plan 98 Phase 1 locked decision
      #4 pattern). Title: `feat(mvm-build): Plan 100 W1 prep —
      MVM_LINUX_BUILDER_VM env + nested-KVM readiness + doctor probe`.

### Phase W3-doctor sub-slice — `mvmctl doctor` surfaces nested-KVM

Bundled with W1 in the same PR — the doctor line consumes W1's
predicate so shipping both in one review keeps the surface coherent.

- [x] **W3-D.1** Added `nested_kvm_check(plat)` to
      `crates/mvm-cli/src/doctor.rs` parallel to `kvm_check`. Linux-
      only — macOS / Windows / WSL2 / LinuxNoKvm get an `n/a
      (Linux-only — …)` line. Linux-native branch reports one of four
      states: env-set + ready ("Plan 100 W6 nesting ready"), env-set
      + missing (hard error with kernel-module fix command), env-
      unset + ready (informational), env-unset + missing
      (informational — enable before opting in).
- [x] **W3-D.2** Extended the `builder backend` line (Plan 98 §1.5)
      to append `; $MVM_LINUX_BUILDER_VM=1 (Plan 100 W6 opt-in)` to
      the source segment when the env is set. The env doesn't change
      *which* backend wins (libkrun stays the Linux default); it
      changes how the workload path will dispatch once Plan 100 W6
      lands.
- [x] **W3-D.3** Hermetic unit tests in `doctor::tests`:
      - `nested_kvm_check_macos_reports_na`
      - `nested_kvm_check_windows_reports_na`
      - `nested_kvm_check_wsl2_reports_na`
      - `nested_kvm_check_linux_native_reports_actionable_text`
        (cfg-gated to `target_os = "linux"`).
      - `builder_backend_check_linux_surfaces_linux_builder_vm_env`
        (cfg-gated to Linux + `builder-vm` feature).
- [x] **W3-D.4** `cargo test -p mvm-cli doctor::tests::nested_kvm` +
      `just lint` green.
- [ ] **W3-D.5** PR landed alongside W1 (bundled).

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
