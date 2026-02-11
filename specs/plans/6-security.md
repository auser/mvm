You have produced a multi-tenant Firecracker fleet architecture with:

- Coordinator + node agents
- Cluster-wide tenant networking
- Immutable Nix-built rootfs
- Sleep/wake snapshots
- vsock guest control (no SSH)
- Per-tenant drives for data/config/secrets

Upgrade the design to the MOST SECURE production-ready architecture possible
without removing any existing features.

Do not add Docker or SSH.
Do not remove Firecracker, Nix, vsock, sleep/wake, or multi-tenant networking.

This is a structural hardening update, not a feature expansion.

----------------------------------------------------------------
1) SPLIT AGENT INTO PRIVILEGED + UNPRIVILEGED COMPONENTS
----------------------------------------------------------------

Currently the node agent likely runs as root and performs:
- Firecracker launches
- tap/bridge creation
- nftables configuration
- cgroup manipulation
- drive attachment

This must be split:

A) mvm-agentd (unprivileged)
   - Runs as non-root
   - Handles:
       - coordinator communication (mTLS)
       - desired-state reconciliation logic
       - sleep policy decisions
       - quota evaluation
       - planning lifecycle transitions
   - Cannot manipulate network, cgroups, or jailer directly.

B) mvm-hostd (privileged, minimal)
   - Runs as root
   - Exposes a root-only Unix socket
   - Accepts a narrow set of RPC actions:
       - create_instance(plan)
       - stop_instance(id)
       - sleep_instance(id)
       - wake_instance(id)
       - attach_drives(id)
       - setup_network(tenantNetId)
   - Does NOT speak to coordinator.
   - Performs:
       - jailer execution
       - tap/bridge/nftables changes
       - cgroup creation
       - snapshot create/restore

The two processes communicate via a local Unix socket with strict permissions.

Document the updated architecture and module layout.

----------------------------------------------------------------
2) ENCRYPTION AT REST (DATA + SNAPSHOTS)
----------------------------------------------------------------

Add encryption for tenant storage:

- Data volumes (vdb) must use LUKS per tenant or per volume.
- Snapshot files must be encrypted at rest.
- Secrets must never be written unencrypted to persistent disk.

Key management:
- Agent retrieves per-tenant encryption keys from:
    - coordinator
    - or KMS/Vault (abstracted interface)
- Keys must not be stored in plaintext on disk.
- Keys are loaded into memory only during mount/decrypt operations.

Document:
- key lifecycle
- unlock-on-run
- lock-on-sleep/destroy behavior

----------------------------------------------------------------
3) TIGHTEN COORDINATOR API SURFACE
----------------------------------------------------------------

Coordinator must NOT allow arbitrary execution.

Agent API must be strictly declarative:

Allowed:
- ApplyDesiredState(nodeProjection)
- GetNodeStatus
- GetTenantStatus
- Optional bounded Wake(poolId, count)

Forbidden:
- Remote shell
- Arbitrary file upload
- Command execution
- Runtime configuration injection outside desired state

All coordinator-agent communication must:
- use mTLS
- verify node identity
- reject unsigned desired-state updates

Document the final API surface.

----------------------------------------------------------------
4) HARDEN INSTALL / BOOTSTRAP FLOW
----------------------------------------------------------------

Update install/bootstrap design:

- mvm-install.sh must verify SHA256 checksums of downloaded binaries.
- Prefer signed releases (cosign/minisign) if available.
- Avoid blind curl | bash in production guidance.

Node bootstrap must:
- verify /dev/kvm
- verify cgroup v2
- verify nftables
- refuse to start agent if required isolation features are missing.

Document secure bootstrap recommendations.

----------------------------------------------------------------
5) SNAPSHOT HARDENING
----------------------------------------------------------------

Snapshots contain memory dumps and must be treated as sensitive:

- Store per-tenant snapshots in isolated directories.
- Encrypt snapshot memory files.
- Restrict filesystem permissions to root-only.
- Never reuse snapshots across tenants.
- GC must securely wipe deleted snapshot files.

Add capability detection:
- If incremental snapshots unsupported → fallback to full snapshot + compression + ballooning.

----------------------------------------------------------------
6) NETWORK HARDENING
----------------------------------------------------------------

Reaffirm:

- Per-tenant bridge isolation (preferred).
- Cross-tenant traffic impossible by construction.
- Metadata endpoints tenant-scoped.
- net verify must check:
    - bridge correctness
    - subnet correctness
    - isolation rules
    - no accidental cross-bridge attachment

----------------------------------------------------------------
7) SYSTEMD SANDBOXING FOR AGENT PROCESSES
----------------------------------------------------------------

For Linux nodes:

- mvm-agentd (unprivileged) must run with:
    - NoNewPrivileges=true
    - ProtectSystem=strict
    - ProtectHome=true
    - PrivateTmp=true
    - RestrictSUIDSGID=true
    - MemoryDenyWriteExecute=true

- mvm-hostd must run with minimal capabilities required:
    - CAP_NET_ADMIN
    - CAP_SYS_ADMIN (if absolutely required)
    - drop everything else

Document required capabilities explicitly.

----------------------------------------------------------------
8) UPDATE PLAN OUTPUT
----------------------------------------------------------------

Revise the architecture document to reflect:

- Privilege separation
- Encryption at rest
- Hardened bootstrap
- Narrowed API surface
- Snapshot encryption
- Systemd hardening

Do not add unrelated features.
Preserve existing state machine and multi-tenant model.
