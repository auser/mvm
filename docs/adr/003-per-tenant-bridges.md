# ADR-003: Per-Tenant Network Bridges

## Status
Accepted

## Context

Multi-tenant microVMs need network isolation. Options:

1. **Single shared bridge** with iptables rules between tenants
2. **Per-tenant Linux bridges** (separate L2 domains)
3. **VXLAN overlays** for cross-host connectivity
4. **macvlan/ipvlan** sub-interfaces

## Decision

Each tenant gets a dedicated Linux bridge (`br-tenant-<net_id>`). Instances connect via TAP devices to their tenant's bridge. Cross-tenant traffic is denied by construction (separate L2 domains).

## Rationale

- **Isolation by construction**: No iptables rules needed for tenant isolation -- they simply can't reach each other at L2
- **Simplicity**: Standard Linux bridge, no overlay complexity
- **Performance**: Native bridging, no encapsulation overhead
- **Debuggability**: Standard `ip link`, `bridge`, `tcpdump` tools work as expected
- **Within-tenant communication**: Instances in the same tenant can communicate directly (same bridge)

## Consequences

- Each tenant consumes a bridge interface (limit ~4096 via net_id)
- Cross-tenant communication requires explicit routing through the host
- Bridge creation/destruction adds to tenant lifecycle latency (~10ms)
- No built-in cross-host L2 connectivity (future: VXLAN or WireGuard overlay)
