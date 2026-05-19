# Plan 89 — Persistent builder VM via vsock dispatch

**Status:** drafted 2026-05-19, security-scan amendments 2026-05-19,
awaiting review.
**Pairs with:** new ADR (TBD — see §Pairing ADR) describing the trust
boundary change from one-job-per-VM to many-jobs-per-VM.
**Tracked pre-existing bugs surfaced by scan:** issues
[#370](https://github.com/tinylabscom/mvm/issues/370) (`/work`
virtio-fs RO mismatch) and
[#371](https://github.com/tinylabscom/mvm/issues/371) (concurrent
nix-store disk corruption). Both must be resolved before PR4 (the
flip-to-enabled gate) since this plan escalates their blast radius.

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
- **Pre-deserialize frame size cap (security-scan F8).** The framing
  reader path (`crates/mvm-guest/src/vsock.rs::read_authenticated_frame`
  and its host-side mirror) currently reads a `length_prefix: u32` and
  allocates that many bytes before HMAC-verifying. An attacker who
  reaches the dispatch port — or a corrupted client — can request a
  4 GiB allocation, OOM-killing the host supervisor. Single-shot today
  only kills that build; persistent mode kills the dev session and any
  in-flight job. Introduce `const MAX_BUILDER_FRAME_BYTES: u32 = 16 *
  1024 * 1024;` (16 MiB — large enough for any realistic
  `BuilderRequest`; tighten later if telemetry shows we're nowhere
  near it), reject frames over the cap with a typed error *before* the
  allocation, and add a fuzz seed that pins `length_prefix = u32::MAX`
  to lock in the regression.
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
- **Orphan sweep on `dev up` (security-scan F6).** If the host
  supervisor is SIGKILL'd (`pkill -9`, OOM-killer, host crash) the
  libkrun child inherits init and survives. A fresh `mvmctl dev up`
  would then race with an orphan that's still holding the nix-store
  image lock and the dispatch port. Before spawning a new supervisor,
  read `~/.mvm/run/builder-dispatch.pid`, check `/proc/<pid>` (Linux)
  or `kill(pid, 0)` (macOS), and if alive: confirm via the dispatch
  socket it's actually our supervisor (challenge-response), reuse it
  if so, SIGTERM-then-SIGKILL it otherwise. Set `PR_SET_PDEATHSIG=
  SIGTERM` on the libkrun child (Linux) and an equivalent kqueue
  `EVFILT_PROC` watcher (macOS) so the next supervisor crash takes the
  VM with it instead of orphaning. Add a smoke test: SIGKILL the
  supervisor, run `mvmctl dev up`, assert the second supervisor
  starts cleanly and the orphan libkrun is gone.
