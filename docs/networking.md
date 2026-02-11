# Networking

## Overview

mvm uses **cluster-wide, coordinator-assigned** per-tenant networking. Each tenant gets a dedicated Linux bridge on every host where it has instances, with the same subnet everywhere.

## Design Principles

1. **Coordinator owns allocation** -- the coordinator is the only component that assigns tenant subnets
2. **Agents consume, never derive** -- agents receive subnet assignments in the desired state and apply them verbatim
3. **Same tenant, same subnet, every host** -- a tenant's network identity is stable cluster-wide
4. **Isolation by construction** -- separate bridges are separate L2 domains; no firewall rules needed for cross-tenant deny

## Cluster CIDR

```
Cluster CIDR:  10.240.0.0/12   (10.240.0.0 - 10.255.255.255)
Per-tenant:    /24 default      (252 usable instance IPs per host)
```

The coordinator allocates subnets from this range and persists them in its tenant registry. Each tenant gets a `tenant_net_id` (cluster-unique integer, 0..4095) and an `ipv4_subnet` (e.g. `10.240.3.0/24`).

Example allocations:

| Tenant | tenant_net_id | Subnet | Gateway |
|--------|--------------|--------|---------|
| acme | 3 | 10.240.3.0/24 | 10.240.3.1 |
| beta | 17 | 10.240.17.0/24 | 10.240.17.1 |
| gamma | 200 | 10.240.200.0/24 | 10.240.200.1 |

`tenant_net_id` values are never reused, even after tenant deletion.

## Per-Host Bridge

On each host where a tenant has instances, the agent creates a Linux bridge:

```
Bridge:   br-tenant-<tenant_net_id>      (e.g. br-tenant-3)
Gateway:  first usable IP in subnet      (e.g. 10.240.3.1/24)
```

Bridge names are max 14 characters (`br-tenant-4095`), within the 15-character Linux limit.

## IP Allocation Within a Tenant

```
.1        bridge gateway (NAT egress point)
.2        reserved (builder microVM)
.3-.254   instance IPs (allocated sequentially by the local agent)
```

Instance IPs are host-local offsets. The agent assigns them sequentially starting from `.3`. These IPs are stored in `InstanceNet` but are NOT communicated back to the coordinator -- the coordinator does not track individual instance IDs or IPs.

## TAP Device Naming

Each instance gets a TAP device attached to its tenant's bridge:

```
Format:  tn<net_id>i<ip_offset>
```

Examples: `tn3i5`, `tn17i3`, `tn200i100`

Maximum 12 characters, well within the 15-character Linux interface name limit.

## Traffic Rules

### Within-Tenant (ALLOWED)

Instances on the same tenant bridge communicate freely at L2/L3. No rules needed -- they share a broadcast domain.

### Cross-Tenant (DENIED)

Separate bridges are separate L2 domains. An instance on `br-tenant-3` physically cannot reach `br-tenant-17`. This is isolation by construction, not by firewall rule.

### Egress (NAT)

Each tenant bridge gets iptables MASQUERADE for outbound traffic:

```bash
# Global (once)
echo 1 > /proc/sys/net/ipv4/ip_forward

# Per-tenant bridge setup
ip link add br-tenant-<N> type bridge
ip addr add <gateway_ip>/<cidr> dev br-tenant-<N>
ip link set br-tenant-<N> up

# Per-tenant NAT
iptables -t nat -A POSTROUTING -s <subnet> ! -o br-tenant-<N> -j MASQUERADE
iptables -A FORWARD -i br-tenant-<N> ! -o br-tenant-<N> -j ACCEPT
iptables -A FORWARD ! -i br-tenant-<N> -o br-tenant-<N> -m state --state RELATED,ESTABLISHED -j ACCEPT
```

All rules are applied idempotently.

## Sleep/Wake Network Preservation

When an instance sleeps:
- Its `InstanceNet` (TAP device name, MAC, guest IP, gateway) is persisted to `instance.json`
- The TAP device may be torn down to free resources

When an instance wakes:
- The same TAP device is recreated with the same name and MAC
- It is reattached to the same tenant bridge
- The guest resumes with its original IP -- no DHCP, no reconfiguration

Network identity is fully stable across sleep/wake cycles.

## Desired State Network Block

The coordinator includes network allocation in every tenant's desired state:

```json
{
  "tenant_id": "acme",
  "network": {
    "tenant_net_id": 3,
    "ipv4_subnet": "10.240.3.0/24"
  },
  ...
}
```

The agent REJECTS any tenant entry that is missing the `network` block. This ensures agents never need to compute or derive network allocations.

## Verification

`mvm net verify` checks:

1. Each tenant bridge exists and has the correct subnet
2. Bridge gateway IP matches `TenantNet.gateway_ip`
3. iptables NAT and forward rules exist for each tenant
4. No cross-tenant bridge connectivity
5. Subnet matches coordinator allocation in `tenant.json`
6. Within-tenant instances can ping each other (optional active probe)

Output is human-readable by default, `--json` for machine consumption.

## Data Model

### TenantNet (coordinator-assigned, stored in tenant.json)

```rust
pub struct TenantNet {
    pub tenant_net_id: u16,     // Cluster-unique, 0..4095
    pub ipv4_subnet: String,    // Coordinator-assigned CIDR
    pub gateway_ip: String,     // First usable IP
    pub bridge_name: String,    // "br-tenant-<tenant_net_id>"
}
```

### InstanceNet (agent-assigned, stored in instance.json)

```rust
pub struct InstanceNet {
    pub tap_dev: String,        // "tn<net_id>i<offset>"
    pub mac: String,            // Deterministic from tenant_net_id + offset
    pub guest_ip: String,       // From coordinator subnet
    pub gateway_ip: String,     // Tenant gateway
    pub cidr: u8,               // From subnet mask
}
```
