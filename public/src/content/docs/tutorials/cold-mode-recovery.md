---
title: Cold-mode recovery
description: Pause, save, restore, and wake sandboxes from backend-specific snapshot state.
---

Cold mode lets a sandbox stop consuming a running VM while keeping recoverable state.

## Firecracker sealed pause/resume

```sh
cargo run -- pause agent-sandbox
cargo run -- snapshot ls
cargo run -- resume agent-sandbox
```

Firecracker pause/resume writes sealed instance state and verifies it before resume. This is the local `mvm` primitive.

## Vz machine-state save/restore

```sh
cargo run -- snapshot save agent-sandbox --path /tmp/agent-sandbox.vzsnap --hypervisor vz
cargo run -- snapshot restore agent-sandbox --path /tmp/agent-sandbox.vzsnap --hypervisor vz
```

Vz snapshots are opaque machine-state files. `mvm` records the saved file hash in the audit chain when the launch plan and host signer are available, and restore records whether the file matched the prior chain entry.

## Security checklist

- Snapshot artifacts may contain guest memory, files, credentials, tokens, and browser/session state.
- Restore support is backend-specific.
- A restore is not a security boundary; it is a lifecycle transition.
- Deleting a snapshot is not the same as proving physical erasure.
- Published latency numbers must state backend, artifact, and readiness boundary.
