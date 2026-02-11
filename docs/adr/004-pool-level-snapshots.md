# ADR-004: Pool-Level Base Snapshots

## Status
Accepted

## Context

Firecracker supports VM snapshots for fast restore. Snapshot strategy options:

1. **Per-instance snapshots only**: Each instance manages its own full snapshot
2. **Pool-level base + instance delta**: Shared base snapshot per pool, instance-specific deltas on sleep
3. **No snapshots**: Always cold-boot from rootfs

## Decision

Use pool-level base snapshots shared across all instances in a pool. When an instance sleeps, it takes a delta snapshot (memory diff + dirty blocks). When it wakes, it restores from base + delta.

## Rationale

- **Disk efficiency**: Base snapshot stored once per pool, not per instance. For a pool of 16 instances, this saves ~15x the base snapshot size
- **Fast provisioning**: New instances restore from base snapshot (~5ms) instead of cold booting (~125ms)
- **State preservation**: Instance-specific state (in-memory data, open connections) is captured in the delta
- **Build integration**: Base snapshot is created during `pool build`, capturing the post-boot steady state

## Consequences

- Base snapshot must be regenerated on every pool build (new rootfs = new base)
- Instance deltas are invalidated if the base snapshot changes (sleeping instances must cold-boot on new revision)
- Base snapshot consumes disk (~256MB-1GB depending on memory allocation)
- Delta snapshots grow with instance runtime (more dirty pages = larger delta)
- Snapshot compatibility tied to Firecracker version (version upgrades may invalidate snapshots)
