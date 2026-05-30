---
title: Snapshots and cold mode
description: Pause a microVM into sealed state, save backend snapshots, and restore later with explicit integrity checks.
---

Cold mode means a workload is not currently consuming a running guest, but it has recoverable state. In `mvm`, that state is represented by backend-specific snapshot artifacts.

## Current snapshot paths

| Path | Backend | Commands | Status |
| --- | --- | --- | --- |
| Sealed instance snapshot | Firecracker | `mvmctl pause`, `mvmctl resume`, `mvmctl snapshot ls`, `mvmctl snapshot rm` | Shipped for the Firecracker snapshot path. |
| Machine-state file | Vz | `mvmctl snapshot save`, `mvmctl snapshot restore` | Shipped for Vz snapshot save/restore on supported macOS versions. |
| Pool instance sleep | Firecracker pool lifecycle | internal pool lifecycle APIs | Implemented in pool lifecycle; public docs should stay tied to the CLI surface. |

Other backends may support stop/start without machine-state recovery. Do not assume snapshot support unless the active backend reports it.

## Firecracker pause and resume

Pause a running VM:

```sh
mvmctl pause agent-sandbox
```

`mvmctl pause` asks Firecracker to write `vmstate.bin` and `mem.bin` under the VM's instance snapshot directory, seals the sidecar with an epoch-bound HMAC envelope, and marks the VM as paused in the local registry.

Resume it:

```sh
mvmctl resume agent-sandbox
```

`mvmctl resume` verifies the sealed envelope before loading the state and clearing the paused flag. Replay of older sealed snapshots is refused by the epoch binding.

List and remove local sealed snapshots:

```sh
mvmctl snapshot ls
mvmctl snapshot rm agent-sandbox
```

## Vz save and restore

On supported macOS hosts, Vz snapshots are file-based:

```sh
mvmctl snapshot save agent-sandbox --path /tmp/agent-sandbox.vzsnap --hypervisor vz
mvmctl snapshot restore agent-sandbox --path /tmp/agent-sandbox.vzsnap --hypervisor vz
```

The save path writes an opaque Vz machine-state file and records its SHA-256 in the audit chain when the persisted launch plan and host signer are available. Restore re-hashes the file and records whether the current bytes match the prior chain entry.

The restore proceeds even when the snapshot is not in the local chain or the hash differs, because operators may transfer snapshots between hosts. The audit entry labels that result so the operator can review it.

## Security implications

- Snapshot files contain guest memory and runtime state. Treat them as sensitive.
- Restore integrity is backend-specific: Firecracker uses the sealed instance envelope; Vz uses audit-chain hash comparison.
- Deleting a snapshot removes the recovery artifact but does not by itself prove storage-level erasure.
- Snapshots can preserve credentials or derived tokens that existed inside the guest at snapshot time.

## Docs rule

When writing examples, name the backend. "Snapshot restore" is not a universal property of every `mvm` backend, and cold-mode behavior should not be used as a latency claim unless the benchmark states the backend, artifact, and readiness boundary.
