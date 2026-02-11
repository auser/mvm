# CLI Reference

## Dev Mode

Dev mode manages a single Firecracker microVM for local development. These commands are unchanged from the original mvm and do not interact with the multi-tenant system.

### `mvm bootstrap`

Full environment setup from scratch. Installs Lima via Homebrew, creates the VM, downloads Firecracker, kernel, and rootfs.

```bash
mvm bootstrap
```

### `mvm setup`

Creates the Lima VM, installs Firecracker, and downloads assets. Requires `limactl` to be installed.

```bash
mvm setup
```

### `mvm dev`

Smart entry point that detects your current state and does the right thing:
- Lima not installed? Runs full bootstrap.
- Lima VM missing? Runs setup.
- Lima VM stopped? Starts it.
- Firecracker missing? Installs it.
- MicroVM running? Reconnects via SSH.
- MicroVM stuck? Stops and restarts.

```bash
mvm dev
```

### `mvm start [image]`

Starts a microVM and drops into interactive SSH. Optionally provide a built image path.

```bash
mvm start
mvm start path/to/image.elf --cpus 4 --memory 2048
mvm start image.elf --config runtime.toml --volume /data:/mnt/data:10G
```

### `mvm stop`

Stops the running microVM and cleans up resources.

```bash
mvm stop
```

### `mvm ssh`

Reconnects to a running microVM via SSH.

```bash
mvm ssh
```

### `mvm status`

Shows the status of Lima VM, Firecracker, and microVM.

```bash
mvm status
```

### `mvm destroy`

Tears down the Lima VM and all resources. Prompts for confirmation.

```bash
mvm destroy
```

### `mvm build [path]`

Builds a microVM image from a Mvmfile.toml config.

```bash
mvm build
mvm build ./my-image --output built.elf
```

### `mvm upgrade`

Checks for and installs the latest version of mvm.

```bash
mvm upgrade
mvm upgrade --check    # only check, don't install
mvm upgrade --force    # reinstall even if current
```

---

## Tenant Management

### `mvm tenant create <id>`

Creates a new tenant with coordinator-assigned network and resource quotas.

```bash
mvm tenant create acme \
    --net-id 3 --subnet 10.240.3.0/24 \
    --max-vcpus 16 --max-mem 32768 \
    --max-running 8 --max-warm 4
```

**Required flags:**
- `--net-id <N>` -- coordinator-assigned tenant network ID (cluster-unique)
- `--subnet <CIDR>` -- coordinator-assigned IPv4 subnet

**Optional flags:**
- `--max-vcpus <N>` -- max aggregate vCPUs across all instances
- `--max-mem <MiB>` -- max aggregate memory
- `--max-running <N>` -- max concurrently running instances
- `--max-warm <N>` -- max warm (paused) instances

### `mvm tenant list`

Lists all tenants on this node.

```bash
mvm tenant list
mvm tenant list --json
```

### `mvm tenant info <id>`

Shows detailed tenant information including quotas, network, and pool/instance counts.

```bash
mvm tenant info acme
mvm tenant info acme --json
```

### `mvm tenant update <id>`

Updates tenant quotas or configuration.

```bash
mvm tenant update acme --max-vcpus 32 --max-running 16
```

### `mvm tenant destroy <id>`

Destroys a tenant and all its pools and instances.

```bash
mvm tenant destroy acme --force
mvm tenant destroy acme --force --wipe-volumes
```

### `mvm tenant secrets set <id>`

Sets tenant secrets from a file. Secrets are mounted as a read-only ext4 disk at `/run/secrets` inside each instance.

```bash
mvm tenant secrets set acme --from-file secrets.json
```

### `mvm tenant secrets rotate <id>`

Rotates tenant secrets. Running instances will get new secrets on next restart.

```bash
mvm tenant secrets rotate acme
```

---

## Pool Management

Pools use `<tenant>/<pool>` addressing.

### `mvm pool create <tenant>/<pool>`

Creates a new worker pool within a tenant.

```bash
mvm pool create acme/workers \
    --flake github:org/openclaw-worker?rev=abc123 \
    --profile minimal \
    --cpus 2 --mem 1024 \
    --data-disk 2048
```

### `mvm pool list <tenant>`

Lists all pools for a tenant.

```bash
mvm pool list acme
mvm pool list acme --json
```

### `mvm pool info <tenant>/<pool>`

Shows pool details including build revisions and instance counts.

```bash
mvm pool info acme/workers
mvm pool info acme/workers --json
```

### `mvm pool build <tenant>/<pool>`

Builds guest artifacts (kernel, rootfs, FC config) inside an ephemeral Firecracker builder microVM using Nix.

```bash
mvm pool build acme/workers
mvm pool build acme/workers --timeout 3600
```

### `mvm pool scale <tenant>/<pool>`

Sets desired instance counts by state. The system will create, start, warm, sleep, or stop instances to match.

