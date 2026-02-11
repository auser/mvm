# Architecture

## Overview

mvm is a Rust CLI that manages multi-tenant Firecracker microVM fleets. On macOS it runs through Lima; on Linux it can operate directly against /dev/kvm.

```
macOS / Linux Host
  └── mvm CLI (Rust)
        └── Lima VM (Ubuntu, optional on Linux)
              └── Firecracker microVMs (one per instance)
```

## Object Model

```
Tenant (security/quota/network boundary)
  ├── WorkerPool A (flake ref + profile + desired counts)
  │     ├── Instance i-a1b2c3d4 (Running)
  │     ├── Instance i-e5f6a7b8 (Warm)
  │     └── Instance i-c9d0e1f2 (Sleeping)
  └── WorkerPool B
        └── Instance i-...
```

### Tenant

A security, isolation, and policy boundary. NOT a runtime entity.

Owns:
- Quotas (max vCPUs, memory, running/warm counts, disk)
- Network allocation (coordinator-assigned subnet + bridge)
- Secrets (per-tenant, rotatable)
- Audit log (append-only lifecycle events)
- SSH keypair (Ed25519, per-tenant)

### WorkerPool

Defines a homogeneous group of instances within a tenant. Has desired counts but no runtime state.

Owns:
- Nix flake reference + profile (minimal, python, etc.)
- Instance resource template (vCPUs, memory, data disk size)
- Runtime policy (min_running_seconds, min_warm_seconds, drain/graceful timeouts)
- Desired counts (running, warm, sleeping)
- Build history (revisions with artifact paths)
- Shared artifacts (kernel, rootfs, base Firecracker config)
- Base snapshot (shared across all instances in the pool)

### Instance

An individual Firecracker microVM. The ONLY entity with runtime state.

Owns:
- State (Created, Ready, Running, Warm, Sleeping, Stopped)
- Network identity (TAP device, MAC, guest IP within tenant subnet)
- Firecracker process (PID, socket, config)
- Data disk (persistent ext4) + secrets disk (recreated per run) + config disk (non-secret metadata)
- Delta snapshot (instance-specific memory state)
- Idle metrics (last work timestamp, CPU average, heartbeat)
- Lifecycle timestamps (entered_running_at, entered_warm_at, last_busy_at)

## Instance State Machine

```
         create         pool build        start
Absent ────────> Created ──────────> Ready ────────> Running
                                      ^                |  |
                                      |                |  | warm
                                      |           stop |  v
                                      |                |  Warm
                                      |                |  |
                                      |                |  | sleep
                                      |           stop |  v
                                      |                |  Sleeping
                                      |                |  |
                                      |                v  | wake
                                      |             Stopped<-+
                                      |                |
                                      |    rebuild     |
                                      +----------------+
```

Valid transitions (enforced in `instance/state.rs`):

| From | To | Trigger |
|------|----|---------|
| Created | Ready | Pool build completes |
| Ready | Running | Start |
| Running | Warm | Pause vCPUs |
| Running | Stopped | Stop |
| Warm | Sleeping | Snapshot + shutdown |
| Warm | Running | Resume vCPUs |
| Warm | Stopped | Stop |
| Sleeping | Running | Wake (restore from snapshot) |
| Sleeping | Stopped | Stop (discard snapshot) |
| Stopped | Running | Fresh boot |
| Ready | Ready | Rebuild |
| Any | Destroyed | Destroy |

Invalid transitions fail loudly with an error message.

## Module Map

