# Deployment Guide

## Overview

mvm runs on Linux hosts with `/dev/kvm` support. On macOS (development), it operates through a Lima VM. This guide covers single-node, multi-node, and coordinator deployments.

```
Single-node:    Host → mvm agent serve → Firecracker microVMs
Multi-node:     Coordinator → QUIC → N × agent nodes → Firecracker microVMs
```

## Prerequisites

- **Linux**: `/dev/kvm` available, root access for cgroups/networking
- **macOS (dev only)**: Apple Silicon or x86_64, Homebrew, Lima
- **Nix**: Required on builder VMs for image builds (installed automatically)

## Development Mode (macOS / Local Testing)

Dev mode runs a single Firecracker microVM inside a Lima VM. It auto-bootstraps everything on first run — no manual setup needed.

### Quick Start

```bash
# One command does everything: installs Lima, Firecracker, downloads
# kernel + rootfs, boots a microVM, and drops you into SSH
mvm dev

# Inside the microVM:
uname -a          # confirm you're in the guest
exit              # exit SSH — VM keeps running as a daemon

# Check what's running
mvm status

# SSH back in
mvm ssh

# Stop the microVM
mvm stop

# Tear down everything (Lima VM + all resources)
mvm destroy
```

`mvm dev` is idempotent: if the environment is already set up, it connects directly. If the Lima VM is stopped, it restarts it. If Firecracker isn't installed, it installs it.

### Step-by-Step Setup

If you prefer manual control over each stage:

```bash
# 1. Install Homebrew + Lima (macOS only)
mvm bootstrap

# 2. Create Lima VM + install Firecracker + download assets
mvm setup

# 3. Start microVM and SSH in
mvm start

# 4. Open a shell in the Lima VM itself (not the microVM)
mvm shell

# 5. Build mvm from source inside the Lima VM
mvm sync              # release build, installs to /usr/local/bin inside VM
mvm sync --debug      # faster compile, slower runtime
mvm sync --force      # rebuild even if versions match
mvm sync --skip-deps  # skip installing rustup/apt packages
```

### Lima VM Resources

The Lima VM defaults to 8 vCPUs and 16 GiB memory. Override with:

```bash
mvm dev --lima-cpus 4 --lima-mem 8
mvm setup --lima-cpus 4 --lima-mem 8
```

### Dev Cluster (Agent + Coordinator Locally)

For testing the multi-tenant reconciliation stack without remote nodes:

```bash
# Initialize: generates self-signed certs, default desired state,
# coordinator config — all stored in ~/.mvm/dev-cluster/
mvm dev-cluster init

# Start agent + coordinator as background processes
mvm dev-cluster up

# Check status (PIDs, ports, log paths)
mvm dev-cluster status

# Stop everything
mvm dev-cluster down
```

Files created in `~/.mvm/dev-cluster/`:

| File | Purpose |
|------|---------|
| `desired.json` | Default desired state (dev tenant, gateway + worker pools) |
| `coordinator.toml` | Local coordinator config pointing at `127.0.0.1:4433` |
| `agent.pid` / `coordinator.pid` | PID files for background processes |
| `agent.log` / `coordinator.log` | Log output |

### Testing the Multi-Tenant Stack

With the Lima VM running, exercise the full lifecycle locally:

```bash
# Create a tenant with network isolation
mvm tenant create dev-test --net-id 99 --subnet 10.240.99.0/24 \
  --max-vcpus 8 --max-mem 8192

# Create and build a pool
mvm pool create dev-test/workers --flake . --profile minimal \
  --cpus 2 --mem 1024
mvm pool build dev-test/workers

# Scale instances via desired counts
mvm pool scale dev-test/workers --running 2 --warm 1

# Or reconcile from a desired state file
mvm agent desired --file /tmp/desired.json
mvm agent reconcile --desired /tmp/desired.json

# Inspect instance states
mvm instance list --tenant dev-test

# Test sleep/wake round-trip
mvm instance sleep dev-test/workers/<instance-id>
mvm instance wake dev-test/workers/<instance-id>

# Test the template workflow
mvm template create mytemplate --flake . --profile minimal \
  --role worker --cpus 2 --mem 1024
mvm template build mytemplate
mvm template info mytemplate

# Verify network state
mvm net verify

# Clean up
mvm tenant destroy dev-test --force
```

