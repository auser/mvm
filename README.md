# mvm

Rust CLI that orchestrates multi-tenant [Firecracker](https://firecracker-microvm.github.io/) microVM fleets on Linux (or macOS via [Lima](https://lima-vm.io/) on Apple Silicon and x86_64), providing Nix-based reproducible builds, snapshot-based sleep/wake, per-tenant network isolation, and coordinator-driven reconciliation.

```
macOS / Linux Host  -->  Lima VM (Ubuntu)  -->  Firecracker microVMs
      mvm CLI              limactl                  /dev/kvm
```

## Object Model

```
Tenant (security/quota boundary)
  └── WorkerPool (homogeneous workload group)
        └── Instance (individual Firecracker microVM)
```

- **Tenant** -- isolation boundary owning quotas, secrets, network, and audit scope
- **WorkerPool** -- defines a workload type (flake ref, profile, resource limits, desired counts)
- **Instance** -- a single Firecracker microVM with its own state machine

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

Pin a specific version:

```bash
MVM_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

Custom install directory:

```bash
MVM_INSTALL_DIR=~/.local/bin curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

## Quick Start

### Dev Mode (single microVM, no tenants)

```bash
mvm bootstrap   # installs Lima, creates VM, downloads Firecracker + kernel + rootfs
mvm dev          # launches a microVM and drops you into SSH
```

### Multi-Tenant Mode

```bash
# Create a tenant with coordinator-assigned network
mvm tenant create acme --net-id 3 --subnet 10.240.3.0/24 \
    --max-vcpus 16 --max-mem 32768 --max-running 8

# Create and build a worker pool
mvm pool create acme/workers --flake github:org/app --profile minimal --cpus 2 --mem 1024
mvm pool build acme/workers

# Scale up instances
mvm pool scale acme/workers --running 3 --warm 1

# Interact with instances
mvm instance list acme/workers
mvm instance ssh acme/workers/i-a3f7b2c1

# Sleep/wake for cost savings
mvm instance sleep acme/workers/i-a3f7b2c1
mvm instance wake acme/workers/i-a3f7b2c1
```

### Fleet Reconciliation

```bash
# Generate desired state from existing tenants and pools
mvm agent desired --file desired.json

# One-shot reconcile from desired state file
mvm agent reconcile --desired desired.json

# Initialize mTLS certificates (self-signed for dev)
mvm agent certs init

# Long-running daemon with QUIC+mTLS API and periodic reconcile
mvm agent serve --desired desired.json --interval-secs 30 --listen 0.0.0.0:4433
```

### Day-2 Operations

```bash
# Monitor the fleet
mvm node info                  # node capabilities (CPUs, memory, KVM)
mvm node stats                 # aggregate fleet stats across tenants
mvm instance list              # all instances across all tenants

# Inspect a specific tenant
mvm tenant info acme --json    # full config as JSON
mvm events acme --last 10      # recent audit events

# Scale a pool up or down
mvm pool scale acme/workers --running 5 --warm 2 --sleeping 3

# Sleep idle instances for cost savings, wake on demand
mvm instance sleep acme/workers/i-a3f7b2c1
mvm instance wake acme/workers/i-a3f7b2c1

# Rotate secrets and certificates
mvm tenant secrets rotate acme
mvm agent certs rotate

# Garbage collection (remove old snapshots and build artifacts)
mvm pool gc acme/workers
mvm node gc

# Verify network isolation
mvm net verify

# Teardown a tenant (cascades to all pools and instances)
mvm tenant destroy acme --force
```

## Commands

### Dev Mode

| Command | Description |
|---------|-------------|
| `mvm bootstrap` | Full setup from scratch (installs Lima, Firecracker, kernel, rootfs) |
| `mvm setup` | Create Lima VM, install Firecracker, download assets (requires `limactl`) |
| `mvm dev` | Launch into microVM, auto-bootstrapping if anything is missing |
| `mvm start [image]` | Start a microVM and drop into SSH |
| `mvm stop` | Stop the running microVM |
| `mvm ssh` | SSH into a running microVM |
| `mvm status` | Show status of Lima VM and microVM |
| `mvm destroy` | Tear down Lima VM and all resources |
| `mvm build [path]` | Build a microVM image from Mvmfile.toml |
| `mvm upgrade` | Check for and install updates |

### Tenant Management

| Command | Description |
|---------|-------------|
| `mvm tenant create <id>` | Create a tenant with network and quota config |
| `mvm tenant list` | List all tenants |
| `mvm tenant info <id>` | Show tenant details |
| `mvm tenant destroy <id>` | Destroy a tenant and all its resources |
| `mvm tenant secrets set <id>` | Set tenant secrets from file |
| `mvm tenant secrets rotate <id>` | Rotate tenant secrets |

### Pool Management

| Command | Description |
|---------|-------------|
| `mvm pool create <tenant>/<pool>` | Create a worker pool |
| `mvm pool list <tenant>` | List pools for a tenant |
| `mvm pool info <tenant>/<pool>` | Show pool details |
| `mvm pool build <tenant>/<pool>` | Build artifacts in ephemeral Firecracker VM |
| `mvm pool scale <tenant>/<pool>` | Set desired running/warm/sleeping counts |
| `mvm pool destroy <tenant>/<pool>` | Destroy a pool |

### Instance Operations

| Command | Description |
|---------|-------------|
| `mvm instance create <t>/<p>` | Create a new instance in a pool |
| `mvm instance list` | List instances (filterable by `--tenant`/`--pool`) |
| `mvm instance start <t>/<p>/<i>` | Start an instance |
| `mvm instance stop <t>/<p>/<i>` | Stop an instance |
| `mvm instance warm <t>/<p>/<i>` | Pause vCPUs (warm standby) |
| `mvm instance sleep <t>/<p>/<i>` | Snapshot and shut down |
| `mvm instance wake <t>/<p>/<i>` | Restore from snapshot |
| `mvm instance ssh <t>/<p>/<i>` | SSH into an instance |
| `mvm instance stats <t>/<p>/<i>` | Show instance metrics |
| `mvm instance destroy <t>/<p>/<i>` | Destroy an instance |
| `mvm instance logs <t>/<p>/<i>` | View Firecracker logs |

### Agent & Fleet

| Command | Description |
|---------|-------------|
| `mvm agent desired` | Generate desired state JSON from existing tenants/pools |
| `mvm agent reconcile --desired <file>` | One-shot reconcile from desired state file |
| `mvm agent serve` | Long-running daemon with QUIC API + periodic reconcile |
| `mvm agent certs init` | Initialize mTLS certificates (or `--ca <path>` for external CA) |
| `mvm agent certs rotate` | Rotate node certificate |
| `mvm agent certs status` | Show certificate status |
| `mvm net verify` | Verify tenant network isolation |
| `mvm node info` | Show node capabilities |
| `mvm node stats` | Show aggregate fleet statistics |

## Instance State Machine

```
Created --> Running <--> Warm --> Sleeping
               ^          |         |
               |     stop |    stop  |  wake
               |          v         v    |
               +------- Stopped <---+----+
```

- **Created** -- Instance registered, no process yet
- **Running** -- Firecracker process active, vCPUs executing
- **Warm** -- vCPUs paused via FC API, ready to resume instantly
- **Sleeping** -- Delta snapshot on disk, FC process killed, near-zero resource usage
- **Stopped** -- No process, no snapshot; can be restarted fresh
- **Destroyed** -- Terminal state, all resources cleaned up

Transitions are subject to **minimum runtime enforcement**: instances must stay Running for a configurable duration before becoming eligible for Warm, and Warm before eligible for Sleeping. See [docs/minimum-runtime.md](docs/minimum-runtime.md).

## Networking

Each tenant gets a dedicated Linux bridge with a coordinator-assigned subnet from the cluster CIDR (`10.240.0.0/12`).

- **Within-tenant**: instances on the same bridge communicate freely
- **Cross-tenant**: denied by construction (separate L2 domains)
- **Egress**: NAT masquerade to external networks
- **Sleep/wake**: network identity preserved across snapshot cycles

See [docs/networking.md](docs/networking.md) for details.

## Architecture

```
src/
  main.rs                  # CLI dispatch
  bin/mvm-hostd.rs         # Privileged executor daemon binary
  agent.rs                 # Reconcile loop + QUIC daemon + signed state verification
  node.rs                  # Node identity + stats + attestation
  infra/                   # Host/VM infrastructure (config, shell, bootstrap, UI)
  vm/
    microvm.rs, lima.rs    # Dev mode (unchanged)
    tenant/                # Tenant config, lifecycle, quotas, secrets
    pool/                  # Pool config, build, artifacts, scaling
    instance/              # Instance state machine, lifecycle, networking, snapshots
    bridge.rs              # Per-tenant bridge management
    naming.rs              # ID generation + validation
  security/                # Jailer, cgroups, seccomp, audit, mTLS certs, LUKS encryption
    signing.rs             # Ed25519 signed desired state
    snapshot_crypto.rs     # AES-256-GCM snapshot encryption
    attestation.rs         # Node attestation provider (TPM2/SEV-SNP hook)
  hostd/                   # Privilege separation (agentd/hostd split)
    protocol.rs            # IPC types (HostdRequest/HostdResponse)
    server.rs              # Privileged executor daemon
    client.rs              # Agentd client for hostd IPC
  sleep/                   # Sleep policy + idle metrics + minimum runtime enforcement
  worker/                  # Guest worker lifecycle hooks + vsock guest agent client
```

See [docs/architecture.md](docs/architecture.md) for the full module map.

## Security

Production deployments use a split-process model for privilege separation:

- **mvm-hostd** runs as root with minimal capabilities, executing only pre-defined privileged operations
- **mvm-agentd** runs fully unprivileged, handling reconciliation and the QUIC API

Additional security features: LUKS-encrypted data volumes, AES-256-GCM snapshot encryption, Ed25519 signed desired state (unsigned requests rejected in production), mTLS transport, per-tenant network isolation, jailer chroot + UID isolation, seccomp filters, cgroup resource limits, memory hygiene (`Zeroizing` for all key material), and vsock-based guest communication (no SSH daemon in production guests).

See [docs/security.md](docs/security.md) for the full threat model and hardening details.

## Platform Support

| Platform | Architecture | Method |
|----------|-------------|--------|
| Linux | x86_64, aarch64 | Native (direct /dev/kvm) |
| macOS | Apple Silicon (aarch64) | Via Lima VM |
| macOS | Intel (x86_64) | Via Lima VM |

Requires KVM support. On macOS, Lima provides the Linux VM with nested virtualization enabled.

## Build

```bash
cargo build
cargo run -- --help
```

## Documentation

- [Architecture](docs/architecture.md) -- module map, data model, design decisions
- [Security](docs/security.md) -- threat model, privilege separation, encryption, signed state
- [Networking](docs/networking.md) -- cluster-wide subnets, bridges, isolation
- [CLI Reference](docs/cli.md) -- complete command reference
- [Agent & Reconciliation](docs/agent.md) -- desired state schema, reconcile loop, QUIC API
- [Minimum Runtime](docs/minimum-runtime.md) -- runtime policy, vsock drain protocol, drive model

## License

Apache 2.0 — see [LICENSE](LICENSE) for details.
