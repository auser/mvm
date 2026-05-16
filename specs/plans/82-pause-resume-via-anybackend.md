# Plan 65 — Route `mvmctl pause` / `resume` through `AnyBackend`

> The pause/resume CLI verbs talk directly to a Firecracker UDS via
> `FirecrackerIO` and bypass the `VmBackend::pause`/`resume` trait
> methods that the trait already defines. That hard-codes a
> backend choice and blocks hermetic audit-emit coverage of the
> `WorkloadSleep` / `WorkloadWake` rows. Plan 65 routes both verbs
> through `AnyBackend`, preserving the existing snapshot
> semantics as a separate orthogonal step.
>
> Roughly 2–3 days, 3 workstreams.

**Status (2026-05-12)**: not started. Substrate dependency is PR #108
(`MockBackend` + `AnyBackend::Mock` variant + `bring_up_mock_vm`
fixture). ADR-045 documents the architectural decision.

## Context

`crates/mvm-cli/src/commands/vm/pause.rs` and the same file's
`run_resume` both call `mvm_backend::microvm::resolve_running_vm_dir`
+ `FirecrackerIO::new(socket)` + `pause_and_seal` /
`verify_and_resume`. The flow:

```
mvmctl pause <vm>
  ├── resolve_running_vm_dir(vm) → vm_dir   [Firecracker-specific path]
  ├── FirecrackerIO::new(<vm_dir>/runtime/firecracker.socket)
  ├── pause_and_seal(vm, &io)
  │     ├── PUT /vm {"state":"Paused"}      via UDS
  │     ├── PUT /snapshot/create ...        via UDS — writes vmstate + mem files
  │     └── return IntegritySidecar { epoch, vmstate_len, mem_len }
  ├── registry.set_paused(vm, true)
  └── audit_emit!(WorkloadSleep, vm: vm, "epoch={} vmstate={} mem={}", …)
```

The `audit_emit!` only fires after `pause_and_seal` succeeds. Without
a real Firecracker socket the verb bails at step 1 (the `?` on
`resolve_running_vm_dir`), and the `WorkloadSleep` row stays untestable.

`VmBackend::pause(&VmId)` and `VmBackend::resume(&VmId)` exist on the
trait. `MockBackend` implements them (flips a `paused` bool in its
in-memory record). `FirecrackerBackend::pause` already wraps
`microvm::pause_vm(name)` — just the vCPU pause, no snapshot.

So the trait has the right shape; the CLI verb just doesn't use it.

## State of play

### Already in `origin/main` (substrate)

- `VmBackend::pause` / `resume` trait methods + capability flag.
- `FirecrackerBackend::pause` / `resume` (vCPU-only).
- `MockBackend::pause` / `resume` (in-memory; flips paused flag).
- `pause_and_seal` / `verify_and_resume` in
  `crates/mvm/src/vm/instance_snapshot.rs` — pause + write snapshot
  files in one step.
- `AuditSandbox` test fixture + `bring_up_mock_vm` helper.

### Missing (integration)

1. `pause.rs::run_pause` and `run_resume` bypass `AnyBackend`. The
   refactor below routes them through it.
2. The snapshot side-effect (`pause_and_seal` writing vmstate + mem
   files) is currently inlined; it needs to move into a backend
   capability or a separate call to keep it post-refactor.
3. No live tests for `WorkloadSleep` / `WorkloadWake`.

## Workstreams

Three workstreams, each independently mergeable.

### W1 — Add `snapshot` / `restore` to `VmBackend` trait (~1 day)

**Goal**: split the "pause vCPUs" responsibility from the "write
snapshot files" responsibility at the trait level.

**Action**:

- New trait methods in `crates/mvm-core/src/protocol/vm_backend.rs`:
  ```rust
  fn snapshot(&self, id: &VmId) -> Result<SnapshotArtifacts>;
  fn restore(&self, id: &VmId, artifacts: &SnapshotArtifacts) -> Result<()>;
  ```
  Default impl: bails with "backend does not support snapshots".
  Backends override based on their `VmCapabilities::snapshots` flag.
- `FirecrackerBackend::snapshot` wraps the existing Firecracker
  `PUT /snapshot/create` logic from `instance_snapshot::pause_and_seal`
  *minus* the pause step (that's now `pause`'s job alone).
- `MockBackend::snapshot` writes stub artifact files into a
  per-VM tempdir under the data dir; returns deterministic sizes
  (e.g. `epoch=1, vmstate_len=42, mem_len=84`).
- `SnapshotArtifacts` lives in `mvm-core`: `{ epoch: u64, vmstate_path:
  PathBuf, mem_path: PathBuf, vmstate_len: u64, mem_len: u64 }`.

**Exit tests**:

- `firecracker_snapshot_writes_files_to_vm_dir` (unit, uses a stub
  Firecracker socket via the existing `FirecrackerIO` test plumbing).
- `mock_snapshot_writes_stub_artifacts_to_tempdir`.
- `mock_restore_reads_artifacts_back` (round-trip).
- `mock_snapshot_capability_flag_matches_impl` (caps.snapshots is true).

### W2 — Refactor `pause.rs` to use the trait (~1 day)

