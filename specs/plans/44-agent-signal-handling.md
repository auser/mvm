# Plan 44 — Guest agent signal handling

Status: **W1 + W2 shipped.** W3 (SIGHUP config reload) remains backlog —
not load-bearing until config reload becomes a real feature.

## Background

Until plan 44 W1 shipped, the guest agent had no signal handlers.
SIGTERM, SIGINT, SIGHUP all hit the default
disposition — the process died abruptly without giving any subsystem
a chance to drain or release resources. Today this is fine because:

- The dominant teardown path is **VM destruction**, not agent
  shutdown. When Firecracker is killed (host shutdown / container
  stop), KVM tears down the VM's memory — every process inside,
  including the agent, vanishes in the same instant. There's no
  signal to handle because there's no kernel left to deliver one.
- The agent has no hot-reload or in-place update flow. The
  unit-of-restart is the whole VM.
- Cleanup that *would* matter (warm-process worker drain, reaping
  children) is bounded by the VM's lifetime; the host-side stack
  doesn't observe in-VM zombies.

## What surfaced this gap

Plan 43 (warm-process function dispatch) added a worker pool that
spawns long-lived child processes inside the agent. The plan
called for a SIGTERM handler that would call
`WorkerPool::shutdown(Duration::from_secs(5))` to:

1. Stop accepting new vsock connections.
2. Wait for in-flight `RunEntrypoint` calls to drain (bounded by
   their per-call `timeout_secs`).
3. SIGTERM each worker, give a grace period, SIGKILL stragglers,
   `wait()` to reap.

`WorkerPool::shutdown()` was implemented (see
`crates/mvm-guest/src/worker_pool.rs`) but the handler that calls
it was deferred. Plan 43's "Known limitation" section names this
as v0.3 work.

## Why deferring is safe (today)

`WorkerHandle::Drop` already does SIGTERM/SIGKILL/reap when the
pool is dropped naturally — for example, in tests or if the pool
were ever replaced at runtime. It does NOT fire on
`std::process::exit` because Rust skips destructors on hard exit.

When the agent process dies on SIGTERM (default disposition):

- The kernel reparents worker children to PID 1.
- Workers receive SIGPIPE on their stdin pipe (the agent's FD
  table tearing down closes the parent end).
- A correctly-written wrapper exits in response and PID 1 reaps it.
- Inside the VM PID 1 is typically `init` / `systemd-style` /
  `mvm-verity-init`'s pivoted real init — which reaps zombies.

So there's no *permanent* leak. The cost is "no orderly drain" —
in-flight calls die mid-frame and surface to the host as
`EntrypointEvent::Error` or just connection-closed.

## When this plan should be picked up

Any of these flips the calculus:

1. **Hot-reload / in-place update** — if mvm grows a path that
   restarts the agent without rebooting the VM (e.g. agent-only
   security patches), graceful shutdown becomes load-bearing.
2. **Long-running calls** — if `timeout_secs` defaults grow past
   ~30 s, killing the agent during a call is increasingly user-visible.
   A drain window lets in-flight work finish.
3. **SIGHUP for config reload** — if `runtime.json` / `agent.json`
   become reloadable, the same signal-handler infrastructure
   serves both. Adding two handlers at once amortizes the design
   cost.
4. **Operator-driven shutdown** — if mvmd starts sending SIGTERM
   to the agent (rather than killing the VM) for orderly tenant
   migration, graceful shutdown is required.
5. **Containerized agent (no microVM)** — Apple Container's PID 1
   handling differs from a Firecracker microVM; if the agent
   starts running in non-VM contexts, the orphan-reparent
   semantics may not apply.

## Approach

Three workstreams. W1 is the substrate; W2 / W3 are the consumers
that justify shipping it.

### W1 — Signal-handler primitive

Decide on the handling strategy:

- **Option A: raw `libc::sigaction` + atomic flag.** ~30 lines, no
  new deps. Handler is `extern "C"` and async-signal-safe (only
  flips an `AtomicBool`). The accept loop polls the flag between
  iterations; `accept()` returns `EINTR` when a signal lands so
  the loop wakes up promptly. Matches the codebase's "no extra
  deps" preference.
- **Option B: `signal-hook` crate.** Higher-level, supports
  multiple signals cleanly. Adds a workspace dep.

