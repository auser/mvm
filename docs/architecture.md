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

## Workspace Structure

mvm is organized as a Cargo workspace with 7 specialized crates plus a root facade:

```
mvm/
├── src/
│   ├── lib.rs          # Facade: re-exports all workspace crates
│   └── main.rs         # Binary entry: calls mvm_cli::run()
├── crates/
│   ├── mvm-core/       # Pure types, IDs, config, protocol, signing (no runtime deps)
│   ├── mvm-guest/      # Vsock protocol, integration manifest
│   ├── mvm-build/      # Nix builder pipeline
│   ├── mvm-runtime/    # Shell execution, security, VM lifecycle, bridge, pool/tenant/instance
│   ├── mvm-agent/      # Reconcile engine, coordinator client, sleep policy
│   ├── mvm-coordinator/# Gateway load-balancer, TCP proxy, wake manager
│   └── mvm-cli/        # Clap CLI, UI, bootstrap, upgrade
├── resources/          # Lima template, builder scripts (Tera templates)
├── deploy/systemd/     # Service files (mvm-agent, mvm-agentd, mvm-hostd)
└── nix/openclaw/       # OpenClaw template (flake, roles, profiles)
```

### Dependency Graph

```
mvm-core (foundation, no mvm deps)
├── mvm-guest (core)
├── mvm-build (core, guest)
├── mvm-runtime (core, guest, build)
├── mvm-agent (core, runtime, build, guest)
├── mvm-coordinator (core, runtime)
└── mvm-cli (core, agent, runtime, coordinator, build)
```

Changes to `mvm-core` ripple across all crates. Changes to `mvm-cli` affect nothing else.

### Binaries

| Binary | Source | Purpose |
|--------|--------|---------|
| `mvm` | Root package (`src/main.rs`) | CLI, delegates to `mvm_cli::run()` |
| `mvm-hostd` | `mvm-runtime` | Privileged daemon for instance operations |
| `mvm-builder-agent` | `mvm-guest` | Guest-side agent in builder VMs |

### Crate Details

**mvm-core** — Pure types and protocol definitions. No runtime dependencies.
- `tenant.rs` — TenantConfig, TenantQuota, TenantNet, filesystem paths
- `pool.rs` — Role, PoolSpec, DesiredCounts, InstanceResources, RuntimePolicy, BuildRevision
- `instance.rs` — InstanceStatus, InstanceState, InstanceNet, validate_transition()
- `agent.rs` — Desired state schema, reconcile protocol, QUIC frame types
- `protocol.rs` — Hostd IPC types (Unix socket frames)
- `build_env.rs` — BuildEnvironment trait (abstraction for build pipeline)
- `signing.rs` — Ed25519 signed payload types
- `routing.rs` — Gateway routing table logic
- `naming.rs` — ID validation and naming conventions
- `template.rs` — Template metadata types

**mvm-runtime** — All runtime operations: shell execution, VM lifecycle, security.
- `shell.rs` — `run_in_vm()`, `run_host()`, `replace_process()`
- `vm/lima.rs`, `vm/firecracker.rs`, `vm/microvm.rs` — Dev mode lifecycle
- `vm/bridge.rs` — Per-tenant bridge create/destroy/verify
- `vm/tenant/` — Tenant lifecycle, quotas, secrets
- `vm/pool/` — Pool lifecycle, artifact management
- `vm/instance/` — Instance lifecycle, networking, FC config, disks, snapshots, health
- `vm/template/` — Template registry and lifecycle
- `security/` — Jailer, cgroups, seccomp, audit, LUKS, certs, signing, snapshot crypto, attestation
- `hostd/` — Privilege separation server (protocol, server, client)
- `sleep/` — Sleep metrics
- `worker/` — Guest worker hooks, vsock client
- `build_env.rs` — RuntimeBuildEnv (implements BuildEnvironment trait)

**mvm-build** — Nix builder pipeline for reproducible guest images.
- `build.rs` — Main pool build API (`pool_build`, `pool_build_with_opts`)
- `orchestrator.rs` — Build orchestration (backend selection, artifact extraction)
- `vsock_builder.rs` — Vsock-based builder communication
- `scripts.rs` — Tera script rendering for builder VMs
- `cache.rs` — Build artifact caching
- `template_reuse.rs` — Template-based base image reuse

**mvm-guest** — Vsock protocol and guest-side integration definitions.
- `vsock.rs` — GuestRequest/GuestResponse types, frame protocol
- `integrations.rs` — OpenClaw integration manifest and state model
- `builder_agent.rs` — Guest-side builder agent logic

**mvm-agent** — Reconciliation engine and fleet coordination.
- `agent.rs` — Reconcile loop, QUIC server, rate limiting
- `hostd.rs` — Hostd client wrapper
- `node.rs` — Node identity and stats collection
- `sleep/policy.rs` — Sleep policy evaluation
- `templates.rs` — Built-in deployment templates (e.g., "openclaw")

**mvm-coordinator** — Gateway load-balancer and wake orchestration.
- `server.rs` — Coordinator QUIC server
- `proxy.rs` — TCP proxy implementation
- `routing.rs` — Route resolution
- `wake.rs` — Wake request handling
- `idle.rs` — Idle instance tracking
- `state.rs` — State management (in-memory or Etcd)
- `health.rs` — Health check system

**mvm-cli** — User-facing CLI and environment setup.
- `commands.rs` — Main entry point and Clap command routing
- `bootstrap.rs` — Homebrew + Lima installation
- `template_cmd.rs` — Template management commands
- `dev_cluster.rs` — Local dev cluster operations
- `doctor.rs` — System diagnostics
- `display.rs` — Formatted output rendering
- `upgrade.rs` — Self-update

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