**Goal**: `run_pause` / `run_resume` use `AnyBackend.pause` +
`AnyBackend.snapshot` (composition), eliminating the
`FirecrackerIO` import from the CLI layer.

**Action**:

- `pause.rs::run_pause`:
  ```rust
  let backend = AnyBackend::from_hypervisor(/* registry-recorded hypervisor */);
  backend.pause(&VmId(args.name.clone()))?;
  let artifacts = backend.snapshot(&VmId(args.name.clone()))?;
  registry.set_paused(&args.name, true);
  println!("{}: paused (epoch {}, vmstate {} B, mem {} B)", ...);
  mvm_core::audit_emit!(WorkloadSleep, vm: &args.name,
      "epoch={} vmstate={} mem={}",
      artifacts.epoch, artifacts.vmstate_len, artifacts.mem_len);
  ```
- `run_resume` mirrors: `backend.restore(...)?; backend.resume(...)?`.
- The hypervisor selection mirrors `down.rs` — auto-detect from
  registry + platform, with an optional `--hypervisor` flag added
  (matches the `up`/`from_hypervisor` pattern). Default behavior
  for production callers is unchanged.
- Delete `pause_and_seal` and `verify_and_resume` from
  `crates/mvm/src/vm/instance_snapshot.rs`; the `FirecrackerIO`
  imports in `pause.rs` go away.

**Exit tests**:

- `run_pause_against_firecracker_writes_audit_with_real_sizes`
  (unit, in `pause.rs`; backend trait mocked).
- The existing snapshot-integrity tests in `mvm-base/snapshot_integrity`
  continue to pass — those test the on-disk sidecar shape and are
  orthogonal to who writes the files.

### W3 — Live coverage for `pause` / `resume` (~½ day)

**Goal**: `WorkloadSleep` and `WorkloadWake` rows graduate from
classification-only to live drive-and-assert.

**Action**:

- `tests/audit_emissions_live.rs`:
  ```rust
  #[test]
  fn pause_emits_workload_sleep_audit_entry() {
      let sandbox = AuditSandbox::new();
      bring_up_mock_vm(&sandbox, "test-pause-vm");
      let output = sandbox.mvmctl()
          .args(["pause", "test-pause-vm", "--hypervisor", "mock"])
          .output().expect("spawn");
      assert!(output.status.success(), ...);
      let log = read_audit_log(&sandbox.audit_log_path());
      assert!(count_entries_with_kind(&log, "workload_sleep") >= 1);
      assert!(log.contains("\"vm_name\":\"test-pause-vm\""));
      assert!(log.contains("epoch="));
  }

  #[test]
  fn resume_emits_workload_wake_audit_entry() { /* mirror */ }
  ```
- Update the module-doc coverage list at the top of the test file.

**Exit criteria**:

- Both new tests pass.
- `cargo test --workspace --no-fail-fast` clean.
- `cargo run -p xtask -- check-audit-positional` clean.

## Phasing

W1 → W2 (W2 depends on the trait extension). W3 lands after W2. The
suggested PR boundary is one PR per workstream so reviewers can
focus, but W1+W2 could land in a single PR if the diff stays under
~400 lines.

## Non-goals (explicit)

- **Live KVM coverage.** A real Firecracker snapshot is exercised by
  `tests/seccomp_apply.rs` and `tests/smoke_e2e_boot.rs`. Plan 65's
  live tests stay on the mock backend; KVM coverage is separate.
- **`mvmctl up --resume-from-snapshot`.** The CLI surface for
  restoring a snapshot into a fresh VM is plan-46 territory; plan
  65 only handles the trait + verb-side refactor.
- **Multi-snapshot management.** `mvmctl snapshot ls` / `rm` already
  exist; plan 65 doesn't change them.

## Success criteria

By plan 65 close:

1. `VmBackend::snapshot` / `restore` exist on the trait; both
   `FirecrackerBackend` and `MockBackend` implement them.
2. `mvmctl pause` / `resume` go through `AnyBackend`; the
   `FirecrackerIO` import is gone from `pause.rs`.
3. `WorkloadSleep` and `WorkloadWake` rows are live-pinned in
   `tests/audit_emissions_live.rs`.
4. The `audit_total_coverage.rs` classification doesn't change
   (the rows were already `Emits`).
5. `cargo run -p xtask -- check-audit-positional` clean.

## Cross-repo dependencies

None. Plan 65 is purely within mvm.

## Risk notes

- **Snapshot semantics drift.** The current `pause_and_seal` writes
  vmstate + mem files atomically (single Firecracker RPC). Splitting
  into `pause` + `snapshot` introduces a window where the VM is
  paused but the snapshot isn't sealed. Mitigation: keep
  `pause_and_seal` callable internally for the production
  Firecracker path; the trait-routed path uses it under the hood.
  W1's `FirecrackerBackend::snapshot` calls `pause_and_seal`'s
  snapshot-only half; the pause half is `FirecrackerBackend::pause`.
- **Restore ordering.** Firecracker requires restore to happen
  against a fresh microVM shell that's waiting for snapshot load.
  The trait's `restore` method needs to spell out whether the caller
  must `start` first. Existing `verify_and_resume` assumes a running
  shell; the refactor must preserve that contract or document the
  change.
