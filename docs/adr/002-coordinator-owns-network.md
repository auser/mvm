# ADR-002: Coordinator Owns Network Allocation

## Status
Accepted

## Context

In a multi-node fleet, each tenant needs a unique subnet. Two approaches:

1. **Node-local allocation**: Each node independently assigns subnets from a local pool
2. **Coordinator-assigned**: A central coordinator assigns subnets and passes them to agents

## Decision

The coordinator owns all network allocation. Agents never derive or assign IP addresses independently. Network identity (subnet, net_id, gateway) is always provided in the desired state document.

## Rationale

- **No conflicts**: Central allocation prevents subnet collisions across nodes
- **Deterministic**: The same tenant always gets the same subnet, regardless of which node
- **Mobility**: Tenants can be migrated between nodes without IP changes
- **Simplicity**: Agents don't need a distributed consensus mechanism for IP allocation
- **Auditability**: Network assignments are tracked in a single place

## Consequences

- Agents cannot create tenants without coordinator-provided network config
- The coordinator is a single point of configuration (not a runtime dependency -- agents cache the desired state)
- Network config must be included in every desired state document
- If the coordinator is unavailable, agents continue operating with their last-known desired state
