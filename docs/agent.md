# Agent & Reconciliation

## Overview

The mvm agent is a node-level component that manages the local fleet of microVM instances. It operates in two modes:

1. **One-shot reconcile** (`mvm agent reconcile`) -- reads a desired state file and converges
2. **Daemon** (`mvm agent serve`) -- runs a continuous reconcile loop with a QUIC+mTLS API server

## Desired State Schema

The coordinator produces a desired state document scoped to a specific node:

```json
{
  "schema_version": 1,
  "node_id": "node-abc123",
  "tenants": [
    {
      "tenant_id": "acme",
      "network": {
        "tenant_net_id": 3,
        "ipv4_subnet": "10.240.3.0/24"
      },
      "quotas": {
        "max_vcpus": 16,
        "max_mem_mib": 32768,
        "max_running": 8,
        "max_warm": 4,
        "max_pools": 3,
        "max_instances_per_pool": 10,
        "max_disk_gib": 100
      },
      "secrets_hash": "sha256:abc...",
      "pools": [
        {
          "pool_id": "workers",
          "flake_ref": "github:org/openclaw-worker?rev=abc123",
          "profile": "minimal",
          "instance_resources": {
            "vcpus": 2,
            "mem_mib": 1024,
            "data_disk_mib": 2048
          },
          "desired_counts": {
            "running": 3,
            "warm": 1,
            "sleeping": 2
          },
          "seccomp_policy": "baseline",
          "snapshot_compression": "zstd"
        }
      ]
    }
  ],
  "prune_unknown_tenants": false,
  "prune_unknown_pools": false
}
```

### Mandatory Fields

- `network.tenant_net_id` and `network.ipv4_subnet` are **required** for every tenant. The agent rejects any tenant entry missing these fields.
- `quotas` defines the maximum resources this tenant may consume on this node.
- `desired_counts` tells the agent how many instances should be in each state.

### What the Coordinator Owns

- Tenant network allocation (subnet + net_id)
- Tenant quotas
- Pool definitions (flake ref, profile, resources)
- Desired instance counts per pool
- Prune policy

### What the Agent Owns

- Instance IDs (generated locally, never sent to coordinator)
- Instance IPs (offsets within coordinator-assigned subnet)
- Build execution
- Snapshot management
- Local resource scheduling

## Reconcile Loop

```
for each desired_tenant:
    REQUIRE network.tenant_net_id and network.ipv4_subnet
    ensure tenant exists (create dirs, bridge if missing)
    apply coordinator-assigned network config
    update quotas

    for each desired_pool:
        ensure pool exists
        if artifacts missing or flake_ref changed:
            build (ephemeral FC microVM + nix build)

        count current instances by status
        compute tenant resource usage

        # Scale to desired counts (in order):
        1. Wake sleeping -> running     (if running deficit)
        2. Resume warm -> running       (if running deficit)
        3. Start stopped -> running     (if running deficit)
        4. Create new -> running        (if deficit AND quota allows)
        5. Stop excess running          (if running surplus)
        6. Warm running -> warm         (to match warm count)
        7. Sleep warm -> sleeping       (to match sleeping count)

        # Quota check before every start/wake/create

    if prune_unknown_pools:
        destroy pools not in desired list

if prune_unknown_tenants:
    destroy tenants not in desired list
```

### Enforcement Rules

- **Quota enforcement** -- the agent checks `compute_tenant_usage()` against `TenantQuota` before every start/wake/create operation
- **cgroup v2 limits** -- per-instance memory.max, cpu.max, pids.max; per-tenant aggregate tracking
- **Network isolation** -- per-tenant bridge, cross-tenant denied by construction
- **Filesystem isolation** -- per-tenant directories, no shared writable state
- **Audit logging** -- all lifecycle events logged to per-tenant audit.log

### Pinned and Critical

- **Pinned tenants** (`pinned: true` in TenantConfig) -- reconcile cannot auto-stop any of the tenant's instances
- **Pinned pools** (`pinned: true` in PoolSpec) -- reconcile won't auto-sleep instances in this pool
- **Critical pools** (`critical: true` in PoolSpec) -- reconcile won't touch instances at all

### Manual Override

When a user manually operates on an instance via CLI (e.g., `mvm instance stop`), a short-lived `manual_override_until` timestamp is set on the InstanceState. The reconcile loop respects this window and won't fight the manual action until it expires.

## Daemon Mode

`mvm agent serve` starts a tokio async runtime with:

### QUIC + mTLS API Server

- Transport: `quinn` + `rustls`
- Certificate hierarchy: Root CA -> Coordinator cert + Node certs
- Short-lived certificates (24h default), auto-renewal at 50% lifetime
- Rate limiting: token-bucket, 10 req/s default

### API Endpoints

```
POST /v1/reconcile                                    # push desired state
GET  /v1/node/info                                    # node capabilities
GET  /v1/node/stats                                   # aggregate stats
GET  /v1/tenants                                      # list tenants with usage
GET  /v1/tenants/<id>/instances                       # list instances
POST /v1/tenants/<id>/pools/<p>/instances/<i>/wake    # urgent wake
```

### Reconcile Loop (in daemon)

- Runs periodically (default 30s, configurable via `--interval-secs`)
- Reads desired state from file or last pushed state via API
- Executes the reconcile algorithm above
- Also evaluates sleep policy per pool

### Certificate Management

```bash
# Initialize with CA certificate
mvm agent certs init --ca ca.crt

# Request a node certificate from coordinator
mvm agent certs request --coordinator https://coordinator:4433

# Rotate certificates (auto-renewal)
mvm agent certs rotate

# Check certificate status
mvm agent certs status --json
```

## Sleep Policy Integration

The daemon also runs sleep policy evaluation alongside reconciliation:

- **Idle detection** -- per-instance metrics: last work timestamp, CPU moving average, guest heartbeat
- **Policy** (per-pool, configurable):
  - idle > T1 (default 5min) -> warm
  - idle > T2 (default 15min) -> sleep
  - memory pressure -> sleep coldest instances first
- **Guards** -- never sleep pinned/critical pools
- **Worker hooks** -- guest signals at `/run/mvm/worker-{ready,idle,busy}` inform policy decisions

## Graceful Shutdown

On SIGTERM:
- Stop accepting new API requests
- Complete in-flight operations
- Do NOT stop running instances (they survive agent restart)
- Persist current state
- Exit cleanly
