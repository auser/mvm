# Plan 67 — Live coverage for `volume mount` / `unmount`

> Two Emits rows in `AUDIT_POSTURE` — `VmVolumeAdd` (mount) and
> `VmVolumeRemove` (unmount) — sit behind a path that mixes
> `MountPathPolicy` validation, the per-VM volume registry, and a
> virtio-fs daemon attach to a running Firecracker socket. Plan 67
> makes the verbs hermetically testable by giving `MockBackend` a
> volume registry surface that the CLI verbs can read+write without
> reaching the virtio-fs daemon.
>
> Roughly 1–2 days, 2 workstreams.

**Status (2026-05-12)**: not started. Substrate dependency is plan
65's W1 (per-VM directory layout for the mock). ADR-045 documents
the architectural decision.

## Context

`crates/mvm-cli/src/commands/vm/volume.rs` flow for `volume mount`:

```
mvmctl volume mount <vm> <host_path> <guest_mount>
  ├── validate_vm_name(vm)
  ├── MountPathPolicy::validate(host_path, guest_mount)
  ├── resolve_running_vm_dir(vm) → vm_dir
  ├── attach virtio-fs daemon to <vm_dir>/runtime/firecracker.socket
  ├── append to per-VM volume registry at <vm_dir>/volumes.json
  └── audit_emit!(VmVolumeAdd, vm: vm, "host={} guest={}", ...)
```

The virtio-fs daemon attach is the only step that needs a real
Firecracker. The rest is local filesystem + JSON. With a mock VM
whose `vm_dir` exists (plan 66 W1) and a mock virtio-fs attach that
no-ops, the verb runs end-to-end.

## State of play

### Already in `origin/main`

- `MountPathPolicy::validate` in `mvm-security::policy::mount_path`.
- Per-VM volume registry serde types.
- `volume.rs` CLI verb.

### Missing (integration)

1. `MockBackend` doesn't have a `mount` / `unmount` trait surface.
2. `volume.rs` doesn't route through `AnyBackend` at all today —
   it calls into Firecracker directly.

## Workstreams

### W1 — `VmBackend::mount_volume` / `unmount_volume` trait surface (~½ day)

**Goal**: add the trait methods so `MockBackend` can satisfy them
in-memory and `FirecrackerBackend` can wrap the real virtio-fs attach.

**Action**:

- New trait methods in `crates/mvm-core/src/protocol/vm_backend.rs`:
  ```rust
  fn mount_volume(
      &self,
      id: &VmId,
      host_path: &Path,
      guest_mount: &Path,
      read_only: bool,
  ) -> Result<()>;

  fn unmount_volume(&self, id: &VmId, guest_mount: &Path) -> Result<()>;
  ```
  Default impl bails with "backend does not support volume mounts".
- `FirecrackerBackend::mount_volume` lifts the existing virtio-fs
  attach logic from `volume.rs`.
- `MockBackend::mount_volume` appends `(host_path, guest_mount,
  read_only)` to an in-memory `Vec` per VM; never touches the FS.
- `MockBackend::unmount_volume` removes from the in-memory vec.

**Exit tests**:

- `mock_mount_volume_records_in_memory`.
- `mock_unmount_volume_idempotent_on_missing_path`.
- `mock_capabilities_advertises_volume_mount_when_appropriate`.

### W2 — Refactor `volume.rs` + add live tests (~1 day)

**Goal**: route the CLI verb through `AnyBackend.mount_volume` and
pin both Emits rows.

**Action**:

- `volume.rs` calls `AnyBackend::from_hypervisor(...)` (with the
  same hypervisor-from-registry pattern plan 65 establishes for
  pause/resume) and then `backend.mount_volume(...)`.
- The per-VM volume registry append stays in `volume.rs` (it's a
  host-side concern, not a backend concern).
- `audit_emit!` already in place.

**Live tests** in `tests/audit_emissions_live.rs`:

```rust
#[test]
fn volume_mount_emits_vm_volume_add_audit_entry() {
    let sandbox = AuditSandbox::new();
    bring_up_mock_vm(&sandbox, "test-vol-vm");
    let host_path = sandbox.home_path().join("shared");
    std::fs::create_dir(&host_path).expect("mkdir host_path");
    let output = sandbox.mvmctl()
        .args([
            "volume", "mount", "test-vol-vm",
            host_path.to_str().unwrap(), "/mnt/shared",
            "--hypervisor", "mock",
        ])
        .output().expect("spawn");
    assert!(output.status.success(), ...);
    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(count_entries_with_kind(&log, "vm_volume_add") >= 1);
}

#[test]
fn volume_unmount_emits_vm_volume_remove_audit_entry() { /* mirror */ }
```

**Exit criteria**:

- Both tests pass.
- `cargo test --workspace --no-fail-fast` clean.
- xtask lints clean.

## Phasing

W1 → W2. Both are small; could ship as one PR.

## Non-goals

- **virtio-fs daemon process management.** The mock backend never
  spawns a daemon; the trait method is a no-op return for mock.
- **MountPathPolicy refactor.** The existing security validation
  runs *before* the backend call and stays in `volume.rs`.

## Success criteria

By plan 67 close:

1. `VmBackend::mount_volume` / `unmount_volume` exist; both backends
   implement them.
2. `volume.rs` routes through `AnyBackend`.
3. `VmVolumeAdd` and `VmVolumeRemove` rows are live-pinned.

## Risk notes

- **MountPathPolicy false-negatives.** The policy may reject
  tempdir-rooted paths. Pre-test with a real `mvmctl volume mount`
  invocation in a sandbox before committing; tweak the test paths
  if validation refuses.
