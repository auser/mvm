# Desired State Schema Reference

The desired state document drives the agent's reconciliation loop. It declares the intended configuration for a node, and the agent converges the actual state to match.

## Schema

```json
{
  "schema_version": 1,
  "node_id": "string",
  "tenants": [ ... ],
  "prune_unknown_tenants": false,
  "prune_unknown_pools": false
}
```

### Top-Level Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | `u32` | Yes | Must be `1` |
| `node_id` | `string` | Yes | Identifier for this node |
| `tenants` | `array` | Yes | List of desired tenants |
| `prune_unknown_tenants` | `bool` | No | If true, destroy tenants not in this document |
| `prune_unknown_pools` | `bool` | No | If true, destroy pools not in this document |

### Tenant Object

```json
{
  "tenant_id": "string",
  "network": { ... },
  "quotas": { ... },
  "pools": [ ... ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tenant_id` | `string` | Yes | Lowercase alphanumeric + hyphens |
| `network` | `object` | Yes | Network identity (coordinator-assigned) |
| `quotas` | `object` | Yes | Resource limits |
| `pools` | `array` | Yes | List of desired pools |

### Network Object

```json
{
  "tenant_net_id": 3,
  "ipv4_subnet": "10.240.3.0/24"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tenant_net_id` | `u16` | Yes | Unique network ID (0-4095) |
| `ipv4_subnet` | `string` | Yes | IPv4 CIDR subnet for this tenant |

The gateway is derived as the `.1` address of the subnet. The bridge is named `br-tenant-<net_id>`.

### Quotas Object

```json
{
  "max_vcpus": 16,
  "max_mem_mib": 32768,
  "max_running": 8,
  "max_warm": 4,
  "max_pools": 10,
  "max_instances_per_pool": 32,
  "max_disk_gib": 500
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `max_vcpus` | `u32` | Yes | Max total vCPUs across all instances |
| `max_mem_mib` | `u64` | Yes | Max total memory in MiB |
| `max_running` | `u32` | Yes | Max concurrently running instances |
| `max_warm` | `u32` | Yes | Max warm (paused) instances |
| `max_pools` | `u32` | Yes | Max pools per tenant |
| `max_instances_per_pool` | `u32` | Yes | Max instances per pool |
| `max_disk_gib` | `u64` | Yes | Max total disk in GiB |

### Pool Object

```json
{
  "pool_id": "workers",
  "flake_ref": "./my-flake",
  "profile": "baseline",
  "instance_resources": { ... },
  "desired_counts": { ... }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pool_id` | `string` | Yes | Pool identifier within the tenant |
| `flake_ref` | `string` | Yes | Nix flake reference |
| `profile` | `string` | Yes | Build profile name |
| `instance_resources` | `object` | Yes | Per-instance resource allocation |
| `desired_counts` | `object` | Yes | Target instance counts by state |

### Instance Resources Object

```json
{
  "vcpus": 2,
  "mem_mib": 1024,
  "data_disk_mib": 0
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `vcpus` | `u8` | Yes | vCPUs per instance (must be > 0) |
| `mem_mib` | `u32` | Yes | Memory per instance in MiB |
| `data_disk_mib` | `u32` | Yes | Data disk size in MiB (0 = none) |

### Desired Counts Object

```json
{
  "running": 4,
  "warm": 2,
  "sleeping": 0
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `running` | `u32` | Yes | Target running instance count |
| `warm` | `u32` | Yes | Target warm instance count |
| `sleeping` | `u32` | Yes | Target sleeping instance count |

## Reconciliation Behavior

When the agent processes a desired state document:

1. **Tenant creation**: Tenants in the document that don't exist are created
2. **Pool creation**: Pools in the document that don't exist are created
3. **Instance scaling**: Instances are created/started/stopped to match desired counts
4. **Sleep policy**: Idle instances are warmed/slept according to the sleep policy
5. **Pruning** (optional): Tenants/pools not in the document are destroyed

### Scaling Logic

- **Scale up running**: Start stopped instances first, then create new ones
- **Scale down running**: Stop excess running instances (newest first)
- **Warm/sleeping**: Managed by the sleep policy evaluator

### Idempotency

The reconcile operation is idempotent. Running it multiple times with the same desired state produces no changes after the first convergence.

## QUIC API

The desired state can be pushed to a remote agent via the QUIC API:

```bash
# Push via coordinator client
mvm coordinator push --desired desired.json --node 10.0.1.5:4433

# Or use the agent's reconcile command directly
mvm agent reconcile --desired desired.json
```

### Request Types

| Request | Description |
|---------|-------------|
| `Reconcile(DesiredState)` | Push desired state for reconciliation |
| `NodeInfo` | Query node identity and capabilities |
| `NodeStats` | Query aggregate resource usage |
| `TenantList` | List all tenants on the node |
| `InstanceList { tenant_id, pool_id? }` | List instances, optionally filtered |
| `WakeInstance { tenant_id, pool_id, instance_id }` | Urgently wake a sleeping instance |

## Validation

The agent validates the desired state before processing:

- `schema_version` must be `1`
- Tenant IDs must be non-empty
- Pool IDs must be non-empty
- Instance `vcpus` must be > 0

Invalid documents are rejected with an error response.
