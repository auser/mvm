# Plan 89 — Persistent builder VM via vsock dispatch

**Status:** drafted 2026-05-19, awaiting review.
**Pairs with:** new ADR (TBD — see §Pairing ADR) describing the trust
boundary change from one-job-per-VM to many-jobs-per-VM.

## Problem

Every `BuilderJob` spins a fresh libkrun builder VM. Looking at the
per-phase telemetry written by `mvm-builder-init`
(`crates/mvm-builder-init/src/boot_timings.rs:41`), every job pays
for:

- `pseudofs_ready_ms` — `/proc`, `/sys`, `/dev`, `/tmp`.
- `nix_device_ready_ms` — `/dev/vdb` mount.
- `nix_mounted_ms` — overlay mount over `/nix`.
- `modules_ready_ms` — `fuse` + `virtiofs` kmod load.
- `virtiofs_ready_ms` — `job`, `out`, `work` virtio-fs shares.
- `network_ready_ms` — DHCP via passt (Plan 87) or vmnet.

These are constant per-boot overhead. They don't shrink with a warm
`/nix` store (Plan 72), an `auto-optimise-store` hardlinked store
(PR #367 / cmd.sh `NIX_CONFIG`), or a persisted Nix flake eval-cache
(PR #368 / cmd.sh `XDG_CACHE_HOME`). For users running `mvmctl build`
iteratively — edit one line, rebuild, repeat — the boot dance
dominates the wall-clock of small-incremental rebuilds.

`nix_seeded_ms` is excluded from this list: it's a first-boot-per-host
cost amortized across the lifetime of the persistent
`~/.cache/mvm/builder-vm/nix-store-<arch>.img`.

## Goal

Boot one libkrun builder VM per `mvmctl dev` session and dispatch
successive `BuilderJob`s into it over a vsock control channel,
replacing the boot-per-job dance with a boot-once-per-session dance.
The single-shot `LibkrunBuilderVm::run_build` path remains as the
fallback for one-off builds (CI, `mvmctl build` outside a dev session,
template builds — see §Non-goals), so the existing trust boundary is
preserved for callers that opt out.

After this plan: from inside an active `mvmctl dev` session, running
`mvmctl build` a second time skips pseudofs/vdb/overlay/kmod/virtiofs/
DHCP entirely and dispatches the new job into the warm VM over vsock.

## Design

### Components

```
+-----------------------------------------------------+
| Host                                                |
|                                                     |
|  mvmctl dev up                                      |
|    └─ spawn PersistentBuilderSupervisor (host proc) |
|         │                                           |
|         ├─ start_enter ── libkrun VM (long-lived)   |
|         │                  │                        |
|         │                  ├─ mvm-builder-init      |
|         │                  │   (Track A/B/C boot)   |
|         │                  │                        |
|         │                  ├─ vsock dispatch loop   |
|         │                  │   on BUILDER_PORT      |
|         │                  │                        |
|         │                  ├─ /work virtio-fs        |
|         │                  │   = host repo root      |
|         │                  └─ per-job /tmp/<jid>/    |
|         │                                           |
|         ├─ vsock client: idle timer + watchdog      |
|         └─ AF_UNIX dispatch socket for mvmctl build |
|                                                     |
|  mvmctl build (separate process)                    |
|    └─ if supervisor socket present:                 |
|         submit BuilderRequest via AF_UNIX → vsock   |
|       else:                                         |
|         LibkrunBuilderVm::run_build (single-shot)   |
+-----------------------------------------------------+
```

The supervisor lives in the same long-running process that
`mvmctl dev up` already spawns for the dev shell. It owns the libkrun
VM via the existing `mvm-libkrun-supervisor` machinery (Plan 87 W2),
the dispatch socket, the per-job book-keeping, and the idle timer.

### Workstreams

**W1 — Telemetry baseline (~½ day).**

Measure cold boot timings on macOS Apple Silicon and Linux KVM for
(a) an empty `cmd.sh` job and (b) a typical small incremental flake
rebuild. Use the existing `BootTimings` JSON the init binary writes.
No code changes — just a one-off measurement run with results checked
in to `specs/notes/plan-89-baseline.md`. If the boot fan-out is under
~500 ms on both platforms, this plan is not worth shipping and we
stop here.

**W2 — Vsock dispatch wire (~1 day).**

- New types in `mvm-build::builder_protocol`:
  ```
  enum BuilderRequest {
      Run { job_id: Uuid, job: BuilderJob, job_dir_relpath: String },
      Shutdown,
  }
  enum BuilderResponse {
      Result { job_id: Uuid, exit_code: i32, stderr_tail: String,
               boot_timings: Option<BootTimings>,
               job_timings: JobTimings },
      Bye,
  }
  struct JobTimings { dispatch_ms: u64, build_ms: u64, teardown_ms: u64 }
  ```
- `#[serde(deny_unknown_fields)]` on every variant.
- Wrap in `AuthenticatedFrame` (the host↔guest framing fuzzed by
  Plan 73 W4.2) — the persistent VM's dispatch is a host↔guest channel
  and inherits the same threat model as every other vsock RPC.
- New `cargo-fuzz` target `mvm-guest/fuzz/fuzz_targets/builder_request.rs`
  covering both variants.
- Wire the new protocol into the existing single-shot path first so
  the cold-boot VM exits via the new code path before persistent mode
  exists. This is "land the wire change without changing behavior" —
  catches any serde / framing regressions in isolation.

**W3 — Persistent VM lifecycle (~2 days).**

- New `mvm-build::libkrun_builder::LibkrunPersistentBuilderVm` —
  reuses the `LibkrunBuilderVm` config fields plus a workspace_root
  binding.
- New `mvm-builder-init` boot path: if `/job` contains
  `dispatch.sock.marker` instead of `cmd.sh` / `install_spec.json`,
  enter the dispatch loop instead of one-shot execution. The marker
  is staged by the host; absence preserves the existing flake/install
  paths exactly.
- Inside the VM, dispatch loop:
  1. Accept on vsock `BUILDER_DISPATCH_PORT` (new — added to the
     Plan 73 allowlist).
  2. Read one `AuthenticatedFrame<BuilderRequest>`.
  3. For `Run`: create `/tmp/<job_id>/` (per-job scratch — see §Why
     per-job /tmp below), stage the job dir contents from the
     dispatched `job_dir_relpath` in the workspace mount, exec the
     same flake-or-install dispatch logic that's in `linux::run` today.
  4. Stream stderr lines to host via the same vsock conn as they
     arrive (don't buffer — long builds need live logs).
  5. Send `BuilderResponse::Result`, drop scratch, loop.
- Host-side `PersistentBuilderSupervisor::submit(BuilderJob) ->
  Receiver<BuildEvent>`:
  - Serializes the job to a `BuilderRequest`.
  - Acquires the dispatch mutex (V1 = serialized; see §Concurrency).
  - Forwards stderr events to the caller's receiver as they arrive.
  - Joins on `BuilderResponse::Result`, releases the mutex.
- `mvmctl dev up` auto-starts the supervisor (per open-question 1
  resolved by Ari — automatic, not opt-in).
- `mvmctl dev down` sends `BuilderRequest::Shutdown`, the guest
  exits the dispatch loop, the VM powers off cleanly.

**W4 — `mvmctl build` integration (~1 day).**

- `mvmctl build` looks for the supervisor's AF_UNIX socket at
  `~/.mvm/run/builder-dispatch.sock` (created by the supervisor on
  startup, mode 0600, owner-only — matches `~/.mvm` hardening from
  ADR-002 W1.5).
- Socket present + reachable → submit job via supervisor.
- Socket absent or unreachable → fall back to
  `LibkrunBuilderVm::run_build`.
- The fallback path is silent and automatic. A `--no-reuse` flag on
  `mvmctl build` forces single-shot for cases where the user wants
  isolation (e.g. debugging a flaky build, or running against a
  workspace outside the dev session's root — see §Workspace binding).

**W5 — Idle timeout, resource release, recovery (~1 day).**

- Idle timer: 30 minutes (configurable via
  `MVM_BUILDER_IDLE_TIMEOUT_SECS`, see §Open questions resolution).
  On expiry the supervisor sends `Shutdown` and tears down the VM.
  Next `mvmctl build` triggers a cold restart of the supervisor's VM
  (boot fan-out paid again).
- Healthcheck: supervisor polls the vsock conn every 30 s with a
  zero-cost `Run { job: Probe }` variant (TODO: decide whether to add
  a `Probe` variant or just open a fresh conn). On disconnect ↔ VM
  crashed: log the failure, mark the supervisor unhealthy, route
  subsequent submits to the single-shot fallback path until the next
  `mvmctl dev down/up` cycle. Re-spawning the persistent VM
  automatically on crash is V2+ work.
- Resource pinning: persistent VM holds the libkrun defaults
  (4 vCPUs, 8 GiB RAM from `LibkrunBuilderVm::DEFAULT_*`). The 30 min
  idle release puts a ceiling on how long this allocation persists
  past the last user action.

### Job dispatch protocol — wire details

`BUILDER_DISPATCH_PORT` is a new vsock CID-port pair. Plan 73's
allowlist (`crates/mvm-builder-init/src/network.rs`'s vsock allow
list, and the host-side proxy's symmetric allow list in
`crates/mvm/src/vm/...`) needs the new port added.

Frame layout follows the existing `AuthenticatedFrame` design:
sequence number + monotonic counter + HMAC-tagged payload. The
persistent VM's session key is derived once at boot from the
host-supplied ephemeral key in the same fashion as every other vsock
session — no new key material surface.

Stderr streaming: each line is a `BuilderResponse::StderrChunk { job_id,
line }` (small variant added to the enum). The host plumbs these into
its existing `vm_state_dir/console.log` so debugging stays uniform.

### Workspace mount strategy

Open-question 4 resolution: bind the persistent VM's `/work`
virtio-fs share to the **git toplevel of the directory where `mvmctl
dev up` was invoked**, falling back to `$PWD` if not in a git
repository. This is the "mount one tree" option from the design
draft — covers the 90% case (in-repo flake builds, `mvmctl template
build`, addon builds), and cleanly fails-soft for the 10% cross-repo
case by routing to the single-shot fallback.

`mvmctl build` checks at submission time whether the target flake
path lies under the supervisor's bound root. If yes, dispatch via the
supervisor. If no, log "workspace outside dev session root, falling
back to single-shot" and route to `LibkrunBuilderVm::run_build`.
This decision happens host-side; the supervisor never sees
out-of-tree dispatches.

The libkrun gotcha (`start_enter` calls `exit()`, virtio-fs mounts
are fixed at boot) is the load-bearing constraint behind this design
choice. We can't hot-add a virtio-fs share for a new workspace; the
session's root is the session's root.

### Why per-job `/tmp`

Today's one-shot VM has a fresh tmpfs `/tmp` because the VM itself is
fresh. With reuse, two consecutive jobs share `/tmp` unless we
explicitly isolate. `mvm-builder-init`'s dispatch loop creates
`/tmp/<job_id>/` per job and exports `TMPDIR=/tmp/<job_id>`,
`HOME=/tmp/<job_id>`, `XDG_STATE_HOME=/tmp/<job_id>/.local/state`
into the build subprocess's env. `XDG_CACHE_HOME` stays on
`/nix-store/.cache` (PR #368) because cross-job cache reuse is the
whole point of that path.

On job completion the dispatch loop `rm -rf /tmp/<job_id>/`. If the
job has a hung subprocess (orphan), the parent dispatch loop is
responsible for SIGKILL'ing its process group before cleanup — same
pattern as the existing `run_job` body, just per-iteration.

### Lifecycle binding

Open-question 1 resolved: auto-start the persistent supervisor from
`mvmctl dev up`. Users who don't want it can pass
`--no-persistent-builder` (or set `MVM_PERSISTENT_BUILDER=0`).

Open-question 2 resolved: 30-minute idle timeout. Reasoning: short
enough that an overnight idle dev session doesn't pin 8 GiB RAM for
12 hours, long enough that the typical "think about the problem,
read docs for 20 minutes, run another build" cycle never trips the
cold restart. Configurable via env var for the dev who knows their
workflow.

### Concurrency

V1: serialize via the host-side dispatch mutex. One job in flight at
a time per supervisor. Queued submits block in the caller until the
mutex releases.

V2+ (deferred — explicit non-goal for this plan): parallel job
execution would require per-job overlay namespaces (so two builds
don't fight over `/nix-store/upper`), per-job dispatch sockets, and a
careful think about cache-coherence on the shared `XDG_CACHE_HOME`.
Defer until someone has a concrete use case; serialized V1 already
captures the boot-amortization win.

### Migration path

`mvm-build::libkrun_builder::LibkrunBuilderVm::run_build` stays
exactly as it is. `LibkrunPersistentBuilderVm` is additive. The choice
between persistent and one-shot lives one layer up
(`mvm-cli::commands::build`).

`mvmctl template build` stays one-shot (see §Non-goals). `mvmctl
build` from inside a dev session uses persistent. `mvmctl build` from
outside uses one-shot.

Every existing call site of `LibkrunBuilderVm::run_build` is preserved
verbatim — the persistent path is gated by the CLI command + the
supervisor's presence, not by any change to the builder VM API.

### Why now

The two parallelism / cache wins that just landed (PR #367
`auto-optimise-store`, PR #368 eval-cache persist) reduced the
per-job *build* time on incremental rebuilds. The leftover wall-clock
is dominated by boot fan-out — a fixed-cost overhead that doesn't
shrink with smarter Nix configuration. The user-visible effect is a
"weird latency floor" on small builds: change one line, rebuild,
still wait 4-5 seconds before the build itself starts. Eliminating
the boot dance is the next obvious lever.

### Pairing ADR

This plan changes the trust posture from "every job runs in its own
single-occupancy VM, torn down on exit" to "jobs share a long-lived
VM, with per-job scratch isolation". That's a real change in the
threat model — worth its own ADR, paired with this plan and merged
together. The ADR should cover:

1. What the persistent VM means for ADR-002's claim 1 (no host-fs
   access beyond explicit shares). Claim is preserved — the workspace
   mount is the same virtio-fs share, just bound earlier.
2. Cross-job state isolation: what's per-job (`/tmp/<job_id>/`) and
   what's shared (`/nix-store/`, eval-cache). Document that a
   compromised build can affect subsequent builds in the same
   session via the shared cache, and that the existing
   `nix-store --verify` and `auto-optimise-store` hardlink semantics
   make this no worse than two consecutive cold-boot builds against
   the same persistent `/nix-store/`.
3. Vsock dispatch surface: same framing as every other host↔guest
   RPC, no new key material, fuzzed by Plan 73 patterns.
4. Failure mode: VM crash demotes the supervisor; queue drains via
   single-shot. Persistent VM does not auto-restart in V1.

ADR number to be assigned at filing time (memory note about
numbering collisions applies).

## Non-goals

- **Parallel jobs inside one persistent VM.** Serialized V1 only.
- **A detached daemon outside any `mvmctl` invocation.** Lifecycle
  is bounded by `mvmctl dev`. No `systemctl --user` story.
- **Host-side Nix evaluation.** CLAUDE.md is explicit; this plan
  honors that. Every Nix evaluation still goes through a VM we
  launched.
- **Persistent VM for `mvmctl template build`** (open-question 3
  resolution): template builds stay one-shot. Reasoning: templates
  are explicitly built-for-sharing artifacts; the contamination risk
  from sharing a builder VM across builds — however small — is the
  wrong tradeoff for an artifact that may be redistributed.
  Templates also tend to be one-off invocations rather than tight
  iterative loops, so the amortization win is smaller. The single-
  shot path is already correct here; leave it alone.
- **Persistent VM for the app-deps install pipeline** (Plan 73). The
  sealed-volume invariants there assume single-occupancy. Reuse
  could land in a future plan if there's demand, but it's not part
  of this one.
- **Restart on crash in V1.** V1 demotes to single-shot fallback
  on persistent VM failure; auto-restart with re-queue is V2+.

## Risks

1. **Workspace mount drift.** The persistent VM's `/work` is a live
   virtio-fs share of the host workspace. `git checkout` of a
   different branch mid-session changes the file tree under the
   share. Nix's eval-cache (PR #368) is fingerprinted by store-input
   hash, not tree mtime — so a branch switch invalidates cached
   results naturally. Verify with an integration test:
   `mvmctl build` → `git checkout other-branch` →
   `mvmctl build` produces a different output (no false hits).

2. **Resource pinning.** 8 GiB RAM and 4 vCPUs reserved for the
   dev-session lifetime. On a 16 GiB host that's half of RAM gone.
   Mitigation: 30 min idle release; `mvmctl dev status` reports
   current builder VM state and resource hold; `mvmctl dev down`
   releases immediately. The single-shot fallback is silent so
   demoting the supervisor (manually or via crash) costs only the
   boot fan-out per subsequent build, not a hard error.

3. **Passt instance lifetime (Plan 87 interaction)** (open-question
   4 resolution): passt is now the default networking (PR #360 / Plan
   87 PR3). A long-lived builder VM means a long-lived passt
   instance bound to the host's network state at dev-up time. If the
   host's network reconfigures (WiFi network change, VPN connect/
   disconnect, dock/undock with different DHCP) mid-session, passt's
   view of the host network may go stale. Recommendation:
   `PasstSupervisor` (Plan 87 W2) gains a `restart_for_network_change()`
   trigger that the persistent builder VM calls when the host
   reports a network change (mac OS: `SystemConfiguration` notifier;
   Linux: `NetworkManager` D-Bus). Until that's wired, document the
   workaround: `mvmctl dev down && mvmctl dev up` after a major
   host network change. Add a smoke test for the persistent VM
   surviving a manually-triggered passt restart.

4. **Boot-report telemetry semantics change.** `mvmctl boot-report`
   (Plan 76 Phase 4) reads from per-job result dirs and assumes
   every dispatched job has a fresh `BootTimings`. After this plan,
   only the supervisor's first dispatch reports cold timings;
   subsequent jobs report `boot_timings: None` and a populated
   `job_timings` block instead. The report tool needs an update to
   distinguish the two cases — drawn explicitly so the contributor
   knows whether the cold-boot was paid or not.

5. **Shared eval-cache contention under V2+ parallelism.** Not a V1
   risk (V1 is serialized), but called out because PR #368 already
   has this same shared-disk concurrency posture for two
   simultaneous one-shot VMs. Squaring V2+ parallelism with the
   eval-cache and `auto-optimise-store` semantics is design work
   the V1 plan deliberately defers.

6. **Dispatch socket race on startup.** Window between supervisor
   spawn and dispatch-socket-listen where `mvmctl build` could
   probe and find no socket, falling back to single-shot. Mitigation:
   supervisor writes a `~/.mvm/run/builder-dispatch.pid` file
   *after* the socket is listening; `mvmctl build`'s probe checks
   the PID file's reachability before assuming "no supervisor".
   Trivial but easy to get wrong if not called out.

## Success criteria

1. Inside an `mvmctl dev` session, two consecutive `mvmctl build`
   invocations against the same flake show:
   - First build: full `BootTimings` recorded (cold boot).
   - Second build: `boot_timings: None`, `job_timings.dispatch_ms`
     populated, total wall-clock measurably lower than two
     consecutive single-shot builds.
2. `mvmctl dev down` releases the persistent VM cleanly; no
   orphan libkrun process.
3. Idle timeout fires after 30 min of no submits; subsequent
   `mvmctl build` triggers a cold restart silently.
4. `cargo test --workspace` clean; `cargo clippy --workspace --
   -D warnings` clean; new fuzz target builds and runs in CI.
5. `mvmctl build --no-reuse` from inside a dev session uses the
   single-shot path regardless of supervisor presence.
6. `mvmctl build` from outside a dev session uses the single-shot
   path (no supervisor running).
7. The trust posture in the paired ADR is reflected by the test
   suite: a malicious build cannot read a previous build's
   `/tmp/<job_id>/`, and the persistent VM's vsock dispatch
   passes the same fuzz patterns as the existing host↔guest RPCs.

## Order of operations

W1 (telemetry baseline) stands alone — its conclusion gates the
rest. If the numbers don't justify the work, this plan stops at W1.

Assuming W1 says go:

- **PR1**: W2 (vsock dispatch wire) — additive, no behavior change.
- **PR2**: W3 (persistent VM + dispatch loop, behind opt-in env var
  `MVM_PERSISTENT_BUILDER=1`).
- **PR3**: W4 (`mvmctl build` integration) + paired ADR.
- **PR4**: flip the default to enabled; idle timeout (W5) ships
  alongside.
- **PR5**: docs (CLAUDE.md note on shared-state semantics,
  `mvmctl dev status` reporting, `mvmctl boot-report` updates).

Each PR is independently revertible. PR4 is the gate to watch:
that's where existing dev-session users start sharing builder VMs
across jobs by default, so it lands with a smoke test against two
consecutive `mvmctl build`s in a single session and against the
malicious-build cross-job-leak test from §Success criteria item 7.