- **Per-dispatch chain-signed audit entries (security-scan F5).**
  Claim 8 ("every workload runs from a signed, audited
  `ExecutionPlan`") relies on `mvm-supervisor`'s admission emitting
  `plan.admitted` / `plan.launched` / `plan.failed` chain-signed
  entries to `~/.mvm/audit/<tenant>.jsonl`. The current admission gate
  fires once per VM spawn; with persistent dispatch, one admission
  fans out to many builds and the audit chain stops at the admission.
  Each dispatched job is a workload in its own right and must extend
  the chain. Emit `builder.job.dispatched`, `builder.job.completed`,
  and `builder.job.failed` from the host supervisor (not from inside
  the VM — the VM can't sign as the host), each carrying the
  `job_id`, the `BuilderJob` hash, the per-job `JobTimings`, and the
  parent admission record's ID. `mvmctl audit verify` must continue
  to exit nonzero on chain drift. Mirror the
  `verify_audit_chain` test pattern (Plan 64 W4) for the new entry
  shapes.

**W4 — `mvmctl build` integration (~1 day).**

- `mvmctl build` looks for the supervisor's AF_UNIX socket at
  `~/.mvm/run/builder-dispatch.sock` (created by the supervisor on
  startup, mode 0600, owner-only — matches `~/.mvm` hardening from
  ADR-002 W1.5).
- Socket present + reachable → submit job via supervisor.
- Socket absent or unreachable → fall back to
  `LibkrunBuilderVm::run_build`.
- **Demotion is announced, not silent (security-scan F4).** The
  original draft made the fallback silent; the scan flagged this as a
  trust-model surprise. A user expecting amortized boot but getting
  cold-boot every time has no way to tell. On every fallback,
  `mvmctl build` prints (to stderr, not the build log) `warning:
  persistent builder unavailable (reason: <not-running | crashed |
  workspace-out-of-tree>), falling back to single-shot — boot fan-out
  paid for this build`. The reason is taken from the supervisor's
  liveness probe (see PID-file TOCTOU fix in W5), not inferred from
  the socket's absence alone. `mvmctl dev status` also surfaces the
  current persistent-builder state, so users can self-diagnose
  without running a doomed `mvmctl build`.
- A `--no-reuse` flag on `mvmctl build` forces single-shot for cases
  where the user wants isolation (e.g. debugging a flaky build, or
  running against a workspace outside the dev session's root — see
  §Workspace binding).

**W5 — Idle timeout, resource release, recovery (~1 day).**

- Idle timer: 30 minutes (configurable via
  `MVM_BUILDER_IDLE_TIMEOUT_SECS`, see §Open questions resolution).
  On expiry the supervisor sends `Shutdown` and tears down the VM.
  Next `mvmctl build` triggers a cold restart of the supervisor's VM
  (boot fan-out paid again).
- **PID-file TOCTOU fix (security-scan F4).** Risks §6 calls out a
  startup-race between supervisor spawn and dispatch-socket-listen,
  resolved by writing `~/.mvm/run/builder-dispatch.pid` after the
  socket is listening. That's still a TOCTOU: between
  `mvmctl build`'s `read(pid_file)` and its `connect(socket)`, the
  supervisor can crash and a new one can recycle the same PID. Atomic
  fix: the supervisor opens the socket first, writes a `{pid, nonce,
  start_time_epoch}` JSON line to the PID file (write-then-rename for
  atomic publication), and `mvmctl build` includes the read nonce
  in its first dispatch frame; the supervisor rejects mismatched
  nonces. Cost: one frame field, one extra reject path. Removes the
  whole class of "I connected to the wrong supervisor" bugs.
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

**Issue [#370](https://github.com/tinylabscom/mvm/issues/370) is a
hard prerequisite for this plan.** The `/work` share is documented
RO but is mounted RW today (`crates/mvm-build/src/libkrun_builder.rs:88-90`
lies; `crates/mvm-libkrun/src/lib.rs:211-214` notes libkrun has no
`readonly` toggle; `crates/mvm-builder-init/src/main.rs:856-867`
mounts with `MsFlags::empty()`). Single-shot bounds the blast
radius to one job's worth of write-back; persistent dispatch
extends it to every job in the session. Until #370 is fixed (guest
mounts `work` tag with `MS_RDONLY`, host updates the lying doc, and
the libkrun `readonly` toggle is plumbed when upstream lands it),
PR4 (the flip-to-enabled gate) must not merge.

**Submodule and symlink ambiguity (security-scan F9).** "Git
toplevel of `$PWD`" is ambiguous when the user runs `mvmctl dev up`
from inside a git submodule (toplevel is the *outer* repo, which
the user may not have intended to expose) or from a directory
reached via symlinks (resolving vs. preserving changes which paths
end up under `/work`). Resolve both at supervisor start: (a)
`git rev-parse --show-superproject-working-tree` — if non-empty,
warn the user that the outer repo is being bound and require
`--workspace <path>` to override; (b) canonicalize the bind path
with `std::fs::canonicalize` before passing to libkrun, and reject
binds that resolve outside the user's `$HOME` (a symlink farm
pointing at `/etc` should not be silently mounted into the builder
VM). Record the canonical path in `mvmctl dev status` output.

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

### Why per-job `/tmp` (and what else needs per-job isolation)

Today's one-shot VM has a fresh tmpfs `/tmp` because the VM itself is
fresh. With reuse, two consecutive jobs share `/tmp` unless we
explicitly isolate. `mvm-builder-init`'s dispatch loop creates
`/tmp/<job_id>/` per job and exports `TMPDIR=/tmp/<job_id>`,
`HOME=/tmp/<job_id>`, `XDG_STATE_HOME=/tmp/<job_id>/.local/state`
into the build subprocess's env. `XDG_CACHE_HOME` stays on
`/nix-store/.cache` (PR #368) because cross-job cache reuse is the
whole point of that path.

**Per-job `/tmp` alone is not enough (security-scan F2).** The scan
called out that a malicious build can still leak across jobs via
several channels that the env-var trick doesn't close:

- **Orphan processes via `setsid` / daemonization.** A build that
  forks a long-lived child detached from the dispatch loop's process
  group will survive `rm -rf /tmp/<job_id>/`. Mitigation: the per-job
  exec runs under `unshare --pid --mount --ipc --net=none --fork`
  (Linux only — that's all we run inside the builder VM). The pid
  namespace makes job teardown a single `kill -KILL -1` from inside
  the namespace; mount/ipc namespaces tear down the per-job
  bind-mounts and SysV/POSIX IPC keys atomically; the `net=none` is
  belt-and-suspenders since the per-VM iptables baseline already
  blocks egress (see iptables note below).
- **`/dev/shm`, `/run`, `/var/tmp`.** Per-job tmpfs bind-mounts —
  `mount -t tmpfs none /tmp/<job_id>/shm` then
  `mount --bind /tmp/<job_id>/shm /dev/shm`, same for the others.
  Tearing down the mount namespace at job exit removes them.
- **Inherited open FDs.** The dispatch loop opens vsock + virtio-fs
  fds. Set `FD_CLOEXEC` on every fd before exec; the only fds the
  build sees are stdin/stdout/stderr.
- **uid 0 inside the VM.** Builds currently run as root, which makes
  every isolation primitive above defeasible (a malicious build can
  remount, can `kill -9` outside its pid namespace from the parent
  pid ns, etc.). The build subprocess drops to a fixed unprivileged
  uid (`builder:builder`, uid 902, created by `mvm-builder-init` at
  boot) via `setuid` before the build script runs. The dispatch loop
  itself stays uid 0 because it needs to set up the namespaces.

On job completion the dispatch loop tears down the namespace (one
`kill -KILL -1` from inside, then `waitpid`), then `rm -rf
/tmp/<job_id>/`. The namespace teardown is the load-bearing step;
the `rm -rf` is belt-and-suspenders.

**Per-VM iptables baseline accumulates state (security-scan F7).**
Plan 73's network setup installs an iptables baseline once at boot
inside the VM (allowlist for the agent/forward vsock ranges, drop
everything else). Single-shot tears this down with the VM. Persistent
mode keeps it; a build that does `iptables -I OUTPUT 1 -j ACCEPT` (or
loads a kernel module that does, or exploits a `CAP_NET_ADMIN` leak)
poisons every subsequent job. Mitigation: at the start of each
dispatched job, the dispatch loop re-applies the baseline from a
known-good in-VM script (`/etc/mvm/net-baseline.sh`, root-readable
only, ext4 immutable bit via `chattr +i`). Cheap (~10 ms) and
deterministic. Strip `CAP_NET_ADMIN` from the build subprocess as
well so the in-job mutation path requires kernel exploit, not just a
shell command.

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

**Cross-process exclusivity on the nix-store image (issue
[#371](https://github.com/tinylabscom/mvm/issues/371) + security-scan
F10).** The in-supervisor mutex only protects jobs *within one
supervisor*. A second process (`mvmctl deps install` running in
parallel, a second `mvmctl dev up` against the same `$HOME`) can
attach `~/.cache/mvm/builder-vm/nix-store-<arch>.img` to its own VM
and the two ext4 mounts will silently corrupt the disk. Fix:
`fcntl(F_SETLK, F_WRLCK)` on the image file in
`ensure_nix_store_image()` (`crates/mvm-build/src/libkrun_builder.rs:754`)
before any VM attaches it. Hold the lock for the VM's lifetime;
release on VM exit. Block-with-friendly-message when contended
(`waiting for nix-store lock held by pid N (<cmd>)`). This is issue
#371's fix — listed here because the persistent VM holds the image
open for the entire dev session, which makes the contention window
hours instead of seconds, so this plan can't ship without it.

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
together. The ADR should cover (security-scan F11 broadened this
list — the original four items missed several decisions the scan
flagged):

1. What the persistent VM means for ADR-002's claim 1 (no host-fs
   access beyond explicit shares). The naive read of the claim is
   preserved — the workspace mount is the same virtio-fs share, just
   bound earlier — but issue #370 shows the share has been *RW*, not
   RO, the entire time. The ADR must (a) state the actual current
   reality, (b) describe the fix landing under #370, (c) explain why
   PR4 (flip-to-enabled) is gated on #370 rather than treating it as
   parallel work.
2. Cross-job state isolation: what's per-job (`/tmp/<job_id>/`,
   `/dev/shm/<job_id>/`, mount/pid/ipc namespaces, dropped to
   uid 902) and what's shared (`/nix-store/`, eval-cache, tarball
   cache). Document the shared-cache poisoning risk from §Risks §7
   and the V1 mitigation matrix. Document that admission audit
   (Claim 8) extends with per-dispatch `builder.job.*` chain entries
   so a compromised build's effect is auditable post-hoc.
3. Vsock dispatch surface: same framing as every other host↔guest
   RPC, no new key material, fuzzed by Plan 73 patterns. The
   `MAX_BUILDER_FRAME_BYTES` pre-deserialize length cap is a new
   defensive layer because the OOM-via-`u32::MAX` vector becomes
   session-killing instead of build-killing under persistence.
4. Failure mode: VM crash demotes the supervisor; queue drains via
   single-shot. Persistent VM does not auto-restart in V1.
   **Demotion is announced, not silent** — see W4. The trust model
   relies on the user knowing whether they're in persistent or
   single-shot mode.
5. Workspace-bind blast radius: which host paths can end up under
   `/work`. Cover the submodule case (outer-repo bind requires
   explicit opt-in), the symlink case (canonicalize before bind,
   reject outside `$HOME`), and the read-only enforcement story tied
   back to item 1.
6. nix-store image exclusivity: `flock` semantics, contention UX,
   how it interacts with concurrent `mvmctl deps install` (issue
   #371). The persistent VM holds the lock for the dev session,
   which is hours instead of seconds — the ADR should be explicit
   about what "you can't run `mvmctl deps install` while a dev
   session is up" means for users and how the error surfaces.
7. Per-job pid/mount/ipc namespace decision: why we picked
   `unshare --pid --mount --ipc --net=none --fork` over weaker
   options (env-vars only; cgroup-only isolation; container-style
   chroot). Reference the kernel features available in the libkrun
   bundled kernel (verify with `grep CONFIG_USER_NS
   /proc/config.gz` from inside the VM — both `USER_NS` and
   `PID_NS` are required).
8. Orphan supervision: `PR_SET_PDEATHSIG` (Linux) / `EVFILT_PROC`
   (macOS) wiring, what we do when `mvmctl dev up` finds a stale
   PID file, how the host signer key (Plan 64 W2) and audit chain
   survive a supervisor restart.

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
  of this one. The `flock` on the nix-store image (see §Concurrency)
  means `mvmctl deps install` cannot run concurrently with an active
  dev session's persistent builder; that's a UX constraint we accept
  in V1 rather than a behavioral regression.
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
   the PID file's reachability before assuming "no supervisor". The
   PID file carries `{pid, nonce, start_time_epoch}` and the
   client echoes the nonce in its first dispatch frame — see W5
   PID-file TOCTOU fix.

7. **Shared eval-cache / tarball-cache poisoning (security-scan F3).**
   `XDG_CACHE_HOME` lives on the persistent `/nix-store` so jobs can
   reuse Nix flake evaluation and downloaded tarballs across builds
   (the whole point of PR #368). A compromised build can poison the
   eval-cache (replacing a cached `flake.lock` resolution) or the
   tarball cache (replacing a fetched source tarball). `nix-store
   --verify` covers store *paths* but not the eval-cache database
   files or the tarball cache; `auto-optimise-store` only re-hardlinks
   store paths. Mitigation matrix for V1: (a) the cache directory
   lives at a fixed in-VM path with `chmod 0700`; (b) the per-job
   build runs as `builder:builder` (uid 902) which has read-only
   access to the cache dir and write access only via a privileged
   helper that re-validates each cache mutation against the store
   path it's keyed to; (c) document in CLAUDE.md that the persistent
   cache shares trust with the dev session, so anyone running
   `mvmctl build` on untrusted input has the same threat model as
   running `nix build` on it. V2+: signed cache entries keyed by
   builder identity. Track as an open ADR question; do not silently
   inherit PR #368's posture.

8. **Supervisor SIGKILL leaves orphan libkrun (security-scan F6).**
   See W3 "orphan sweep on `dev up`". Captured here so the risk is
   visible at the top of the threat table, not buried in a
   workstream's bullets.

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

**Hard prerequisites for PR4 (from security-scan findings):**
issues [#370](https://github.com/tinylabscom/mvm/issues/370) (RO
`/work`) and [#371](https://github.com/tinylabscom/mvm/issues/371)
(nix-store `flock`) must be merged. Per-job namespace isolation
(W3, §Why per-job /tmp) and per-dispatch audit entries (W3) must
have green smoke tests. PR4's CI gate runs the cross-job-leak test
*and* asserts `mvmctl audit verify` exits zero after a multi-job
session.

## Security-scan finding index

The security scan run on 2026-05-19 against the original drafted
plan produced eleven findings. This index maps each to the section
that addresses it, so a future reader can audit coverage without
re-reading the scan report:

| # | Severity | Finding | Plan section |
|---|---|---|---|
| F1 | Critical | RW `/work` virtio-fs (issue #370) | §Workspace mount strategy, §Pairing ADR §1 |
| F2 | High | Per-job `/tmp` insufficient (orphans, `/dev/shm`, FDs, uid 0) | §Why per-job /tmp |
| F3 | High | Shared `XDG_CACHE_HOME` poisonable | §Risks §7, §Pairing ADR §2 |
| F4 | Medium | Silent demotion + PID-file TOCTOU | §W4 (announced demotion), §W5 (TOCTOU fix) |
| F5 | Medium | Audit chain stops at admission | §W3 (per-dispatch audit), §Pairing ADR §2 |
| F6 | Medium | Supervisor SIGKILL → orphan libkrun | §W3 (orphan sweep), §Risks §8 |
| F7 | Medium | In-VM iptables baseline only at boot | §Why per-job /tmp (baseline re-apply) |
| F8 | Medium | Vsock no pre-deserialize length cap | §W2 (`MAX_BUILDER_FRAME_BYTES`) |
| F9 | Low | Workspace bind ambiguity (submodule / symlink) | §Workspace mount strategy |
| F10 | Low | Concurrent `deps install` corrupts nix-store (issue #371) | §Concurrency (`flock`), §Non-goals |
| F11 | Info | Pairing-ADR scope too narrow | §Pairing ADR (items 5–8 added) |