### Example: OpenClaw Deployment

OpenClaw is a messaging integration platform. mvm ships a built-in template that creates the full stack — gateway + workers — with a single command.

#### One-Command Deploy

```bash
# Creates tenant, gateway pool, worker pool, builds both, scales up
mvm new openclaw myapp
```

This runs 7 steps automatically:

1. Allocates a network ID and /24 subnet (e.g., `10.240.1.0/24`)
2. Creates tenant `myapp` with default quotas (32 vCPUs, 64 GiB memory)
3. Creates pool `myapp/gateways` (role: gateway, 2 vCPU, 1024 MiB, no data disk)
4. Creates pool `myapp/workers` (role: worker, 2 vCPU, 2048 MiB, 2048 MiB data disk)
5. Builds both pools (Nix inside ephemeral Firecracker VMs)
6. Scales gateway to 1 running, workers to 2 running + 1 warm

Gateways start first (role priority) so routing is ready before workers come up.

#### Inspect the Deployment

```bash
# Dashboard view
mvm connect myapp

# Detailed inspection
mvm tenant info myapp              # quotas, network, audit
mvm pool info myapp/gateways       # role: gateway, artifacts, desired counts
mvm pool info myapp/workers        # role: worker, data disk, desired counts
mvm instance list --tenant myapp   # all instances across both pools
```

#### Configure Secrets

Workers receive secrets via a tmpfs-backed read-only disk at `/run/secrets`:

```bash
# Set tenant secrets from a JSON file
mvm tenant secrets set myapp --from-file secrets.json
```

With secret scoping, each integration receives only its own keys. See [integrations.md](integrations.md).

#### Scale the Fleet

```bash
# Scale workers up
mvm pool scale myapp/workers --running 4 --warm 2 --sleeping 10

# Scale gateway (usually 1 is enough)
mvm pool scale myapp/gateways --running 1 --warm 1
```

Warm instances restore from snapshot in ~200ms on demand. Sleeping instances are fully snapshotted to disk and consume no CPU/memory.

#### Customizing the Template

Override the flake, network, or resources:

```bash
# Custom flake
mvm new openclaw myapp --flake github:myorg/my-openclaw-fork

# Explicit network
mvm new openclaw myapp --net-id 42 --subnet 10.240.42.0/24

# Resource overrides via config file
mvm new openclaw myapp --config deploy.toml
```

The config file (`deploy.toml`) supports per-pool overrides:

```toml
[overrides]
flake = "github:myorg/my-openclaw-fork"

[overrides.workers]
vcpus = 4
mem_mib = 4096
instances = 8

[overrides.gateways]
vcpus = 2
mem_mib = 2048

[secrets]
[secrets.api_keys]
file = "./secrets/api-keys.json"
```

#### Update Images

When the flake changes (new integration, dependency update):

```bash
# Rebuild worker pool (uses cache if flake.lock unchanged)
mvm pool build myapp/workers

# Force rebuild (ignores cache)
mvm pool build myapp/workers --force

# Rolling update: new instances use new artifacts, old instances
# are replaced on next sleep/wake cycle or reconcile
```

#### Production: Agent-Driven Reconciliation

Instead of manual CLI commands, run the agent daemon:

```bash
# Generate desired state from current config
mvm agent desired --file /etc/mvm/desired.json

# Start the agent (continuously reconciles)
mvm agent serve --desired /etc/mvm/desired.json --interval-secs 30
```

Or push desired state from a coordinator:

```bash
# On the coordinator host
mvm coordinator push --desired myapp.json --node 10.0.1.1:4433
```

#### Tear Down

```bash
# Destroy the entire deployment (tenant + all pools + all instances)
mvm tenant destroy myapp --force
```

### Debugging Dev Mode

```bash
# Verbose logging
RUST_LOG=debug mvm dev

# Check Lima VM state directly
limactl list

# Shell into the Lima VM (bypassing mvm)
limactl shell mvm

# Inspect Firecracker state inside Lima
limactl shell mvm bash -c "ls /opt/mvm/"

# Run system health checks
mvm doctor
```

### SSH Config

Add mvm to your SSH config for easy access:

```bash
mvm ssh-config >> ~/.ssh/config
# Then:
ssh mvm
```

---

## Installation

### Quick Install

