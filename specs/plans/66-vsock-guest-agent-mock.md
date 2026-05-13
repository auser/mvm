# Plan 66 — Mock vsock guest-agent for `fs` / `proc` live coverage

> `mvmctl fs write/mkdir/rm/chmod` and `mvmctl proc start/signal/kill/stdin/wait`
> connect to the in-guest `mvm-guest-agent` daemon over vsock and RPC
> against it. Eight Emits rows in `AUDIT_POSTURE` (VmFsMutate, VmProcStart,
> VmProcSignal, VmProcStdin, Kill, plus the proc-ls/proc-wait reads)
> sit behind that surface. Plan 66 mocks the vsock + agent protocol
> so each row becomes hermetically testable.
>
> Roughly 4–5 days, 4 workstreams. Biggest substrate investment of
> the audit-emit follow-on plans; pays off across `fs` + `proc` + future
> `exec`/`run-code`/`console` work.

**Status (2026-05-12)**: not started. Substrate dependency is PR #108
(`MockBackend` + `bring_up_mock_vm`). ADR-045 architectural decision.

## Context

`crates/mvm-cli/src/commands/vm/fs.rs` and `proc.rs` both follow the
same shape:

```
mvmctl fs write <vm> <path>
  ├── microvm::resolve_running_vm_dir(vm) → vm_dir
  ├── open <vm_dir>/runtime/agent.sock (vsock UDS proxy)
  ├── send GuestRequest::WriteFile { path, bytes } over the wire
  ├── read GuestResponse::WriteFile { bytes_written }
  └── audit_emit!(VmFsMutate, vm: vm, "op=write path={} bytes={}", ...)
```

The wire types live in `crates/mvm-guest/src/vsock.rs`
(`GuestRequest`, `GuestResponse`, `AuthenticatedFrame`). The host-side
client lives in `mvm/src/vm/guest_client.rs` (or similar — verify).

Two things block hermetic coverage:

1. **`resolve_running_vm_dir`** fails on the mock VM because
   `MockBackend.start` doesn't create the on-disk `vm_dir` layout
   that Firecracker creates.
2. **The vsock proxy socket** never exists for mock VMs.

The fix is a per-VM mock guest-agent that listens on a Unix socket
in the mock VM's home-tempdir-rooted `vm_dir`, implements the
`GuestRequest`/`GuestResponse` protocol deterministically, and is
started as part of the `bring_up_mock_vm` fixture.

## State of play

### Already in `origin/main` (substrate)

- `GuestRequest` / `GuestResponse` / `AuthenticatedFrame` serde
  types in `mvm-guest::vsock`.
- The `mvm-guest-agent` daemon binary in `crates/mvm-guest/src/bin/`.
- Host-side client code (need to confirm exact module path).
- `AuditSandbox` fixture + `bring_up_mock_vm`.

### Missing (integration)

1. `MockBackend.start` doesn't create the per-VM directory layout
   `resolve_running_vm_dir` expects.
2. No mock implementation of the guest-agent RPC surface.
3. No live tests for any `fs` or `proc` row.

## Workstreams

### W1 — `MockBackend` creates the per-VM directory layout (~½ day)

**Goal**: `resolve_running_vm_dir(vm_name)` returns a real path for
mock VMs. The path layout matches Firecracker's so consumers
(`fs.rs`, `proc.rs`, future verbs) don't branch on backend.

**Action**:

- `MockBackend::start_with_mode` creates
  `<mvm_data_dir>/vms/<vm_name>/runtime/` and writes a stub
  `mode.json` (matches what `handle_registry` reads).
- The mock VM's vsock proxy socket path
  `<vm_dir>/runtime/agent.sock` is created in W2 once the mock
  agent is wired in.
- `MockBackend::stop` removes the per-VM directory.
- Unit tests in `mock.rs` assert the layout matches what
  `resolve_running_vm_dir` expects.

**Exit tests**:

- `mock_start_creates_vm_dir_resolve_finds_it`.
- `mock_stop_removes_vm_dir`.
- `mock_start_idempotent_under_second_call_with_same_name` —
  collides → bails (existing behavior).

### W2 — `MockGuestAgent` per-VM Unix socket server (~2 days)

**Goal**: a deterministic, in-process implementation of the
guest-agent RPC surface. Spawned per mock VM; serves the
`GuestRequest`/`GuestResponse` protocol over the per-VM
`agent.sock`.

**Action**:

- New `crates/mvm-backend/src/mock_guest_agent.rs`:
  ```rust
  pub struct MockGuestAgent {
      socket_path: PathBuf,
      state: Arc<Mutex<MockGuestState>>,
      thread: Option<JoinHandle<()>>,
  }

  struct MockGuestState {
      files: HashMap<PathBuf, Vec<u8>>,
      processes: HashMap<String /* token */, MockProc>,
  }

  impl MockGuestAgent {
      pub fn start(vm_dir: &Path) -> Result<Self>;
      pub fn stop(self) -> Result<()>;
  }
  ```
