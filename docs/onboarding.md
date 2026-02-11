# Onboarding Guide

This guide covers the end-to-end path from bare machine to running OpenClaw deployment.
Prerequisites: a Linux host (or macOS with Apple Silicon) and network access.

## Fleet Topology

```
Coordinator (external)
    |  QUIC + mTLS (port 4433)
    v
Host A: mvm agent serve         Host B: mvm agent serve
    |                                |
    +-- gateway VM (routes)          +-- gateway VM
    +-- worker VMs (integrations)    +-- worker VMs
```

The coordinator pushes signed desired state to hosts. Hosts run `mvm agent serve`, reconcile
actual state to match, and report back via QUIC. See [agent.md](agent.md) for protocol details.

## Adding a Host

### 1. Prepare the host

```bash
mvm add host --ca ca.crt --signing-key coordinator.pub
```

This single command:
- **Bootstraps** the environment (Lima on macOS, Firecracker, kernel, rootfs)
- **Initializes mTLS certificates** using the provided CA (omit `--ca` for self-signed dev certs)
- **Installs the coordinator's Ed25519 signing key** to `/etc/mvm/trusted_keys/`

For production (`MVM_PRODUCTION=1`), the agent rejects unsigned desired state — the signing
key is required. In dev mode, both signed and unsigned requests are accepted.

| Argument | Required | Description |
|----------|----------|-------------|
| `--ca <path>` | No | CA certificate PEM. Omit for self-signed dev mode |
| `--signing-key <path>` | No | Coordinator's Ed25519 public key (base64, 32 bytes) |
| `--production` | No | Enable production bootstrap (skip Homebrew, assume apt) |

After preparation, start the agent daemon:

```bash
mvm agent serve --interval-secs 30
```

See [security.md](security.md) for mTLS details and [cli.md](cli.md) for `agent serve` flags.

### 2. Verify the host

From the coordinator (or any node with mTLS certs):

```bash
mvm coordinator status --node <host-ip>:4433
```

This queries `NodeInfo` over QUIC: architecture, vCPUs, memory, jailer availability, cgroup v2 support.

## Creating a Deployment

### 3. Create an OpenClaw deployment

```bash
mvm new openclaw myapp
```

This single command creates a complete deployment:
1. **Auto-allocates** a network identity (net-id + /24 subnet from 10.240.0.0/12)
2. **Creates tenant** `myapp` with default quotas (32 vCPUs, 64GiB memory)
3. **Creates gateway pool** `myapp/gateways` (role: gateway, 2 vCPU, 1024 MiB)
4. **Creates worker pool** `myapp/workers` (role: worker, 2 vCPU, 1024 MiB, 2048 MiB data disk)
5. **Builds** both pools (Nix inside ephemeral Firecracker VMs)
6. **Scales** gateway to 1 running, workers to 2 running + 1 warm

| Argument | Required | Description |
|----------|----------|-------------|
| `openclaw` | Yes | Template name |
| `myapp` | Yes | Deployment name (becomes tenant ID) |
| `--net-id <N>` | No | Override auto-allocated network ID |
| `--subnet <CIDR>` | No | Override auto-computed subnet |
| `--flake <ref>` | No | Override template's default flake reference |

Gateways are reconciled first (role priority 0) so network routing is ready before workers
start. Workers get the secrets drive and vsock agent for integration lifecycle. See
[roles.md](roles.md) for the full role model.

## Connecting to Your Deployment

### 4. View the deployment dashboard

```bash
mvm connect myapp
```

Shows:
- **Network**: gateway IP, subnet, bridge name
- **Pools**: role, resources, desired counts
- **Instances**: IDs, status, guest IPs
- **Quick reference**: commands for secrets, scaling, instance listing

Sample output:

```
Deployment: myapp

Network:
  Gateway:  10.240.1.1
  Subnet:   10.240.1.0/24
  Bridge:   br-tenant-1

Pools:
  myapp/gateways (role: gateway, 2vcpu/1024MiB, running: 1, warm: 0)
  myapp/workers  (role: worker, 2vcpu/1024MiB, running: 2, warm: 1)

Instances:
  myapp/gateways/i-a1b2c3d4 Running ip=10.240.1.2
  myapp/workers/i-e5f6a7b8  Running ip=10.240.1.3
  myapp/workers/i-c9d0e1f2  Running ip=10.240.1.4
  myapp/workers/i-a3b4c5d6  Warm    ip=10.240.1.5

Quick reference:
  Set secrets:    mvm tenant secrets set myapp --from-file secrets.json
  List instances: mvm instance list --tenant myapp
  Scale workers:  mvm pool scale myapp/workers --running 4 --warm 2
```