```bash
curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

This installs the `mvm` binary to `/usr/local/bin` (override with `MVM_INSTALL_DIR`).

### Install Modes

The installer supports four modes:

```bash
# Binary only (default)
curl -fsSL .../install.sh | sh

# Dev mode (macOS + Lima, or Linux dev)
curl -fsSL .../install.sh | bash -s -- dev

# Production node
curl -fsSL .../install.sh | bash -s -- node \
  --coordinator-url https://coordinator:7777 \
  --install-service

# Coordinator
curl -fsSL .../install.sh | bash -s -- coordinator
```

| Mode | What it does |
|------|-------------|
| (default) | Downloads mvm binary for detected platform |
| `dev` | Installs binary + Homebrew/Lima (macOS) + runs `mvm bootstrap` |
| `node` | Installs binary + `mvm bootstrap --production` + optional systemd service |
| `coordinator` | Installs binary + runs `mvm coordinator bootstrap` |

### Installer Flags

| Flag | Description |
|------|-------------|
| `--coordinator-url URL` | Agent's coordinator endpoint (node mode) |
| `--interval-secs N` | Reconcile interval in seconds (default: 15) |
| `--install-service` | Install and enable systemd unit (node mode) |
| `--no-install-mvm` | Skip download, use existing mvm on PATH |
| `--mvm-path PATH` | Use a specific binary path |
| `--tls-ca PATH` | CA certificate path |
| `--tls-cert PATH` | Node certificate path |
| `--tls-key PATH` | Node private key path |

## Single-Node Deployment

A single node runs the agent directly, using a local desired state file.

### 1. Install and Bootstrap

```bash
# Install
curl -fsSL .../install.sh | bash -s -- node

# Or manually:
mvm bootstrap --production
```

Bootstrap installs Firecracker, sets up directories at `/var/lib/mvm/`, and configures the host.

### 2. Create Tenants and Pools

```bash
# Create a tenant (network + quota boundary)
mvm tenant create acme --net-id 3 --subnet 10.240.3.0/24 \
  --max-vcpus 64 --max-mem 65536

# Build a template (shared across pools)
mvm template create base --flake github:org/repo --profile minimal \
  --role worker --cpus 2 --mem 1024
mvm template build base

# Create pool referencing the template
mvm pool create acme/workers --template base --cpus 2 --mem 1024
```

### 3. Generate Desired State

```bash
mvm agent desired --file /etc/mvm/desired.json --node-id local
```

Edit the file to set desired instance counts:

```json
{
  "node_id": "local",
  "tenants": [{
    "tenant_id": "acme",
    "pools": [{
      "pool_id": "workers",
      "desired_running": 3,
      "desired_warm": 1,
      "desired_sleeping": 5
    }]
  }]
}
```

### 4. Run the Agent

**Foreground (development):**

```bash
mvm agent serve --desired /etc/mvm/desired.json --interval-secs 30
```

**Systemd (production):**

```bash
sudo cp deploy/systemd/mvm-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now mvm-agent
```

The agent continuously reconciles actual state toward the desired state, creating, starting, warming, sleeping, and destroying instances as needed.

## Multi-Node Deployment

A multi-node deployment adds a coordinator that load-balances traffic and manages on-demand wake across multiple agent nodes.

### Architecture

```
External clients
      │
      ▼
Coordinator (TCP proxy, port-based routing)
      │ QUIC + mTLS
      ├──→ Agent node-1 (Firecracker VMs)
      ├──→ Agent node-2 (Firecracker VMs)
      └──→ Agent node-3 (Firecracker VMs)
```

### 1. Set Up TLS Certificates

Each node needs mTLS certificates for QUIC communication.

**Self-signed (dev/single-org):**

```bash
# On each node:
mvm agent certs init
mvm agent certs status  # verify
```

This generates a self-signed CA and node certificate at `/var/lib/mvm/certs/`.

**External CA (production):**

```bash
# Copy your CA cert, then init:
mvm agent certs init --ca /path/to/company-ca.crt
mvm agent certs status
```

Certificate files:

| File | Path | Description |
|------|------|-------------|
| `ca.crt` | `/var/lib/mvm/certs/ca.crt` | CA certificate (root of trust) |
| `node.crt` | `/var/lib/mvm/certs/node.crt` | Node certificate (signed by CA) |
| `node.key` | `/var/lib/mvm/certs/node.key` | Node private key (mode 600) |

**Rotate certificates:**

```bash
mvm agent certs rotate
mvm agent certs status --json  # verify expiry
```

### 2. Deploy Agent Nodes

On each Linux host:

```bash
curl -fsSL .../install.sh | bash -s -- node \
  --coordinator-url https://coordinator.example.com:7777 \
  --install-service \
  --tls-ca /etc/mvm/certs/ca.crt \
  --tls-cert /etc/mvm/certs/node.crt \
  --tls-key /etc/mvm/certs/node.key
