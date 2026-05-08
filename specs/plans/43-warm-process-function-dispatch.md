# Plan 43 — warm-process function dispatch (mvm side)

> Substrate work for the warm-process tier of mvmforge ADR-0011. Adds an
> opt-in worker pool inside the guest agent so function-entrypoint
> calls can reuse a long-running wrapper process instead of cold-spawning
> per `mvmctl invoke`. Sits on top of plan 41 (cold-tier
> `RunEntrypoint`) and is orthogonal to warm-VM session reuse.

## Context

Plan 41 / Sprint 45 shipped the **cold tier**: `RunEntrypoint` spawns the
validated wrapper at `/usr/lib/mvm/wrappers/<name>` per call, serialized
through `RUN_ENTRYPOINT_LOCK` (M12 — one in-flight call per VM). Per-call
process spawn pays the wrapper's interpreter cold-start (~hundreds of
milliseconds for Python) on every invoke.

mvmforge ADR-0011 specifies a three-tier concurrency hierarchy. Plan 41
gave us tier 1 (cold). Tier 2 (warm-process) keeps a pool of wrapper
processes alive across calls; tier 3 (warm-VM, `mvmctl session`) is
orthogonal and ships separately. This plan covers tier 2.

The host wire is unchanged — `mvmctl invoke` still sends
`GuestRequest::RunEntrypoint { stdin, timeout_secs }` and consumes the
same `EntrypointEvent` stream. Whether the substrate respawns or reuses
is a guest-side choice driven by `/etc/mvm/runtime.json`, the new
mvmforge-owned config file. Images without `runtime.json` (or without a
`concurrency` field) keep the cold path bit-for-bit.

## Trigger config

mvmforge writes `/etc/mvm/runtime.json` at image-build time via
`mkGuest extraFiles` (existing mechanism, no flake change here):

```json
{
  "language": "python",
  "module": "...", "function": "...", "format": "json",
  "source_path": "/app",
  "concurrency": {
    "kind": "warm_process",
    "max_calls_per_worker": 1000,
    "max_rss_mb": 512,
    "pool_size": 1,
    "in_process": "serial"
  }
}
```

`concurrency` absent → cold tier. `kind = "warm_process"` → this plan's
new path. `in_process = "concurrent"` is rejected at parse time (out of
scope for v0.2 per ADR-0011).

## Architectural decisions

These resolve the contracts ADR-0011 left open:

1. **Backpressure**: FIFO queue with bounded depth. Default
   `max_queue_depth = 2 * pool_size`, overridable via `runtime.json`.
   Overflow → `EntrypointEvent::Error { kind: Busy }` — same envelope
   the cold path returns on M12 contention. The cap is a defensive
   bound (a host caller that loops `mvmctl invoke` could OOM the agent
   without it; each queued caller holds its `stdin: Vec<u8>` in memory).
2. **Worker → agent framing**: one length-prefixed JSON frame per call
   (4-byte big-endian length + body, 256 KiB cap,
   `serde(deny_unknown_fields)`). Buffered, not streamed sub-frames —
   matches the current cold-path emission shape (one `Stdout`, one
   `Stderr`, one terminal). The agent synthesizes `EntrypointEvent`s
   from the worker's single response so the host wire stays identical.
3. **Binary encoding**: base64 over JSON for stdin/stdout/stderr. Single
   serializer (`serde_json`) across the agent. Bincode is a v0.3 perf knob.
4. **Malformed runtime.json**: fail loud — agent exits non-zero at boot.
   mvmforge owns the file; a broken one is a build bug, not a runtime
   fallback. Missing file → cold tier (no error).
5. **`in_process = "concurrent"`**: rejected at parse time.
6. **M12 invariant under warm-process**: bypassed. New invariant: "one
   in-flight call per worker, up to `pool_size` concurrent." Cold path
   keeps the existing global mutex untouched.
7. **No `dev-shell` feature gate**. Warm-process is production code.
   The CI gate (`scripts/check-prod-agent-no-exec.sh`) only forbids
   `mvm_guest_agent::do_exec`; new `mvm_guest::worker_pool::*` symbols
   pass.
8. **Recycle-after-call only**: RSS sampled from `/proc/<pid>/statm`
   after the worker returns its response, never mid-dispatch.

## Boundaries

