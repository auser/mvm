# Minimum Runtime Policy

## Semantics

The minimum runtime policy prevents premature reclamation of worker instances by enforcing
minimum durations in each lifecycle state before allowing transitions.

| Field | Default | Effect |
|-------|---------|--------|
| `min_running_seconds` | 60 | Instance must stay Running for this long before eligible for Warm or Stopped |
| `min_warm_seconds` | 30 | Instance must stay Warm for this long before eligible for Sleeping |
| `drain_timeout_seconds` | 30 | Max wait for guest drain ACK before forcing sleep |
| `graceful_shutdown_seconds` | 15 | Max wait for FC process to exit before SIGKILL |

Enforcement is entirely on the **host agent** using wall-clock timestamps (`entered_running_at`,
`entered_warm_at` on `InstanceState`). The guest is not involved in enforcement decisions.

### Transition Guards

```
Running -> Warm:     blocked if elapsed < min_running_seconds
Running -> Stopped:  blocked if elapsed < min_running_seconds
Warm -> Sleeping:    blocked if elapsed < min_warm_seconds
All other transitions: always allowed
```

When a transition is blocked, the sleep policy returns `SleepAction::None` with a reason
indicating which minimum was not satisfied. The reconcile loop logs a `TransitionDeferred`
audit entry and increments the `mvm_instances_deferred_total` Prometheus counter.

## Exceptions

### Memory Pressure Override

Under memory pressure, `pressure_candidates()` still includes instances that haven't met
their minimum runtime. However, eligible instances are **prioritized** (sorted first by
eligibility, then by idle time descending). This means:

- Normal operation: ineligible instances are skipped entirely
- Memory pressure: ineligible instances are deprioritized but may still be reclaimed

### Force Flag

`instance_sleep(..., force: true)` bypasses the vsock drain protocol entirely. It does NOT
bypass the minimum runtime check in the sleep policy — that check happens before `instance_sleep`
is called, in `evaluate_instance()`.

### Pinned / Critical Pools

Pools with `pinned: true` or `critical: true` bypass all sleep policy evaluation entirely.
Their instances are never candidates for warm/sleep transitions regardless of idle metrics
or minimum runtime.

## Drain Protocol

When the sleep policy decides to sleep an instance (Warm -> Sleeping), the host performs
a cooperative drain via the vsock guest agent:

1. **Host sends `SleepPrep`** via vsock with `drain_timeout_secs` from the pool's runtime policy
2. **Guest agent receives request** and:
   - Finishes/checkpoints in-flight OpenClaw work
   - Flushes data to disk
   - Drops page cache
   - Responds with `SleepPrepAck { success: true }`
3. **Host receives ACK** and proceeds with snapshot + shutdown
4. **On timeout**: If no ACK within `drain_timeout_secs`, host logs a `MinRuntimeOverridden`
   audit event and forces the sleep anyway (data integrity best-effort)
5. **On vsock failure**: If the vsock connection fails (e.g. guest agent not running),
   host proceeds with sleep (best-effort)

### Wake Signal

After restoring from snapshot, the host sends a `Wake` request via vsock. The guest agent
should reinitialize connections and refresh secrets. This is best-effort — failure does not
block the wake operation.

## Drive Model

Each Firecracker instance mounts up to 4 drives:

| Drive | ID | Access | Lifecycle | Contents |
|-------|----|--------|-----------|----------|
| rootfs | `rootfs` | read-only | Immutable, shared across pool | Kernel + base filesystem |
| config | `config` | read-only | Recreated on every start/wake | Instance/pool metadata JSON |
| data | `data` | read-write | Persistent, optional LUKS | Application data |
| secrets | `secrets` | read-only | Recreated on every start/wake, tmpfs-backed | Tenant secrets |

### Config Drive Contents

The config drive contains a single `config.json` with non-sensitive metadata:

```json
{
  "instance_id": "i-abc123",
  "pool_id": "workers",
  "tenant_id": "acme",
  "guest_ip": "10.240.3.5",
  "vcpus": 2,
  "mem_mib": 1024,
  "min_runtime_policy": {
    "min_running_seconds": 60,
    "min_warm_seconds": 30,
    "drain_timeout_seconds": 30,
    "graceful_shutdown_seconds": 15
  }
}
```

The guest agent reads this on boot to configure itself without requiring SSH or any
network-based configuration mechanism.

### Trust Boundaries

- **Config drive**: Non-sensitive metadata. Separate from secrets for clear trust boundary.
  Readable by any process in the guest.
- **Secrets drive**: Sensitive material (API keys, tokens). tmpfs-backed (never touches disk
  on the host). Mounted with restrictive permissions in the guest.

## OpenClaw Safety

The minimum runtime policy ensures OpenClaw workers are not reclaimed before they've had
time to process requests:

1. **Startup protection**: `min_running_seconds` prevents warm/stop within the first N seconds
   after boot or wake, giving the worker time to register with the scheduler and pick up work
2. **Drain protocol**: Before sleeping, the host asks the guest agent to checkpoint work.
   The guest can finish in-flight requests before acknowledging
3. **Data integrity**: The data drive is persistent and survives sleep/wake cycles. LUKS
   encryption protects data at rest when a tenant key is provisioned
4. **Secrets refresh**: Fresh secrets are mounted on every start/wake via the secrets drive.
   The config drive provides non-secret metadata without SSH

## Configuration

Set via pool spec (coordinator pushes via desired state):

```json
{
  "pool_id": "workers",
  "runtime_policy": {
    "min_running_seconds": 120,
    "min_warm_seconds": 60,
    "drain_timeout_seconds": 45,
    "graceful_shutdown_seconds": 20
  }
}
```

All fields have sensible defaults. Omitting `runtime_policy` entirely uses:
- `min_running_seconds`: 60
- `min_warm_seconds`: 30
- `drain_timeout_seconds`: 30
- `graceful_shutdown_seconds`: 15

Set both `min_running_seconds` and `min_warm_seconds` to 0 to disable minimum runtime
enforcement (transitions happen immediately based on idle metrics alone).
