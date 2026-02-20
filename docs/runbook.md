# Operations Runbook

Procedures for diagnosing and resolving production issues. For common user-facing errors, see [troubleshooting.md](troubleshooting.md).

## Instance Stuck in Warm or Sleeping

An instance that won't transition out of Warm or Sleeping usually indicates a stale PID or corrupted snapshot.

**Diagnose:**

```bash
mvm instance stats <tenant>/<pool>/<instance>
# Check: is the PID alive? Is the snapshot directory populated?
```

**Resolve:**

```bash
# Force stop clears PID and releases resources
mvm instance stop <tenant>/<pool>/<instance>

# Restart fresh
mvm instance start <tenant>/<pool>/<instance>
```

If the instance is stuck because Firecracker crashed mid-snapshot:

```bash
# Destroy and let the reconcile loop recreate
mvm instance destroy <tenant>/<pool>/<instance>
```

The agent's reconcile loop will create a replacement instance within one interval cycle.

## Build Failures

### Inspecting Build Logs

When `mvm pool build` or `mvm template build` fails, the error includes the last 50 lines of Nix output (SSH backend) or collected log frames (vsock backend).

For more detail:

```bash
# Increase timeout and enable debug logging
RUST_LOG=mvm_build=debug mvm template build <name> --timeout 600

# Test the flake locally before building in a VM
nix build .#packages.x86_64-linux.<profile>
```

### Clearing Build Cache

The cache key is a composite of `flake.lock` hash + profile + role. If the cache is stale:

```bash
# Force rebuild ignores cache and template reuse
mvm template build <name> --force

# Or for pools:
mvm pool build <tenant>/<pool> --force
```

### Builder VM Issues

If the ephemeral builder VM fails to boot:

```bash
# Check builder agent binary exists
ls /var/lib/mvm/builder/mvm-builder-agent

# Override builder mode if vsock isn't working
MVM_BUILDER_MODE=ssh mvm template build <name>
```

## Network Diagnostics

### Verify Network State

```bash
# Quick check
mvm net verify

# Deep check (probes all TAP devices and bridges)
mvm net verify --deep
```

### Bridge Issues

Each tenant has a dedicated bridge (`br-tenant-<net_id>`). If a bridge is missing:

```bash
# List bridges (inside Lima VM or on Linux host)
ip link show type bridge | grep br-tenant

# The bridge is created when the first instance starts.
# To force recreation, start any instance in the tenant:
mvm instance start <tenant>/<pool>/<instance>
```

### TAP Device Cleanup

Orphaned TAP devices from crashed instances:

```bash
# List TAP devices
ip link show type tun | grep tn

# TAP naming: tn<net_id>i<ip_offset>
# Remove orphan (only if no instance references it):
ip link del tn3i5
```

### Instance Connectivity

```bash
# Check IP assignment
mvm instance list --tenant <tenant>

# Ping from host (inside Lima VM on macOS)
ping -c 1 <guest_ip>

# Check iptables NAT rules
iptables -t nat -L -n | grep <tenant_subnet>
```

## Stale PIDs and Crashed Instances

The agent detects stale PIDs (process gone but PID file remains) during reconciliation and marks the instance as failed. To manually check:

```bash
# Run doctor for system-wide health
mvm doctor

# Check a specific instance
mvm instance stats <tenant>/<pool>/<instance>

# If PID is stale, force stop:
mvm instance stop <tenant>/<pool>/<instance>
```

For bulk cleanup:

```bash
# One-shot reconcile prunes stale state
mvm agent reconcile --desired /etc/mvm/desired.json --prune
```

## LUKS Key Rotation

Data volume encryption keys are per-tenant. Rotation requires re-encrypting all active data volumes.

### Procedure

1. **Generate new key:**

```bash
openssl rand -hex 32 > /var/lib/mvm/keys/<tenant>.key.new
chmod 600 /var/lib/mvm/keys/<tenant>.key.new
```

2. **Stop all instances in the tenant:**

```bash
mvm pool scale <tenant>/<pool> --running 0 --warm 0 --sleeping 0
# Wait for all instances to stop
mvm instance list --tenant <tenant>
```

3. **Rotate the key:**

```bash
mv /var/lib/mvm/keys/<tenant>.key /var/lib/mvm/keys/<tenant>.key.old
mv /var/lib/mvm/keys/<tenant>.key.new /var/lib/mvm/keys/<tenant>.key
```