### 5. Configure secrets

```bash
mvm tenant secrets set myapp --from-file secrets.json
```

Secrets are delivered via a tmpfs-backed read-only ext4 disk at `/run/secrets` inside each
instance. They never touch persistent storage. With secret scoping enabled, each integration
receives only the keys it needs. See [integrations.md](integrations.md) for scoped secrets.

## Desired State Alternative

Instead of the CLI workflow above, the coordinator can push a single desired state document
that creates the entire deployment:

```json
{
  "schema_version": 1,
  "node_id": "local",
  "tenants": [
    {
      "tenant_id": "myapp",
      "network": { "tenant_net_id": 1, "ipv4_subnet": "10.240.1.0/24" },
      "quotas": {
        "max_vcpus": 32, "max_mem_mib": 65536,
        "max_running": 16, "max_warm": 8,
        "max_pools": 10, "max_instances_per_pool": 32,
        "max_disk_gib": 500
      },
      "pools": [
        {
          "pool_id": "gateways",
          "flake_ref": "github:openclaw/nix-openclaw",
          "profile": "minimal",
          "role": "gateway",
          "instance_resources": { "vcpus": 2, "mem_mib": 1024, "data_disk_mib": 0 },
          "desired_counts": { "running": 1, "warm": 0, "sleeping": 0 }
        },
        {
          "pool_id": "workers",
          "flake_ref": "github:openclaw/nix-openclaw",
          "profile": "minimal",
          "role": "worker",
          "instance_resources": { "vcpus": 2, "mem_mib": 1024, "data_disk_mib": 2048 },
          "desired_counts": { "running": 2, "warm": 1, "sleeping": 0 }
        }
      ]
    }
  ],
  "prune_unknown_tenants": false,
  "prune_unknown_pools": false
}
```

Push it with: `mvm coordinator push --desired myapp.json --node <host-ip>:4433`

See [desired-state-schema.md](desired-state-schema.md) for the full schema reference.

## Day-2 Operations

- **Update images**: modify the flake, then `mvm pool build myapp/workers`
- **Scale**: `mvm pool scale myapp/workers --running 4 --warm 2`
- **Rotate secrets**: `mvm tenant secrets rotate myapp`
- **Rotate certs**: `mvm agent certs rotate`
- **Monitor**: `mvm node stats --json`
- **Audit trail**: `mvm events myapp --last 20`

See [minimum-runtime.md](minimum-runtime.md) for sleep/wake lifecycle and drain protocol.

## Security Summary

| Layer | Step | Mechanism |
|-------|------|-----------|
| Binary integrity | `mvm add host` | SHA256 checksum verification |
| Transport security | `mvm add host --ca` | mTLS (QUIC + rustls) |
| State integrity | `mvm add host --signing-key` | Ed25519 signed desired state |
| Network isolation | `mvm new` (tenant create) | Per-tenant L2 bridge |
| Secrets protection | `mvm tenant secrets set` | tmpfs, 0600/0400, read-only mount |
| Data encryption | Optional | LUKS AES-256-XTS per-tenant |
| Build reproducibility | `mvm pool build` | Nix flakes |
| Runtime isolation | Instance start | Jailer + cgroups + seccomp |
| Privilege separation | Agent daemon | hostd (root) + agentd (unprivileged) |

See [security.md](security.md) for the full threat model and hardening details.

## File Paths Reference

| Path | Purpose |
|------|---------|
| `/var/lib/mvm/node_id` | Node UUID (auto-generated) |
| `/var/lib/mvm/certs/ca.crt` | CA certificate |
| `/var/lib/mvm/certs/node.crt` | Node certificate |
| `/var/lib/mvm/certs/node.key` | Node private key (0600) |
| `/etc/mvm/trusted_keys/*.pub` | Coordinator Ed25519 public keys |
| `/var/lib/mvm/tenants/<t>/tenant.json` | Tenant configuration |
| `/var/lib/mvm/tenants/<t>/secrets.json` | Tenant secrets |
| `/var/lib/mvm/tenants/<t>/audit.log` | Per-tenant audit log |
| `/var/lib/mvm/tenants/<t>/pools/<p>/pool.json` | Pool specification |
| `/var/lib/mvm/keys/<t>.key` | LUKS encryption key (0600) |
