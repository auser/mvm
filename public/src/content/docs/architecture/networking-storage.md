---
title: Networking and storage
description: How egress, ports, files, volumes, and snapshots fit the mvm model.
---

## Networking

Local runtime networking is policy-shaped. Examples should start from deny-by-default and add only the destinations or ports the workload needs.

## Ports

Port forwarding is an explicit operation. Treat service exposure as part of the workload contract: name the guest port, host binding, protocol, and expected readiness behavior.

## Files

File operations cross the host/guest boundary and need path checks. SDK examples should avoid broad host mounts and should explain whether data is copied, mounted, generated during build, or persisted in a volume.

## Volumes

Volumes are stateful. They need explicit lifecycle, ownership, encryption-at-rest posture, and cleanup semantics.

## Snapshots and cold mode

Snapshots preserve machine state. They can contain sensitive data, process memory, generated files, and credentials that were present in the guest. Restore flows must name backend support and integrity evidence.
