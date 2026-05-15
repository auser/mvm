---
title: Guest Agent
description: The mvm guest agent provides host visibility and control over microVMs via vsock.
---

Every microVM built with `mkGuest` includes **mvm-guest-agent**, a lightweight Rust daemon that runs inside the guest on vsock port 5252.

## Capabilities

| Capability | Description |
|------------|-------------|
| **Health checks** | Runs per-service health commands on a schedule, reports results to the host |
| **Worker status** | Tracks idle/busy state by sampling `/proc/loadavg` — used by fleet autoscaling |
| **Snapshot lifecycle** | Coordinates sleep/wake: flushes data, drops page cache before snapshot, signals restore |
| **Integration management** | Loads service definitions from `/etc/mvm/integrations.d/*.json` |
| **Probes** | Loads read-only system checks from `/etc/mvm/probes.d/*.json` (disk usage, custom metrics) |
| **Filesystem diff** | Walks the overlay upper dir to report files created, modified, or deleted since boot |
| **Remote command** | Dev-only: execute commands inside the guest via vsock |

## Protocol

The agent communicates using **length-prefixed JSON frames** over vsock (Firecracker, Apple Container, microvm.nix) or a unix socket (Docker):

1. Host writes `CONNECT 5252\n` to the socket
2. Agent responds with `OK 5252\n`
3. All subsequent communication is request/response pairs

Request types: `ping`, `status`, `sleep-prep`, `wake`, and more.

## Profile gate

Every guest image declares an **agent profile** in its
`/etc/mvm/security.json` (plan 76 Phase 1). The profile is the
dispatcher-side allowlist for vsock verbs — dev-only requests
sent to a sealed-prod agent are rejected before any handler runs:

| Profile | Effective verb set | Used by |
|---------|-------------------|---------|
| `sealed-prod` (default) | Lifecycle, status, entrypoint, sleep/wake, volume mount/unmount, idle-timeout updates. The full ADR-002 production-safe surface. | Production images. The policy file lives on a dm-verity rootfs (ADR-002 §W3) so the profile cannot be widened at runtime. |
| `dev` | `sealed-prod` plus shell `Exec`, process RPC, filesystem RPC, console PTY, port forwarding, and `RunCode`. | `mvmctl dev` images and any image built with `dev-shell` feature. |
| `builder` | Reserved for builder-only verbs. The current builder agent speaks a separate `BuilderRequest` protocol, so this profile is wire-stable but unused for the tenant agent. | Future builder VM agent if/when its verbs land on the tenant wire. |

Rejected requests return a typed `UnsupportedInProfile` response:

```json
{ "UnsupportedInProfile": { "profile": "sealed-prod", "verb": "Exec" } }
```

SDK callers can branch on capability without parsing message text —
this is the protocol-layer analog of
`ProcErrorKind::UnsupportedInProduction` for process RPC.

The profile gate is **complementary** to the existing compile-time
gate (`#[cfg(feature = "dev-shell")]` for `do_exec` / `do_run_code` /
process RPC handlers per ADR-002 §W4.3, claim 4). The compile-time
gate keeps the handler symbols *absent* from production binaries; the
profile gate keeps the dispatcher *reachable but refusing* for dev
verbs in sealed-prod. Both checks run on every request.

## Readiness model

Plan 76 Phase 2 binds the vsock control port **before** entrypoint
validation and warm-process pool startup. Phase 3 extends the same
pattern to integration / probe drop-in scans. The agent accepts
`Ping` / `ReadinessStatus` / `EntrypointStatus` immediately, and
`RunEntrypoint` returns a typed `RunEntrypointError::NotReady` until
entrypoint validation completes.

Background init threads in order of when they start:

1. Entrypoint validation → warm-pool startup (serial inside one
   thread because the pool depends on `VALIDATED_ENTRYPOINT`).
2. Integration drop-in scan + health-loop spawn.
3. Probe drop-in scan + probe-loop spawn.

All three run in parallel after the accept loop is already serving
control-plane traffic. A malformed drop-in cannot block bind or
delay `Ping`; a slow `after_start.sh` only delays
`warm_pool_ready_ms`, not the rest of the readiness report.

