# Troubleshooting

Common issues and their solutions.

## Lima VM Issues

### "Lima VM not found"

```
Error: Lima VM 'mvm' is not available. Run 'mvm setup' or 'mvm bootstrap' first.
```

**Fix**: Run `mvm bootstrap` (macOS) or `mvm setup` (Linux with Lima installed).

### "Failed to run command in Lima VM"

The Lima VM exists but is stopped.

**Fix**:
```bash
limactl start mvm
# or
mvm dev  # auto-starts Lima
```

### Lima VM is stuck

```bash
limactl stop mvm --force
limactl start mvm
```

If that fails:
```bash
mvm destroy
mvm bootstrap
```

## Firecracker Issues

### "Firecracker socket not responding"

The Firecracker process may have crashed.

**Check**:
```bash
mvm instance stats <tenant>/<pool>/<instance>
```

If the PID is stale (process doesn't exist), the instance will auto-recover on the next reconcile cycle, or manually:
```bash
mvm instance stop <tenant>/<pool>/<instance>
mvm instance start <tenant>/<pool>/<instance>
```

### "Failed to create TAP device"

**Cause**: Insufficient permissions inside the Lima VM, or TAP device name collision.

**Fix**:
```bash
# Verify network state
mvm net verify

# Check for orphaned TAP devices (inside Lima VM)
limactl shell mvm bash -c "ip link show | grep tap"
```

### Instance won't start after sleep

**Cause**: Snapshot may be corrupted or incompatible after a Firecracker version change.

**Fix**: Destroy and recreate the instance:
```bash
mvm instance destroy <path>
mvm instance create <tenant>/<pool>
mvm instance start <tenant>/<pool>/<new-id>
```

## Network Issues

### "Bridge not found"

```bash
# Create the bridge for a tenant
mvm net verify  # shows missing bridges

# The bridge is created automatically when instances start
# To recreate, destroy and recreate the tenant's first instance
```

### Cross-tenant traffic blocked

This is by design. Tenants are isolated at L2 (separate bridges). If you need cross-tenant communication, route through the host.

### Instance unreachable via SSH

```bash
# Check instance is running
mvm instance stats <path>

# Verify IP and TAP device
mvm instance list --tenant <tenant>

# Test connectivity from Lima VM
limactl shell mvm bash -c "ping -c 1 <guest_ip>"
```

## Certificate Issues

### "Failed to load client TLS config"

The mTLS certificates haven't been initialized.

**Fix**:
```bash
mvm agent certs init  # generates self-signed dev certs
```

### "Certificate expired"

```bash
mvm agent certs status  # check expiry
mvm agent certs rotate  # generate new cert
```

## Build Issues

### "Pool build failed"

**Common causes**:
- Nix flake syntax error
- Missing flake input
- Network issues during build (Nix needs to download dependencies)

**Debug**:
```bash
# Test the flake locally first
nix build .#<profile>

# Check builder VM logs
mvm pool build <path> --timeout 600  # increase timeout
```

### "Revision not found" on rollback

```bash
# List available revisions (inside Lima VM)
limactl shell mvm bash -c "ls /var/lib/mvm/tenants/<tenant>/pools/<pool>/artifacts/revisions/"
```

## Agent Issues

### Agent won't start

```bash
# Check certs exist
mvm agent certs status

# Start with verbose logging
RUST_LOG=debug mvm agent serve --desired desired.json
```

### Reconcile loop errors

```bash
# Run a one-shot reconcile to see errors
mvm agent reconcile --desired desired.json

# Check for stale PIDs
# The agent detects and cleans these automatically
```

### "Rate limited" on QUIC connection

The agent rate-limits incoming connections. Default: 100 requests/sec burst.

**Fix**: Reduce request frequency from the coordinator.

## Resource Issues

### "Quota exceeded"

```bash
mvm tenant info <tenant>  # check current quotas

# Either scale down instances or increase quotas
mvm tenant create <tenant> --max-vcpus 64 ...  # recreate with higher quotas
```

### Disk space full

```bash
# Check disk usage
limactl shell mvm bash -c "df -h /var/lib/mvm"

# Clean old revisions (keep 2 most recent)
# This is handled by disk_manager::cleanup_old_revisions()

# Destroy unused instances
mvm instance destroy <path> --wipe-volumes
```

## Logging

Set `RUST_LOG` for debug output:

```bash
RUST_LOG=debug mvm instance start <path>
RUST_LOG=mvm=trace mvm agent reconcile --desired desired.json
RUST_LOG=mvm::agent=debug mvm agent serve
```

Agent daemon logs in JSON format for structured log aggregation.