8. **Templates are reusable** -- build once, share across tenants. Tenant customization happens at runtime via mounted volumes, not at build time.

## Multi-Tenant Template Model

The core concept: **a single template image serves many tenants**. Tenants don't rebuild images — they reuse shared templates and get customized instances via runtime-mounted volumes.

### How It Works

A **template** is a pre-built base image (kernel + rootfs + FC config) produced by a Nix flake. Once built, it can be assigned to any tenant's pool without rebuilding:

```
Template (build once)          Tenant Pools (reuse many times)
┌─────────────────┐      ┌─ acme/workers    (mounts acme secrets/config)
│ openclaw-worker │──────┤─ beta/workers    (mounts beta secrets/config)
│ kernel + rootfs │      └─ gamma/workers   (mounts gamma secrets/config)
└─────────────────┘
```

What makes each tenant's instance unique is NOT the image — it's the volumes mounted at boot:

| Volume | Mount | Contents | Trust Boundary |
|--------|-------|----------|----------------|
| **Secrets drive** | `/run/secrets` | API keys, credentials, tokens | Encrypted tmpfs, recreated per run |
| **Config drive** | `/etc/mvm-config` | Routing tables, integration manifest, identity | Read-only ext4 |
| **Data disk** | `/data` | Persistent tenant state, integration data | Optional LUKS encryption |

This enables fast tenant onboarding (no build required), resource efficiency (shared images), and consistent deployments (all tenants run identical base images).

### Multi-Pool Templates

Some workloads need multiple microVM types working together. For example, **OpenClaw** requires:

| Pool | Role | Purpose |
|------|------|---------|
| `gateways` | gateway | Routes inbound traffic, wakes sleeping workers |
| `workers` | worker | Executes integrations (WhatsApp, Telegram, etc.) |

The `mvm new openclaw myapp` command creates both pools for a tenant automatically.

### Request Flow (Sleep/Wake)

When a request arrives for a sleeping tenant:

```
Client request
  → Coordinator (always running, routes by port)
    → Wakes gateway instance (~200ms snapshot restore)
      → Gateway reads routing table from config drive
        → Wakes worker instances as needed
          → Request processed, response returned
```

From the user's perspective, their setup never changed. The platform optimizes resource usage by sleeping idle tenants and waking them on demand.

### Template Lifecycle

```
1. Create template definition (Nix flake + roles + profiles)
2. Build artifacts: mvm template build openclaw-worker
3. Push to registry (optional, multi-node): mvm template push openclaw-worker
4. Create tenant pools referencing template: mvm pool create acme/workers --template openclaw-worker
5. Instances boot from shared artifacts, mount tenant-specific volumes
```

Templates vs Pools vs Instances:

| Concept | Scope | Tied to Tenant? | What It Owns |
|---------|-------|-----------------|--------------|
| **Template** | Global | No | Reusable base image (kernel + rootfs) |
| **Pool** | Tenant | Yes | Desired counts, resource limits, runtime policy |
| **Instance** | Pool | Yes | State machine, network identity, volumes, PID |

See [roles.md](roles.md) for role-specific VM profiles and [integrations.md](integrations.md) for the OpenClaw integration lifecycle.

## Build Pipeline

Guest images are built reproducibly using Nix flakes. Three build backends are supported:

1. **Host mode** (default) -- Nix build runs directly on the host/Lima VM
2. **Vsock mode** -- Nix build runs inside an ephemeral Firecracker VM, artifacts extracted via vsock
3. **SSH mode** -- Legacy; Nix build inside FC VM, artifacts copied via SSH

```
mvm pool build acme/workers
  1. Load pool spec (flake ref + profile)
  2. Select build backend (host, vsock, or ssh)
  3. Execute `nix build` (produces kernel, rootfs, fc-base.json)
  4. Extract artifacts to pool artifacts dir
  5. Record BuildRevision with content-hash
  6. Symlink artifacts/current → revisions/<hash>/
```

Builder VMs (vsock/ssh modes) are stateless, disposable, and uniquely named per build invocation. Builder specs: 4 vCPUs, 4 GiB RAM, 8 GiB output disk, 30-minute timeout.

When a pool references a template, the build step is skipped — artifacts are reused directly from the template cache.

## Coordinator & Gateway

The coordinator is a long-running service that handles traffic routing and instance lifecycle for multi-tenant deployments:

- **Port-based routing** -- each tenant/pool maps to a port range on the coordinator
- **TCP proxy** -- transparent proxy from coordinator port to instance guest IP
- **On-demand wake** -- incoming connection to a sleeping tenant triggers snapshot restore
- **Idle tracking** -- instances with no traffic for a configurable period become sleep-eligible
- **Wake coalescing** -- multiple wake requests for the same instance are batched

See [coordinator.md](coordinator.md) for configuration and API details.

## Guest Communication

Production guests have **no SSH daemon**. All host-guest communication uses Firecracker vsock:

| Port | Direction | Protocol |
|------|-----------|----------|
| 52 | Host → Guest | Guest agent (status, sleep-prep, wake, integration queries) |
| 53 | Guest → Host | Host-bound requests (wake requests from guest) |

Frame protocol: 4-byte big-endian length prefix + JSON body, over Firecracker's vsock UDS proxy.

Non-secret metadata (pool identity, routing tables, integration manifest) is delivered via a read-only ext4 **config drive** mounted at boot — no runtime network call needed.

See [agent.md](agent.md) for the QUIC API and reconcile loop, and [minimum-runtime.md](minimum-runtime.md) for the vsock drain protocol.
