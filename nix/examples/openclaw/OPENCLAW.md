# OpenClaw MicroVM Configuration

Optimized configuration for running OpenClaw in a Firecracker microVM.

## Template: `openclaw-prod`

**Specifications:**
- **CPUs**: 4 (prevents RCU stalls during V8 compilation)
- **Memory**: 4096 MB (4 GB)
- **Profile**: default
- **Role**: worker
- **Flake**: ./nix/examples/openclaw (local)

## Quick Start

### Option 1: Using the Helper Script

```bash
# Start with defaults (VM name: oc-prod, port: 3000)
./openclaw-start.sh

# Custom VM name
./openclaw-start.sh my-openclaw

# Custom VM name and instance name
./openclaw-start.sh my-openclaw openclaw-instance-2

# Custom port
./openclaw-start.sh oc-prod openclaw-1 8080
```

### Option 2: Manual Command

```bash
cargo run -- run \
    --template openclaw-prod \
    --name oc-prod \
    --env OPENCLAW_INSTANCE_NAME=openclaw-1 \
    --cpus 4 \
    --memory 4096 \
    -p 3000:3000 \
    --forward
```

## Access OpenClaw

Once running, access the web interface at:
```
http://localhost:3000
```

## Managing the VM

```bash
# Check status
cargo run -- status

# Stop the VM
cargo run -- stop oc-prod

# View logs (if needed)
cargo run -- vm diagnose oc-prod
```

## Running Multiple Instances

To run multiple OpenClaw instances simultaneously:

```bash
# Instance 1
cargo run -- run --template openclaw-prod --name oc-1 \
    --env OPENCLAW_INSTANCE_NAME=openclaw-1 \
    --cpus 4 --memory 4096 -p 3000:3000 --forward

# Instance 2 (different port and mDNS name)
cargo run -- run --template openclaw-prod --name oc-2 \
    --env OPENCLAW_INSTANCE_NAME=openclaw-2 \
    --cpus 4 --memory 4096 -p 3001:3000 --forward
```

Access:
- Instance 1: http://localhost:3000
- Instance 2: http://localhost:3001

## Troubleshooting

### mDNS Conflicts

If you see `Can't probe for a service which is announced already`:
- **Solution**: Use unique `OPENCLAW_INSTANCE_NAME` for each VM
- **Or**: Wait 2 minutes for old mDNS records to expire

### RCU Stall Warnings

If kernel logs show `rcu_sched kthread starved`:
- **Cause**: Not enough CPU cores
- **Solution**: Ensure `--cpus 4` or higher

### Port Already in Use

```
Error: Address already in use (os error 48)
```
- **Solution**: Use a different host port: `-p 3001:3000`

## Template Management

```bash
# List templates
cargo run -- template list

# Rebuild template (if flake changed)
cargo run -- template build openclaw-prod --force

# Edit template settings
cargo run -- template edit openclaw-prod --cpus 8 --mem 8192

# Delete template
cargo run -- template delete openclaw-prod
```

## Performance Notes

- **First boot**: 10-15 minutes (V8 compilation, snapshot creation)
- **Subsequent boots**: ~5 seconds (from snapshot)
- **CPU**: 4 cores recommended minimum
- **Memory**: 4 GB recommended for production workloads

## Configuration

Environment variables you can pass with `--env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENCLAW_INSTANCE_NAME` | `openclaw` | Unique mDNS service name |
| `OPENCLAW_PORT` | `3000` | Internal web server port |
| (Add others as needed) | | |

## Network Layout

```
Host (macOS/Linux)
  ↓ http://localhost:3000
Lima VM (172.16.0.1)
  ↓ port forwarding
MicroVM (172.16.0.2:3000)
  ↓ OpenClaw service
```

## Next Steps

1. Wait for template build to complete (~15 min)
2. Run `./openclaw-start.sh`
3. Access http://localhost:3000
4. Enjoy instant subsequent boots via snapshot!