4. **Destroy old instances** (their data volumes are encrypted with the old key):

```bash
mvm instance destroy <tenant>/<pool>/<instance> --wipe-volumes
```

5. **Scale back up** (new instances use the new key):

```bash
mvm pool scale <tenant>/<pool> --running 3 --warm 1
```

6. **Verify and remove old key:**

```bash
# Once all old instances are destroyed:
rm /var/lib/mvm/keys/<tenant>.key.old
```

### Dev Mode Keys

In dev mode, keys are set via environment variables:

```bash
export MVM_TENANT_KEY_ACME=$(openssl rand -hex 32)
```

## Coordinator Failover

The coordinator is currently single-instance. Failover is manual.

### With Etcd State

If the coordinator uses Etcd for state persistence, restart is straightforward:

```bash
# On the standby host (same config, same Etcd cluster):
mvm coordinator serve --config /etc/mvm/coordinator.toml
```

The coordinator recovers route tables and gateway state from Etcd. In-flight connections are dropped but new connections work immediately.

### Without Etcd (In-Memory State)

State is lost on restart. The coordinator rediscovers gateway state through health checks:

1. Start the coordinator with the same config
2. The health check loop probes all configured gateways
3. Running gateways are detected within one `health_interval_secs` cycle (default: 30s)
4. During this window, incoming connections trigger a wake (which is a no-op if the gateway is already running)

### DNS Failover

For automated failover, point the coordinator DNS at a health-checked load balancer:

```
clients → DNS → LB (health check port 8443) → coordinator-a / coordinator-b
```

Only one coordinator should be active at a time to avoid conflicting wake operations.

## Agent Recovery

### Agent Won't Start

```bash
# Check certificates
mvm agent certs status

# Verify desired state file
cat /etc/mvm/desired.json | python3 -m json.tool

# Start with debug logging
RUST_LOG=debug mvm agent serve --desired /etc/mvm/desired.json
```

### Reconcile Drift

If actual state diverges from desired:

```bash
# Run a one-shot reconcile with pruning
mvm agent reconcile --desired /etc/mvm/desired.json --prune

# The --prune flag destroys tenants and pools not in the desired state
```

### Agent Metrics

Check agent activity:

```bash
# JSON log format for structured analysis
mvm agent serve --log-format json --desired /etc/mvm/desired.json 2>&1 | \
  grep reconcile
```

## Snapshot Corruption

If an instance fails to wake from a snapshot:

```bash
# Check snapshot directory
ls -la /var/lib/mvm/tenants/<tenant>/pools/<pool>/instances/<instance>/snapshot/

# If corrupted, destroy and recreate:
mvm instance destroy <tenant>/<pool>/<instance>
# Reconcile loop creates a replacement
```

For pool-wide snapshot issues (e.g., after Firecracker version upgrade):

```bash
# Rebuild the pool to generate new base artifacts
mvm pool build <tenant>/<pool> --force

# Stop and destroy all instances (snapshots are version-specific)
mvm pool scale <tenant>/<pool> --running 0 --warm 0 --sleeping 0
# Wait, then scale back up with new artifacts
mvm pool scale <tenant>/<pool> --running 3 --warm 1
```

## Disk Space Recovery

```bash
# Check disk usage
df -h /var/lib/mvm

# Remove old build revisions (keeps current symlink target)
ls /var/lib/mvm/tenants/<tenant>/pools/<pool>/artifacts/revisions/
# Safe to remove any revision not pointed to by 'current'

# Destroy unused instances with volume wipe
mvm instance destroy <path> --wipe-volumes

# Clean template build cache
mvm template build <name> --force  # replaces old artifacts
```

## Emergency Procedures

### Kill All Instances for a Tenant

```bash
# Scale to zero
mvm pool scale <tenant>/<pool> --running 0 --warm 0 --sleeping 0

# Or destroy the entire tenant (irreversible):
mvm tenant destroy <tenant>
```

### Stop the Agent

```bash
sudo systemctl stop mvm-agent
# or
sudo systemctl stop mvm-agentd mvm-hostd
```

Stopping the agent does not stop running Firecracker instances — they continue as daemons. To stop everything:

```bash
sudo systemctl stop mvm-agent
# Then stop individual instances or kill Firecracker processes
```