```
src/
    main.rs                      # CLI dispatch (clap subcommands)
    agent.rs                     # Reconcile loop + QUIC daemon (tokio)
    node.rs                      # Node identity, info, stats
    infra/                       # Host/VM infrastructure (UNCHANGED from dev mode)
        mod.rs
        config.rs                # Constants (VM_NAME, FC_VERSION, ARCH, network)
        shell.rs                 # run_host, run_in_vm, run_in_vm_stdout, replace_process
        bootstrap.rs             # Homebrew, Lima installation
        upgrade.rs               # Self-update
        ui.rs                    # Colored output, spinners, confirmations
    vm/
        mod.rs
        # Dev mode (UNCHANGED)
        microvm.rs               # Single-VM lifecycle (start, stop, ssh)
        firecracker.rs           # FC binary install, asset download
        network.rs               # Dev-mode TAP/NAT (172.16.0.x)
        lima.rs                  # Lima VM lifecycle
        image.rs                 # Mvmfile.toml build pipeline
        # Multi-tenant model
        naming.rs                # ID validation, instance_id generation, TAP naming
        bridge.rs                # Per-tenant bridge create/destroy/verify
        tenant/
            mod.rs
            config.rs            # TenantConfig, TenantQuota, TenantNet
            lifecycle.rs         # tenant_create, tenant_destroy, tenant_list, tenant_info
            quota.rs             # compute_tenant_usage, check_quota
            secrets.rs           # secrets_set, secrets_rotate
        pool/
            mod.rs
            config.rs            # PoolSpec, DesiredCounts, BuildRevision, InstanceResources
            lifecycle.rs         # pool_create, pool_destroy, pool_scale, pool_update
            build.rs             # Ephemeral builder microVM (Nix build inside FC)
            artifacts.rs         # Revision management, symlinks, rollback
        instance/
            mod.rs
            state.rs             # InstanceStatus enum, InstanceState, validate_transition
            lifecycle.rs         # Unified lifecycle API (the single entry point)
            net.rs               # Per-instance TAP setup/teardown within tenant bridge
            fc_config.rs         # Generate Firecracker config JSON
            disk.rs              # Data disk + secrets disk management
            snapshot.rs          # Base (pool-level) + delta (instance-level) snapshots
    security/
        mod.rs
        jailer.rs                # Firecracker jailer integration + fallback
        cgroups.rs               # cgroup v2 per-instance + per-tenant aggregate
        seccomp.rs               # Seccomp profile selection (baseline/strict)
        audit.rs                 # Append-only per-tenant audit log
        metadata.rs              # Tenant-scoped metadata endpoint
        certs.rs                 # mTLS certificate generation (rcgen)
        encryption.rs            # LUKS disk encryption
        keystore.rs              # Key management
        signing.rs               # Ed25519 signed state verification
        snapshot_crypto.rs       # AES-256-GCM snapshot encryption
        attestation.rs           # Node attestation hook
    sleep/
        mod.rs
        policy.rs                # Sleep heuristics, minimum runtime enforcement, eligibility checks
        metrics.rs               # Per-instance idle metrics collection
    worker/
        mod.rs
        hooks.rs                 # Guest worker lifecycle signals (ready/idle/busy)
        vsock.rs                 # Vsock guest agent client (sleep-prep drain, wake, status)
```

## Filesystem Layout

```
/var/lib/mvm/
    node.json                              # Node identity + resource limits
    builder/                               # Ephemeral build microVM workspace
        run/<build-id>/
    tenants/
        <tenant_id>/
            tenant.json                    # TenantConfig (quotas, network)
            secrets.json                   # Tenant-scoped secrets
            audit.log                      # Per-tenant append-only audit
            ssh_key, ssh_key.pub           # Per-tenant Ed25519 keypair
            pools/
                <pool_id>/
                    pool.json              # PoolSpec
                    build_history.json     # Last N BuildRevisions
                    artifacts/
                        current -> revisions/<hash>/
                        revisions/<hash>/
                            vmlinux
                            rootfs.ext4
                            fc-base.json
                    snapshots/
                        base/              # Shared base snapshot (pool-level)
                            vmstate.bin
                            mem.bin
                            meta.json
                    instances/
                        <instance_id>/
                            instance.json  # InstanceState
                            runtime/       # fc.json, socket, PID, v.sock, logs
                            volumes/       # data.ext4, secrets.ext4, config.ext4
                            snapshots/
                                delta/     # Instance-specific delta snapshot
                            jail/          # Jailer chroot
```

## Key Design Decisions

1. **Firecracker-only execution** -- no Docker, no containers. Builds run inside ephemeral Firecracker VMs using Nix.

2. **Coordinator owns network allocation** -- tenant subnets come from a cluster-wide CIDR (10.240.0.0/12). Agents never derive IPs locally.

3. **Per-tenant bridges** -- network isolation is structural (separate L2 domains), not rule-based.

4. **Pool-level base snapshots** -- all instances in a pool share the same post-boot snapshot, significantly reducing storage for large fleets.

5. **Instance-level delta snapshots** -- only memory dirtied since base is captured per-instance during sleep.

6. **Single lifecycle API** -- all operations (CLI, agent, sleep policy) go through `instance/lifecycle.rs`. No direct Firecracker manipulation elsewhere.

7. **Dev mode isolation** -- dev commands (`mvm start/stop/ssh/dev`) use a completely separate code path and never interact with tenant state.

## Build Pipeline

Guest images are built reproducibly using Nix flakes inside ephemeral Firecracker VMs:

```
mvm pool build acme/workers
  1. Load pool spec (flake ref + profile)
  2. Boot ephemeral builder microVM (Nix + git, on tenant's bridge)
  3. Execute `nix build` inside builder
  4. Copy artifacts (kernel, rootfs, fc-base.json) to pool artifacts dir
  5. Shut down builder, clean up
  6. Record BuildRevision in build_history.json
```

Builder microVMs are stateless, disposable, and uniquely named per build invocation.
