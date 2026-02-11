You have implemented the 90% secure baseline for the mvm multi-tenant Firecracker fleet:

- Per-tenant bridges + cluster-wide subnet allocation
- Firecracker jailer always-on
- cgroup v2 limits per instance
- No SSH anywhere (vsock control only)
- Encrypted persistent data volumes (LUKS)
- Declarative desired-state-only agent API
- mTLS (or token) coordinator communication
- Systemd sandboxing for the node agent
- Strict per-tenant snapshot scoping (unencrypted)

Now implement the FINAL 10% security hardening.

This phase is about privilege separation, cryptographic integrity, and memory/disk confidentiality.

Do not remove features.
Do not simplify architecture.
Do not introduce Docker or SSH.
Preserve the existing state machine and multi-tenant model.

----------------------------------------------------------------
1) PRIVILEGE SEPARATION (MANDATORY)
----------------------------------------------------------------

Split the node agent into two components:

A) mvm-agentd (unprivileged)
   - Runs as non-root user
   - Handles:
       - Coordinator communication (mTLS)
       - Desired-state reconciliation logic
       - Sleep policy decisions
       - Quota enforcement logic
       - Planning instance transitions
   - Cannot manipulate:
       - tap/bridge
       - nftables
       - cgroups
       - jailer
       - snapshot files

B) mvm-hostd (minimal privileged executor)
   - Runs as root
   - Exposes a root-owned Unix domain socket
   - Accepts a minimal RPC surface:
       - create_instance(plan)
       - stop_instance(id)
       - sleep_instance(id)
       - wake_instance(id)
       - attach_drives(id)
       - setup_network(tenantNetId)
       - destroy_instance(id)
   - Does not communicate with the coordinator directly.
   - Does not contain reconciliation logic.
   - Performs all privileged operations:
       - jailer launch
       - cgroup creation
       - nftables manipulation
       - tap device management
       - snapshot create/restore
       - LUKS unlock/lock

The unprivileged agent must never perform privileged syscalls directly.

Document and implement the new module layout.

----------------------------------------------------------------
2) SNAPSHOT ENCRYPTION (MANDATORY)
----------------------------------------------------------------

Snapshots contain memory and must be treated as sensitive.

Implement encryption-at-rest for snapshot files:

- Encrypt snapshot memory files (mem.bin / delta) per tenant.
- Encryption must use:
    - AES-256-GCM or equivalent authenticated encryption.
- Keys:
    - Per-tenant snapshot encryption key.
    - Loaded only into memory during encrypt/decrypt.
    - Never persisted in plaintext.
- Keys must be sourced from:
    - coordinator-provided encrypted blob OR
    - external KMS/Vault abstraction layer.

Snapshot restore flow:
- Decrypt snapshot into memory buffer or temp file.
- Immediately wipe decrypted temp data after restore.
- Ensure strict filesystem permissions (root-only).

Document key lifecycle and failure handling.

----------------------------------------------------------------
3) SIGNED DESIRED STATE (MANDATORY)
----------------------------------------------------------------

Prevent coordinator compromise from silently pushing malicious state.

Implement signed desired-state enforcement:

- Coordinator signs desired state payloads.
- Agent verifies signature before applying.
- Use:
    - Ed25519 signatures OR equivalent.
- Store trusted coordinator public key locally on node.
- Reject unsigned or invalid signatures.

Signed data must include:
- tenant IDs
- pool IDs
- revisions (flake ref + lock hash)
- subnet allocations
- resource limits
- desired instance counts

Agent must refuse to reconcile unsigned desired state.

----------------------------------------------------------------
4) CAPABILITY MINIMIZATION FOR mvm-hostd
----------------------------------------------------------------

Restrict mvm-hostd Linux capabilities to the absolute minimum:

- CAP_NET_ADMIN
- CAP_SYS_ADMIN (only if strictly required; document why)
- Drop all other capabilities explicitly.

Add:
- seccomp filter for mvm-hostd limiting syscalls.
- systemd unit with:
    - CapabilityBoundingSet=
    - PrivateTmp=true
    - ProtectSystem=strict
    - ProtectHome=true
    - NoNewPrivileges=true

Document exact required capabilities.

----------------------------------------------------------------
5) MEMORY HYGIENE
----------------------------------------------------------------

Prevent key material and secrets from lingering:

- Zero memory buffers after:
    - LUKS key usage
    - Snapshot key usage
    - Secrets handling
- Avoid logging any sensitive material.
- Use mlock for in-memory key buffers if feasible.

----------------------------------------------------------------
6) OPTIONAL BUT STRONGLY RECOMMENDED
----------------------------------------------------------------

A) Secure deletion
- When deleting snapshots or volumes:
    - Overwrite before unlink (best effort)
    - Or rely on encrypted filesystem layer

B) Node attestation (future-ready hook)
- Leave extension point for TPM/remote attestation of node identity.
- Not required to fully implement, but architecturally allow it.

----------------------------------------------------------------
7) UPDATE DOCUMENTATION
----------------------------------------------------------------

Revise the architecture documentation to reflect:

- Privilege separation model
- Cryptographic guarantees
- Signed desired-state enforcement
- Snapshot encryption
- Threat model assumptions

Clearly distinguish:
- 90% baseline
- 100% hardened mode

----------------------------------------------------------------
DELIVERABLES
----------------------------------------------------------------

- New module structure (agentd + hostd)
- Snapshot encryption implementation
- Desired-state signature verification
- Updated systemd units with capability bounding
- Updated install/bootstrap flow if needed
- Documentation reflecting final hardened architecture

Code must compile.
Dev mode must remain functional.