- **Two config files, two owners.** `/etc/mvm/agent.json` (mvm-owned,
  transport tuning, exists today) vs `/etc/mvm/runtime.json`
  (mvmforge-owned, per-workload metadata, NEW). Different schemas,
  different release cadences, kept separate.
- **One mode per image in v0.2.** No mixed cold+warm in the same image.
  "Warm with cold overflow" is a v0.3 idea.
- **Workers don't cross VM boundaries.** Each VM has its own pool;
  cross-VM call routing is mvmd's concern.
- **Sync, not async.** Tokio adds binary-size + audit surface to a
  uid-901 production binary; bounded concurrency (`pool_size ≤ 64`)
  doesn't motivate it. Thread-per-connection is the right shape.

## Approach

Five phases, each independently committable.

### Phase 1 — Types and config surface

`crates/mvm-guest/src/runtime_config.rs` (new):

```rust
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub language: String,
    pub module: String,
    pub function: String,
    pub format: String,
    pub source_path: String,
    #[serde(default)]
    pub concurrency: Option<ConcurrencyConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum ConcurrencyConfig {
    WarmProcess(WarmProcessConfig),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WarmProcessConfig {
    pub max_calls_per_worker: u64,
    pub max_rss_mb: u64,
    pub pool_size: usize,
    pub in_process: InProcessMode,
    /// Default = 2 * pool_size. Overflow → Busy.
    #[serde(default)]
    pub max_queue_depth: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InProcessMode { Serial, Concurrent }
```

`pub fn load() -> Result<Option<RuntimeConfig>, RuntimeConfigError>`:
reads `/etc/mvm/runtime.json`. Missing → `Ok(None)`. Malformed → `Err`.
Validates `pool_size` in `[1, 64]` and rejects `Concurrent`. Path is
overridable for tests via `load_from(&Path)`.