- The thread accepts connections on the socket, reads
  `AuthenticatedFrame::GuestRequest`, dispatches:
  - `WriteFile { path, bytes }` → store in `state.files`, return
    `WriteFile { bytes_written: bytes.len() }`.
  - `Mkdir`, `Rm`, `Chmod` → similar shape; the mock just records
    the operation succeeded and returns the success variant.
  - `ProcStart { argv, env }` → assign a token, record the proc,
    return `ProcStart { token, pid: 1234 }`.
  - `ProcSignal { token, signum }` → look up + record + return
    success.
  - `ProcKill { token }` → remove from state, return success.
  - `ProcStdin { token, bytes }` → return `ProcStdin { bytes_accepted: bytes.len() }`.
  - `ProcLs` → list registered procs.
  - `ProcWait` → returns exit-code 0 deterministically.
- `MockBackend::start_with_mode` spawns + tracks a `MockGuestAgent`
  per VM, stored in the backend's state alongside the `MockVm`.
- `MockBackend::stop` calls `MockGuestAgent::stop` then removes
  the dir.

**Exit tests** (in `mock_guest_agent.rs`):

- `agent_serves_write_file_round_trip`.
- `agent_serves_mkdir_and_rm`.
- `agent_proc_start_assigns_token`.
- `agent_proc_kill_idempotent_on_missing_token`.
- `agent_handles_concurrent_connections` (two clients write
  different files simultaneously).
- `agent_stop_closes_socket_cleanly`.

### W3 — `--hypervisor mock` routing in `fs` / `proc` (~½ day)

**Goal**: when `fs` and `proc` resolve the agent socket via
`resolve_running_vm_dir`, the mock VM's socket is found. No
code change in `fs.rs` / `proc.rs` if W1 + W2 land cleanly — the
socket exists, the agent is up, the protocol matches.

**Action**:

- Verify `fs.rs` and `proc.rs` succeed against a mock VM brought up
  with `bring_up_mock_vm`. If not, add minimal hypervisor selector
  flags matching `pause.rs`'s pattern from plan 65.

### W4 — Live drive-and-assert tests (~1 day)

**Goal**: 5+ live tests covering the `fs` and `proc` Emits rows.

**Action**:

- `tests/audit_emissions_live.rs`:
  ```rust
  #[test]
  fn fs_write_emits_vm_fs_mutate_audit_entry() {
      let sandbox = AuditSandbox::new();
      bring_up_mock_vm(&sandbox, "test-fs-vm");
      let output = sandbox.mvmctl()
          .args(["fs", "write", "test-fs-vm", "/tmp/hello", "--content", "hi"])
          .output().expect("spawn");
      assert!(output.status.success());
      let log = read_audit_log(&sandbox.audit_log_path());
      assert!(count_entries_with_kind(&log, "vm_fs_mutate") >= 1);
      assert!(log.contains("op=write path=/tmp/hello"));
  }
  ```
- Mirror for `fs mkdir`, `fs rm`, `fs chmod`.
- For `proc`: chain `start` → assert `VmProcStart`; `signal <token>`
  → assert `VmProcSignal`; `kill <token>` → assert `Kill`;
  `stdin <token>` → assert `VmProcStdin`.
- Two reads pinned as negatives: `proc ls` and `proc wait` →
  no LocalAudit emit.

**Exit criteria**:

- All 10+ new tests pass.
- `cargo test --workspace --no-fail-fast` clean.
- `cargo run -p xtask -- check-audit-positional` clean.

## Phasing

W1 → W2 → W3 → W4. W2 is the substrate; the rest depend on it. W2
can be implemented in parallel with W1 since the dir layout is
simple. Suggested PR boundary: W1+W2 in one PR, W3+W4 in a second.

## Non-goals (explicit)

- **Real vsock kernel module testing.** The plan covers the
  *host-side* end of the agent protocol via a Unix-socket mock;
  testing vsock-over-virtio at the kernel level is plan-25 W4
  territory (live KVM smoke).
- **AuthenticatedFrame signature verification.** The mock agent
  accepts any signature; signature-verification testing is
  separate.
- **Throughput / latency.** Mock latency is sub-millisecond by
  construction; performance testing of the real agent is
  separate.

## Success criteria

By plan 66 close:

1. `MockBackend` creates the per-VM directory layout +
   `MockGuestAgent` listens on `agent.sock`.
2. Every `fs` and `proc` Emits row has a live test in
   `tests/audit_emissions_live.rs`.
3. The `proc ls` and `proc wait` ReadOnly rows have live
   negative-pin tests.
4. `cargo test --workspace --no-fail-fast` clean; xtask lints clean.

## Risk notes

- **Protocol drift.** The mock has to track changes to
  `GuestRequest`/`GuestResponse`. Mitigation: `serde(deny_unknown_fields)`
  on every wire type (already in place per CLAUDE.md §security claim 5)
  means the mock breaks loudly when a new variant lands without a
  matching mock arm.
- **Concurrency.** Multiple tests may bring up multiple mock VMs in
  parallel. Each VM has its own `agent.sock` path; no global state
  collision.
- **macOS portability.** Unix-domain sockets work on macOS; should be
  no portability issue. Watch for path-length limits (sun_path is
  ~104 chars on macOS) — sandbox tempdir paths are typically short
  enough.
