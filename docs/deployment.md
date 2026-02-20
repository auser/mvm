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