A host queries the live state via the `ReadinessStatus` verb:

```json
{
  "ReadinessStatusReport": {
    "control_plane": "ready",
    "entrypoint": "starting",
    "warm_pool": "disabled",
    "integrations": "ready",
    "probes": "disabled",
    "volumes": "disabled",
    "profile": "sealed-prod",
    "boot_millis": {
      "agent_started_ms": 7,
      "vsock_bound_ms": 7,
      "first_accept_ms": 12,
      "entrypoint_ready_ms": null,
      "warm_pool_ready_ms": null,
      "integrations_ready_ms": null,
      "probes_ready_ms": null
    }
  }
}
```

`ComponentState` values:

| State | Meaning |
|-------|---------|
| `disabled` | Subsystem isn't configured for this image (no policy → no state machine to advance). Distinct from `ready` so the host can tell "image opted out" from "still warming". |
| `starting` | Background init in progress. `RunEntrypoint` while `entrypoint = starting` returns `NotReady` — the host should poll readiness and retry. |
| `ready` | Subsystem is up and accepting work. |
| `failed` | Subsystem failed to initialize. Carries a short human-readable `message` (no secrets, no host paths the caller doesn't already know). For `entrypoint`, this maps to `RunEntrypoint` returning the existing `EntrypointInvalid`. |

`BootTimingReport` exposes monotonic milliseconds since agent
process start. Phase 4 fills in the remaining fields
(`warm_pool_ready_ms`, `integrations_ready_ms`, `probes_ready_ms`);
the four populated today are enough to surface cold-path regressions.

## Health Checks

Health checks defined in `mkGuest`'s `healthChecks` parameter are automatically written to `/etc/mvm/integrations.d/` at build time:

```json
{
  "name": "my-service",
  "health_cmd": "curl -sf http://localhost:8080/health",
  "health_interval_secs": 10,
  "health_timeout_secs": 5
}
```

The agent picks them up on boot and begins periodic checks immediately.

### Startup Grace Period

Services that take time to initialize (e.g., running database migrations) can specify a grace period. During the grace period, health check failures are suppressed and the service reports `Starting` status instead of `Error`:

```json
{
  "name": "my-service",
  "health_cmd": "curl -sf http://localhost:8080/health",
  "health_interval_secs": 10,
  "health_timeout_secs": 5,
  "startup_grace_secs": 120
}
```

In a Nix flake, set the grace period via `startupGraceSecs`:

```nix
healthChecks.my-app = {
  healthCmd = "curl -sf http://localhost:8080/health";
  healthIntervalSecs = 10;
  startupGraceSecs = 120;  # suppress failures for 2 minutes after boot
};
```

After the grace period expires, normal health reporting resumes.

## Querying from the Host

```bash
# Check guest console output
mvmctl logs my-vm

# Follow logs in real time
mvmctl logs my-vm -f

# List VMs and their status
mvmctl ls
```

Health check results and probe output are included in the guest console logs.

## Probes

Probes are read-only system checks loaded from `/etc/mvm/probes.d/*.json`:

```json
{
  "name": "disk-usage",
  "command": "df -h /mnt/data | tail -1 | awk '{print $5}'",
  "interval_secs": 60
}
```

Probe results are reported via the vsock protocol and included in guest console logs.

## Snapshot Coordination

Before creating a snapshot, the host sends a `sleep-prep` request. The agent:

1. Runs checkpoint commands for each integration
2. Syncs filesystem buffers
3. Drops page cache
4. Responds with "ready"

On wake (snapshot restore), the host sends a `wake` request and the agent runs restore commands for each integration.

## Filesystem Diff

The agent can report all filesystem changes since boot by walking the overlay upper directory. When the rootfs is mounted read-only with an overlay (`readOnlyRoot = true` in mkGuest), all writes go to the upper dir. The agent detects:

- **Created** files: present in the overlay upper dir
- **Deleted** files: overlay whiteout files (`.wh.*`)
- **Modified** files: existing files overwritten in the upper dir

Query the diff from the host:

```bash
mvmctl diff my-vm         # human-readable output
mvmctl diff my-vm --json  # JSON array of {path, kind, size}
```

This is useful for auditing what an AI agent modified during execution.