```

Or deploy the systemd service manually:

```bash
sudo systemctl enable --now mvm-agent
```

The agent listens on `0.0.0.0:4433` for QUIC connections from the coordinator.

### 3. Configure the Coordinator

Create a coordinator config file:

```toml
[coordinator]
idle_timeout_secs = 300
wake_timeout_secs = 10
health_interval_secs = 30
max_connections_per_tenant = 1000

# Optional: use Etcd for distributed state persistence
# etcd_endpoints = ["http://127.0.0.1:2379"]
# etcd_prefix = "/mvm/coordinator"

[[nodes]]
address = "10.0.1.1:4433"
name = "node-1"

[[nodes]]
address = "10.0.1.2:4433"
name = "node-2"

[[routes]]
tenant_id = "acme"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "10.0.1.1:4433"

[[routes]]
tenant_id = "beta"
pool_id = "gateways"
listen = "0.0.0.0:8444"
node = "10.0.1.2:4433"
idle_timeout_secs = 600  # per-route override
```

### Coordinator Config Reference

| Field | Default | Description |
|-------|---------|-------------|
| `idle_timeout_secs` | 300 | Seconds idle before sleeping gateway |
| `wake_timeout_secs` | 10 | Max seconds to wait for wake |
| `health_interval_secs` | 30 | Background health check interval |
| `max_connections_per_tenant` | 1000 | Connection limit per tenant |
| `etcd_endpoints` | (none) | Etcd cluster endpoints for state persistence |
| `etcd_prefix` | `/mvm/coordinator` | Etcd key prefix |

### 4. Start the Coordinator

```bash
mvm coordinator serve --config /etc/mvm/coordinator.toml
```

Or as a systemd service:

```ini
[Unit]
Description=mvm coordinator
After=network.target

[Service]
ExecStart=/usr/local/bin/mvm coordinator serve --config /etc/mvm/coordinator.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### 5. Etcd for State Persistence

Without Etcd, the coordinator uses in-memory state (lost on restart). For production:

```bash
# Install Etcd cluster (3 nodes recommended)
# Then add to coordinator config:
etcd_endpoints = ["http://etcd-1:2379", "http://etcd-2:2379", "http://etcd-3:2379"]
etcd_prefix = "/mvm/coordinator"
```

The coordinator stores route tables and gateway state in Etcd, allowing restart without losing wake state.

## Privilege Separation

mvm supports two deployment models for the agent:

### Monolithic (Simple)

A single process runs as root:

```
mvm-agent.service (root)
  ├── QUIC API listener
  ├── Reconcile loop
  └── All privileged operations (networking, cgroups, jailer)
```

Use `deploy/systemd/mvm-agent.service`.

### Split (Production)

Two processes with separated privileges:

```
mvm-agentd.service (user=mvm, unprivileged)
  ├── QUIC API listener
  ├── Reconcile loop
  └── IPC to hostd via Unix socket

mvm-hostd.service (root, minimal)
  └── Executes: start/stop/sleep/wake, bridge/TAP setup, jailer
```

Use `deploy/systemd/mvm-agentd.service` + `deploy/systemd/mvm-hostd.service`.

The split model limits the attack surface: the network-facing agentd has no root access, and the privileged hostd only accepts IPC from the local socket.

## Systemd Services

Three service files are provided in `deploy/systemd/`:

| Service | User | Purpose |
|---------|------|---------|
| `mvm-agent.service` | root | Monolithic agent (simple deployment) |
| `mvm-agentd.service` | mvm | Unprivileged agent daemon (split mode) |
| `mvm-hostd.service` | root | Privileged host daemon (split mode) |

Common management commands:

```bash
sudo systemctl start mvm-agent
sudo systemctl stop mvm-agent
sudo systemctl status mvm-agent
sudo journalctl -u mvm-agent -f  # follow logs
```

## Environment Variable Reference

### Security

| Variable | Description |
|----------|-------------|
| `MVM_PRODUCTION` | Set to `1` to enforce production security (jailer, mTLS, signed state) |
| `MVM_TENANT_KEY_<ID>` | Hex-encoded 32-byte LUKS key for tenant (dev mode). E.g., `MVM_TENANT_KEY_ACME` |

### Firecracker

| Variable | Description |
|----------|-------------|
| `MVM_FC_VERSION` | Override Firecracker version (e.g., `v1.14.1`) |
| `MVM_SSH_PORT` | Override SSH port for dev-mode microVM |

### Builder

| Variable | Description |
|----------|-------------|
| `MVM_BUILDER_MODE` | Builder backend: `auto` (default), `vsock`, or SSH |
| `MVM_BUILDER_AGENT_BIN` | Override path to mvm-builder-agent binary |
| `MVM_BUILDER_AGENT_PORT` | Vsock port for builder agent |
| `MVM_BUILDER_AUTHORIZED_KEY` | SSH public key for builder VM |
| `MVM_FC_ASSET_BASE` | Base URL for Firecracker assets (S3-compatible) |
| `MVM_FC_ASSET_KERNEL` | Override kernel filename from asset bucket |
| `MVM_FC_ASSET_ROOTFS` | Override rootfs filename from asset bucket |

### Template Registry

| Variable | Description |
|----------|-------------|
| `MVM_TEMPLATE_REGISTRY_ENDPOINT` | S3-compatible endpoint (e.g., `http://minio:9000`) |
| `MVM_TEMPLATE_REGISTRY_BUCKET` | Registry bucket name |
| `MVM_TEMPLATE_REGISTRY_PREFIX` | Key prefix (default: `mvm`) |
| `MVM_TEMPLATE_REGISTRY_REGION` | AWS region (default: `us-east-1`) |
| `MVM_TEMPLATE_REGISTRY_ACCESS_KEY_ID` | AWS access key |
| `MVM_TEMPLATE_REGISTRY_SECRET_ACCESS_KEY` | AWS secret key |
| `MVM_TEMPLATE_REGISTRY_INSECURE` | Set to `true` for HTTP (no TLS) |

### Installer

| Variable | Description |
|----------|-------------|
| `MVM_VERSION` | Pin release version for install script |
| `MVM_INSTALL_DIR` | Install directory (default: `/usr/local/bin`) |
| `MVM_REPO` | Custom GitHub repo URL |
| `MVM_COORDINATOR_URL` | Coordinator endpoint |
| `MVM_AGENT_INTERVAL_SECS` | Reconcile interval (default: 15) |

### TLS

| Variable | Description |
|----------|-------------|
| `MVM_TLS_CA` | CA certificate path |
| `MVM_TLS_CERT` | Node certificate path |
| `MVM_TLS_KEY` | Node private key path |

### Logging

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level filter (e.g., `info`, `debug`, `mvm=trace`) |

## Filesystem Layout

```
/var/lib/mvm/
├── certs/
│   ├── ca.crt
│   ├── node.crt
│   └── node.key
├── keys/
│   └── <tenant>.key          # LUKS encryption keys (production)
└── tenants/
    └── <tenant>/
        ├── tenant.json        # Tenant config
        ├── audit.log          # Append-only audit log
        └── pools/
            └── <pool>/
                ├── pool.json  # Pool spec
                ├── artifacts/
                │   ├── current -> revisions/<hash>
                │   └── revisions/
                │       └── <hash>/
                │           ├── vmlinux
                │           ├── rootfs.ext4
                │           └── fc-base.json
                └── instances/
                    └── <instance>/
                        ├── state.json
                        ├── config.json
                        ├── data.ext4      # Data volume (optional LUKS)
                        ├── snapshot/      # Delta snapshot
                        └── run/           # PID, socket, logs
```

## Verification

After deployment, verify the setup:

```bash
# Check system health
mvm doctor

# Verify agent connectivity
mvm agent certs status

# Run a one-shot reconcile
mvm agent reconcile --desired /etc/mvm/desired.json

# Check instance status
mvm instance list --tenant acme
```