Recommendation: A for v1. B if W3 (config reload) wants
multi-signal dispatch.

```rust
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_sigterm(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigterm as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}
```

The accept loop changes:

```rust
loop {
    if SHUTDOWN_REQUESTED.load(Ordering::Acquire) {
        break;
    }
    let cfd = unsafe { accept(fd, ...) };
    if cfd < 0 {
        // EINTR is expected when a signal lands; the next iteration
        // sees SHUTDOWN_REQUESTED and breaks.
        continue;
    }
    handle_client(cfd, ...);
}
// Post-loop: drain.
shutdown_subsystems(Duration::from_secs(5));
```

### W2 — Warm-process pool drain consumer

`shutdown_subsystems` calls
`WARM_POOL.get().and_then(|p| p.as_ref()).map(|p| p.shutdown(grace))`.

`WorkerPool::shutdown` already exists and does the right thing:

- Sets the `shutdown` atomic so new dispatches return
  `DispatchError::ShuttingDown`.
- Replaces idle slots with `Dead` (`Drop` SIGTERMs/SIGKILLs each
  worker).
- Sleeps `grace` to let busy slots finish.

Add: a `shutdown_busy` mode that also blocks on busy slots
finishing (currently relies on natural progression). The simpler
form for v1: skip this; busy workers see SIGPIPE when the agent
finally exits.

### W3 — SIGHUP config reload (optional)

If the same plan ships config reload, wire SIGHUP to a separate
`RELOAD_REQUESTED: AtomicBool`. The accept loop checks both flags;
on RELOAD, re-read `/etc/mvm/agent.json` and apply the
hot-reloadable subset. Don't reload `runtime.json` — it pins
worker-pool sizing decided at boot.

Pre-requisite: an `AgentConfig` reload-safety review. Many fields
(vsock port, integration drop-in dir) are not safely reloadable.
Document the reloadable subset before shipping the SIGHUP path.

## Tests

- Unit test the handler installation: install handlers,
  `kill(self_pid, SIGTERM)`, assert `SHUTDOWN_REQUESTED` flips.
- Integration test on `WorkerPool::shutdown`: pool with one busy
  + one idle worker; call `shutdown(1s)`; assert idle is killed
  immediately, busy completes, all workers reaped.
- Symbol-contract gate (`scripts/check-prod-agent-no-exec.sh`):
  add a positive-evidence assertion that
  `mvm_guest_agent::install_signal_handlers` is present, mirroring
  the W5/W7 pattern.

## Risks

- **Async-signal-safety violations.** Anything beyond
  `AtomicBool::store` and `write` to a self-pipe is unsafe in a
  signal handler. The handler must be trivial; all real work
  happens in the accept loop after the flag is observed.
- **EINTR-vs-no-EINTR loops.** Some libc calls retry internally;
  others don't. The accept loop must check the shutdown flag
  *outside* the syscall, not inside, so a missing EINTR doesn't
  wedge shutdown.
- **Signal mask inheritance.** The agent's own SIGTERM mask is
  inherited by spawned workers via `process_group(0)` + fork. If
  workers need to handle their own signals differently, they need
  to reset the mask in `pre_exec`. Most wrappers don't care, but
  document the inheritance.
- **Multiple signals delivered before drain finishes.** A second
  SIGTERM during drain should escalate to immediate exit, not
  retrigger the drain. Handler can count: first sets flag, second
  calls `_exit(128 + SIGTERM)`.

## Acceptance

By close of plan 44, mvm should be able to claim:

1. *The guest agent installs SIGTERM/SIGINT handlers at boot;
   on signal, new connections are refused, in-flight calls drain
   to a configurable timeout, the warm-process pool tears down
   workers cleanly, and the agent exits 0.*
2. *A second SIGTERM during drain escalates to immediate exit so
   a wedged drain doesn't strand operators.*
3. *The symbol-contract gate asserts the handler is wired into
   the production binary.*
4. *Plan 43's "Known limitation" can be removed.*

## Cross-references

- Plan 43 §"Known limitation" — names the deferral.
- `crates/mvm-guest/src/worker_pool.rs::shutdown` — the consumer
  this handler calls.
- `crates/mvm-guest/src/bin/mvm-guest-agent.rs` — install site
  (right after `init_warm_pool` in `main`).