`crates/mvm-guest/src/worker_protocol.rs` (new):

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCallRequest {
    #[serde(with = "base64_bytes")]
    pub stdin: Vec<u8>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCallResponse {
    #[serde(with = "base64_bytes")]
    pub stdout: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub stderr: Vec<u8>,
    pub outcome: WorkerOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum WorkerOutcome {
    Exit { code: i32 },
    Error { kind: String, message: String },
}

pub fn write_pipe_frame<W: Write, T: Serialize>(...) -> io::Result<()>;
pub fn read_pipe_frame<R: Read, T: DeserializeOwned>(...) -> io::Result<T>;
```

Helpers mirror `vsock.rs:read_frame`/`write_frame` but generic over
`Read`/`Write` (pipe handles, not `UnixStream`). `MAX_PIPE_FRAME_SIZE =
256 KiB`. Types are `pub` so mvmforge can depend on `mvm-guest` for the
wrapper-side schema (single source of truth).

Acceptance: serde roundtrips for every type; deny-unknown rejection
tests; fuzz cases for truncated / oversized / partial frames.

### Phase 2 — Worker pool module

`crates/mvm-guest/src/worker_pool.rs` (new):

```rust
struct WorkerHandle {
    pid: u32,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_drain: JoinHandle<Vec<u8>>,
    call_count: u64,
    last_rss_bytes: u64,
}

enum WorkerSlot { Idle(WorkerHandle), Busy, Replacing, Dead }

pub struct WorkerPool {
    slots: Mutex<Vec<WorkerSlot>>,
    cv: Condvar,
    pending_waiters: AtomicUsize,
    cfg: WarmProcessConfig,
    entrypoint: Arc<ValidatedEntrypoint>,
    shutdown: AtomicBool,
}

impl WorkerPool {
    pub fn start(cfg: WarmProcessConfig, entry: Arc<ValidatedEntrypoint>) -> io::Result<Arc<Self>>;
    pub fn dispatch(&self, stdin: Vec<u8>, timeout: Duration) -> DispatchOutcome;
    pub fn shutdown(&self, grace: Duration);
}
```

Worker spawn reuses the validated FD (`/proc/self/fd/<n>` from
`VALIDATED_ENTRYPOINT`). Same security envelope as plan 41's
`entrypoint::execute`: `env_clear()`, `process_group(0)`, `pre_exec` to
zero `RLIMIT_CORE`, stdio piped. The agent's setpriv envelope (W4.5)
propagates to children.

Dispatch flow:
1. `acquire_idle_or_wait()` — if `pending_waiters >= max_queue_depth`,
   return `QueueFull`; otherwise find first `Idle`, swap to `Busy`. If
   none free, increment waiters, `cv.wait`, decrement on wake.
2. Spawn watchdog thread that SIGKILLs the worker process group on
   `timeout_secs` expiry.
3. `write_pipe_frame(WorkerCallRequest)`.
4. `read_pipe_frame::<WorkerCallResponse>`. EOF / parse error →
   `WrapperCrashed`.
5. Cancel watchdog. Bump `call_count`. Sample RSS.
6. Recycle if `call_count >= max_calls_per_worker`, RSS over cap, or the
   call errored — drain (no-op since serial), SIGTERM/grace/SIGKILL,
   reap, spawn replacement. Otherwise return slot to `Idle` and
   `cv.notify_one()`.

Replacement failure marks the slot `Dead`; a 1s recovery thread
periodically retries `Dead → Idle`.

Routing back to `EntrypointEvent` (caller side):
- `Outcome::Exit { code }` → `Stdout(stdout)`, `Stderr(stderr)`,
  `Exit { code }`.
- `Outcome::Error { kind: "wrapper_crash", .. }` → `Error { kind:
  WrapperCrashed, .. }`.
- `kind == "timeout"` → `Error { kind: Timeout, .. }`.
- Other → `Error { kind: InternalError, .. }`.

Acceptance: pool spawn unit tests against `fake_runner`; recycle on
call-count and RSS; FIFO queue under saturation; PID stability across
calls without recycle.

### Phase 3 — Wire the agent

`crates/mvm-guest/src/bin/mvm-guest-agent.rs`:

1. **Boot init** after `init_entrypoint_validation()`:
   ```rust
   let warm_pool = match runtime_config::load() {
       Ok(None) => None,
       Ok(Some(rc)) => match rc.concurrency {
           Some(ConcurrencyConfig::WarmProcess(wp)) => match VALIDATED_ENTRYPOINT.get() {
               Some(Ok(entry)) => Some(WorkerPool::start(wp, Arc::new(entry.clone()))?),
               _ => { eprintln!("warm-process configured but entrypoint validation failed"); std::process::exit(1); }
           },
           None => None,
       },
       Err(e) => { eprintln!("invalid /etc/mvm/runtime.json: {e}"); std::process::exit(1); }
   };
   static WARM_POOL: OnceLock<Option<Arc<WorkerPool>>> = OnceLock::new();
   let _ = WARM_POOL.set(warm_pool);
   ```

2. **Branch in `handle_run_entrypoint`** before the M12 try_lock:
   ```rust
   if let Some(Some(pool)) = WARM_POOL.get() {
       return dispatch_via_warm_pool(file, pool, stdin, timeout_secs);
   }
   // existing cold path unchanged
   ```
   Preserve `#[inline(never)]` on `handle_run_entrypoint` (load-bearing
   for the W5 symbol gate).

3. **Accept loop**: when warm-process is active, spawn-thread-per-conn
   so concurrent invokes reach parallel workers. Cold path stays
   single-threaded — no behavior change for cold-tier images.

4. **Shutdown handler**: install a SIGTERM handler that calls
   `WorkerPool::shutdown(5s)`. Sets the `shutdown` atomic, drains
   in-flight calls, SIGTERM/grace/SIGKILL idle workers.

Acceptance: cold-tier regression — existing `runentrypoint` tests pass
with no `runtime.json`. Warm-tier integration test drives
`handle_run_entrypoint` end-to-end against `fake_runner`.

### Phase 4 — Tests

**Fixture** `crates/mvm-guest/tests/bin/fake_runner.rs`: Rust binary
that reads frames via `worker_protocol::read_pipe_frame`. Behavior via
env: `FAKE_RUNNER_BEHAVIOR=ok|crash|leak_50mib|exit_after_n=N|malformed_frame|sleep_forever`.
Default: echo stdin → stdout, fixed stderr, `Exit { code: 0 }`.

**Unit tests** in `worker_pool.rs::tests`:
- `dispatch_one_call_pid_stable` — case 1
- `recycle_on_call_count_exceeded` — case 2
- `recycle_on_rss_exceeded` (injectable `RssReader` trait) — case 3
- `wrapper_crash_returns_error_and_recycles` — case 4
- `user_code_fault_does_not_recycle` — case 5
- `pool_4_parallel_dispatches` — case 7
- `fifo_queue_under_saturation` — case 8
- `concurrent_mode_rejected_at_load` — case 9 (partial)

**Frame fuzz** in `worker_protocol.rs::tests`:
- truncated length, oversized length, partial payload — case 6.

**Integration** `crates/mvm-guest/tests/runentrypoint_warm.rs`:
- writes `runtime.json` to a tempdir
- builds an `EntrypointPolicy` pointing at `fake_runner`
- exercises `handle_run_entrypoint` end-to-end via mock vsock stream
- adds `no_runtime_json_uses_cold_path` regression — case 9

**Symbol-contract gate** `scripts/check-prod-agent-no-exec.sh`: extend
with positive assertion for `mvm_guest::worker_pool` presence as
evidence the warm path is wired.

### Phase 5 — Coordination signal to mvmforge

This plan is mvm-side only. After it lands, mvmforge owes:

- Plumb `concurrency` through their IR.
- Each language factory writes `runtime.json` with the new shape into
  `mkGuest extraFiles`.
- The runner wrapper speaks the framed multi-call loop on stdin/stdout
  (4-byte BE + JSON `WorkerCallRequest`/`WorkerCallResponse`).
- Wire-format alignment via `mvm-guest::worker_protocol` (re-exported
  `pub` so mvmforge depends on mvm-guest for the schema).

Until mvmforge ships its side, no image carries `concurrency.kind =
"warm_process"`, so the warm path is dormant in the wild and the cold
path is unaffected. We can land this substrate safely and test it via
`fake_runner` without mvmforge ready.

## Surface area

| Surface | Change | Containment |
|---|---|---|
| `runtime.json` parser | New file read at boot | `deny_unknown_fields`, `pool_size ≤ 64`, fail-loud on malformed; rootfs verity-protected (W3) |
| Wrapper-side stdio pipes | Agent reads JSON frames from a child | 256 KiB frame cap; parse error → recycle; watchdog SIGKILLs on `timeout_secs` |
| Worker spawn | Child from validated FD | Same `EntrypointPolicy::production()` validation as cold path; FD-held-open defeats TOCTOU |
| Accept-loop concurrency | Thread-per-connection when warm | Existing shared state is `Arc<Mutex<>>`; explicit `handle_client` audit pass |
| M12 lock bypass | Per-slot Busy replaces global mutex | Cold path keeps M12; new invariant documented and tested |

**Specifically widened vs cold path**:
- In-flight `RunEntrypoint` per VM: 1 → up to `pool_size` (cap 64), with
  `max_queue_depth` (default `2 * pool_size`).
- Long-running wrapper persists across calls. Cross-call state inside
  the wrapper is the user's responsibility per ADR-0011 — log loudly at
  agent boot.
- Wrapper crash mid-call affects only that call; cold path: process
  exits per call regardless.

**Specifically NOT widened (verified surface-stable)**:
- Host-side `mvmctl invoke` argv, stdin/stdout, exit codes.
- vsock wire types (`GuestRequest`, `GuestResponse`, `EntrypointEvent`).
- Cold-path behavior when no `runtime.json` is present.
- Session-mode dispatch (`mvmctl session`, orthogonal).
- CLI audit-emit gate (CLI-side, untouched).

**Audit pass during implementation**:
1. Walk every `handle_client` match arm for non-mutex-protected shared
   state (sleep_prep, drop_caches, port-forward all already safe;
   paranoia pass).
2. Run `scripts/check-prod-agent-no-exec.sh` before and after — confirm
   no new forbidden symbols and the new positive-evidence assertion
   passes.
3. Frame fuzz on `read_pipe_frame`: truncated, oversized, partial-read,
   garbage payload.
4. 10K-recycle stress test, watch `/proc/<agent>/fd/` for leaks.

**Known limitations**:

- Warm-process opt-in is decided at agent boot. There's no host-side
  runtime kill-switch to force a warm image back to cold tier; rolling
  back requires rebuilding. A defense-in-depth runtime override
  (kernel cmdline or `agent.json` toggle that suppresses warm even
  when configured) is out of scope for v0.2 — file as v0.3.
- ~~No SIGTERM handler.~~ **Resolved by [plan 44](44-agent-signal-handling.md)
  W1 + W2** — SIGTERM/SIGINT flip an atomic flag, the accept loop polls
  it, and `WorkerPool::shutdown` drains workers before exit. Cold-tier
  images get the same orderly accept-loop exit (the drain is a no-op
  when no pool is active). SIGHUP config reload (plan 44 W3) remains
  backlog until config reload is wanted.

## Files to touch

**New**:
- `crates/mvm-guest/src/runtime_config.rs`
- `crates/mvm-guest/src/worker_protocol.rs`
- `crates/mvm-guest/src/worker_pool.rs`
- `crates/mvm-guest/tests/bin/fake_runner.rs`
- `crates/mvm-guest/tests/runentrypoint_warm.rs`

**Modified**:
- `crates/mvm-guest/src/lib.rs` — re-export new modules
- `crates/mvm-guest/src/bin/mvm-guest-agent.rs` — boot init,
  spawn-per-conn (warm only), `handle_run_entrypoint` branch
- `crates/mvm-guest/src/entrypoint.rs` — export `spawn_path` and kill
  helpers as `pub(crate)`
- `crates/mvm-guest/Cargo.toml` — add `base64` dep; declare
  `fake_runner` test binary
- `scripts/check-prod-agent-no-exec.sh` — positive symbol assertion

**Untouched**:
- `crates/mvm-cli/src/commands/vm/invoke.rs` — host CLI unchanged
- `crates/mvm-cli/src/commands/vm/exec.rs` — session-mode is orthogonal
- `crates/mvm-runtime/src/vsock_transport.rs` — host transport unchanged
- `nix/flake.nix` `mkGuest` — `runtime.json` ships via existing
  `extraFiles` mechanism

## Risks

1. **Accept-loop concurrency** introduces real parallelism for
   warm-tier images. Audit `handle_client` match arms (existing
   `Arc<Mutex<>>` covers integrations / probes / port forwards).
2. **Pipe writes blocking forever** if a worker stops reading stdin.
   Watchdog SIGKILL closes the pipe and unblocks the agent.
3. **FD leaks** on respawn churn. `Child::Drop` handles stdlib pipes;
   verify with a 10K-recycle stress test.
4. **`prod-agent-no-exec` CI gate** must keep passing. Run the gate
   locally before opening the PR.

## Verification

```bash
cargo test -p mvm-guest worker_pool::tests
cargo test -p mvm-guest worker_protocol::tests
cargo test -p mvm-guest --test runentrypoint_warm

cargo test --workspace
cargo clippy --workspace -- -D warnings
bash scripts/check-prod-agent-no-exec.sh
```

Live smoke (requires a warm-process image from mvmforge):

```bash
mvmctl up warm-py-fn
for i in {1..100}; do echo '{"x":1}' | mvmctl invoke warm-py-fn; done
# Expect: ~10 worker recycles in agent stderr with max_calls_per_worker=10.
# Expect: same N PIDs reused between recycles.

# M12 cold-path regression (no runtime.json):
mvmctl up cold-py-fn
( mvmctl invoke cold-py-fn < input1 & mvmctl invoke cold-py-fn < input2 & wait )
# Second invoke MUST see EntrypointEvent::Error { kind: Busy }.

# Crash recovery: kill -9 a worker mid-call from another shell.
# Expect: EntrypointEvent::Error { kind: WrapperCrashed }; next invoke succeeds.
```

## Acceptance

By close of plan 43, mvm should be able to claim:

1. *A `runtime.json`-driven worker pool runs warm-tier function calls
   from a fixed set of long-lived wrapper processes; cold tier stays
   bit-identical when `runtime.json` is absent.* (Phases 1–3)
2. *Worker recycling fires on `max_calls_per_worker`, `max_rss_mb`, and
   wrapper crash; user-code faults do not recycle.* (Phase 2)
3. *`prod-agent-no-exec` gate keeps passing; new
   `worker_pool::dispatch` symbol is required as positive evidence the
   warm path is wired.* (Phase 4)
4. *Wire-format types are `pub` for mvmforge to depend on, so the
   wrapper-side runner stays in lockstep with the agent without
   schema duplication.* (Phase 1)

mvmforge IR + factory cutover lands in a coordinated PR after; the
warm path is dormant until then.