```bash
mvm pool scale acme/workers --running 5 --warm 2 --sleeping 3
```

### `mvm pool update <tenant>/<pool>`

Updates pool configuration (flake ref, profile, resources).

```bash
mvm pool update acme/workers --flake github:org/app?rev=def456
```

### `mvm pool rollback <tenant>/<pool>`

Rolls back to a previous build revision.

```bash
mvm pool rollback acme/workers --revision 2
```

### `mvm pool destroy <tenant>/<pool>`

Destroys a pool and all its instances.

```bash
mvm pool destroy acme/workers --force
```

---

## Instance Operations

Instances use `<tenant>/<pool>/<instance>` addressing. Instance IDs are system-generated (format: `i-<8hex>`).

### `mvm instance list`

Lists instances, optionally filtered by tenant or pool.

```bash
mvm instance list
mvm instance list --tenant acme
mvm instance list --tenant acme --pool workers
mvm instance list --json
```

### `mvm instance start <t>/<p>/<i>`

Starts an instance (fresh boot or from stopped state).

```bash
mvm instance start acme/workers/i-a3f7b2c1
```

### `mvm instance stop <t>/<p>/<i>`

Stops an instance. Kills the Firecracker process and cleans up cgroups.

```bash
mvm instance stop acme/workers/i-a3f7b2c1
```

### `mvm instance warm <t>/<p>/<i>`

Transitions a running instance to warm state (vCPUs paused, memory retained).

```bash
mvm instance warm acme/workers/i-a3f7b2c1
```

### `mvm instance sleep <t>/<p>/<i>`

Snapshots the instance to disk and shuts it down. Near-zero resource usage while sleeping.

```bash
mvm instance sleep acme/workers/i-a3f7b2c1
mvm instance sleep acme/workers/i-a3f7b2c1 --force  # skip guest ACK
```

### `mvm instance wake <t>/<p>/<i>`

Restores an instance from snapshot and resumes execution.

```bash
mvm instance wake acme/workers/i-a3f7b2c1
```

### `mvm instance ssh <t>/<p>/<i>`

Opens an SSH session to a running instance.

```bash
mvm instance ssh acme/workers/i-a3f7b2c1
```

### `mvm instance stats <t>/<p>/<i>`

Shows instance metrics (snapshot size, idle time, CPU usage, health).

```bash
mvm instance stats acme/workers/i-a3f7b2c1
mvm instance stats acme/workers/i-a3f7b2c1 --json
```

### `mvm instance destroy <t>/<p>/<i>`

Destroys an instance. Stops it first if running.

```bash
mvm instance destroy acme/workers/i-a3f7b2c1
mvm instance destroy acme/workers/i-a3f7b2c1 --wipe-volumes
```

### `mvm instance logs <t>/<p>/<i>`

Shows Firecracker logs for an instance.

```bash
mvm instance logs acme/workers/i-a3f7b2c1
```

---

## Agent & Fleet

### `mvm agent reconcile`

One-shot reconcile from a desired state file. Creates tenants, pools, and instances as needed to match the desired state.

```bash
mvm agent reconcile --desired desired.json
mvm agent reconcile --desired desired.json --prune  # remove unknown tenants/pools
```

### `mvm agent serve`

Starts a long-running daemon that periodically reconciles and evaluates sleep policies.

```bash
mvm agent serve --desired desired.json --interval-secs 30
mvm agent serve \
    --listen 0.0.0.0:4433 \
    --tls-cert node.crt --tls-key node.key --tls-ca ca.crt \
    --coordinator-url https://coordinator:4433
```

### `mvm agent certs`

Manages mTLS certificates for coordinator communication.

```bash
mvm agent certs init --ca ca.crt
mvm agent certs request --coordinator https://coordinator:4433
mvm agent certs rotate
mvm agent certs status --json
```

### `mvm net verify`

Verifies network isolation rules for all tenants on this node.

```bash
mvm net verify
mvm net verify --json
```

Checks:
- Tenant bridges exist with correct subnets
- iptables NAT/forward rules are in place
- No cross-tenant connectivity
- Subnets match coordinator allocations

### `mvm node info`

Shows node capabilities and status.

```bash
mvm node info
mvm node info --json
```

Shows: Lima status, Firecracker version, jailer/cgroup/nftables availability, bridges, node ID.

### `mvm node stats`

Shows aggregate fleet statistics.

```bash
mvm node stats
mvm node stats --json
```

Shows: per-tenant running/warm/sleeping counts, memory usage, snapshot stats.

---

## Global Flags

All list/info/stats commands support `--json` for machine-readable output.

## Addressing Convention

| Scope | Format | Example |
|-------|--------|---------|
| Tenant | `<tenant>` | `acme` |
| Pool | `<tenant>/<pool>` | `acme/workers` |
| Instance | `<tenant>/<pool>/<instance>` | `acme/workers/i-a3f7b2c1` |
