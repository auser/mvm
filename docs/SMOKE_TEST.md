# End-to-End Smoke Test

Manual validation of the full mvm lifecycle. Requires a Linux host with `/dev/kvm` or macOS with Lima.

## Prerequisites

**macOS (via Lima):**
```bash
mvm bootstrap          # installs Lima + Firecracker + kernel + rootfs
mvm status             # verify Lima VM is running, FC "Installed, not running"
```

**Linux (native):**
```bash
# Ensure /dev/kvm exists
ls -la /dev/kvm

# Bootstrap (downloads Firecracker, installs dependencies)
mvm bootstrap

# Verify
mvm node info
```

## 1. Dev Mode Sanity Check

Verify the single-VM dev path still works:

```bash
mvm dev                # should auto-bootstrap if needed, then SSH into microVM
# inside the VM:
uname -a               # confirm you're in the guest
exit                   # exit SSH (VM keeps running)
mvm status             # should show microVM running
mvm stop               # clean shutdown
```

## 2. Tenant Lifecycle

```bash
# Create a tenant with coordinator-assigned network
mvm tenant create smoke-test \
    --net-id 99 --subnet 10.240.99.0/24 \
    --max-vcpus 8 --max-mem 8192 --max-running 4

# Verify
mvm tenant list
mvm tenant info smoke-test
mvm tenant info smoke-test --json    # confirm JSON output works

# Check bridge was created
mvm net verify
```

**Expected**: tenant appears in list, info shows quotas and network config, bridge `br-tenant-99` exists.

## 3. Pool Lifecycle

```bash
# Create a pool
mvm pool create smoke-test/workers \
    --flake . --profile minimal \
    --cpus 2 --mem 1024

# Verify
mvm pool list smoke-test
mvm pool info smoke-test/workers

# Build artifacts (requires Nix inside the VM)
mvm pool build smoke-test/workers

# Set desired counts
mvm pool scale smoke-test/workers --running 2 --warm 1 --sleeping 1
```

**Expected**: pool appears in list, info shows flake ref / resources / desired counts. Build produces kernel + rootfs under `artifacts/revisions/`.

## 4. Instance Lifecycle

```bash
# Create instances manually
mvm instance list --tenant smoke-test --pool workers

# Start an instance
INSTANCE_ID=$(mvm instance list --tenant smoke-test --pool workers --json | jq -r '.[0].instance_id')
mvm instance start smoke-test/workers/$INSTANCE_ID

# Verify it's running
mvm instance stats smoke-test/workers/$INSTANCE_ID

# SSH into the instance
mvm instance ssh smoke-test/workers/$INSTANCE_ID
# inside: uname -a, check /run/secrets if secrets were set
# exit

# Stop the instance
mvm instance stop smoke-test/workers/$INSTANCE_ID
mvm instance list --tenant smoke-test --pool workers   # should show Stopped
```

**Expected**: instance transitions Created -> Running -> Stopped. SSH works, stats show PID/IP/TAP.

## 5. Sleep / Wake Round-Trip

```bash
# Start the instance again
mvm instance start smoke-test/workers/$INSTANCE_ID

# Sleep it (snapshot to disk)
mvm instance sleep smoke-test/workers/$INSTANCE_ID

# Verify snapshot exists
mvm instance stats smoke-test/workers/$INSTANCE_ID   # should show Sleeping status

# Wake it back up
mvm instance wake smoke-test/workers/$INSTANCE_ID

# Verify it's running again with same network identity
mvm instance stats smoke-test/workers/$INSTANCE_ID
```

**Expected**: instance transitions Running -> Sleeping -> Running. Snapshot created and restored. IP and TAP device preserved.

## 6. Agent Reconcile (One-Shot)

Generate desired state from the tenants and pools you just created:

```bash
# Generate desired state from existing tenant/pool config
mvm agent desired --file /tmp/desired.json

# Inspect the generated file
cat /tmp/desired.json

# On macOS, copy the file into the Lima VM for the agent to read
# limactl copy /tmp/desired.json mvm:/tmp/desired.json

# Run one-shot reconcile
mvm agent reconcile --desired /tmp/desired.json

# Verify instances match desired state
mvm instance list --tenant smoke-test --pool workers
```

You can also write `desired.json` by hand or generate it from your own tooling. The schema is documented in [docs/agent.md](agent.md).

**Expected**: agent creates/starts instances to match desired counts. Two instances should be Running.

## 7. Agent Daemon + QUIC Round-Trip

```bash
# Generate certificates
mvm agent certs init --ca /tmp/ca.crt

# Start agent daemon in background
mvm agent serve \
    --desired /tmp/desired.json \
    --interval-secs 30 \
    --listen 127.0.0.1:4433 \
    --tls-cert /tmp/node.crt --tls-key /tmp/node.key --tls-ca /tmp/ca.crt &

AGENT_PID=$!
sleep 3

# Query node status via coordinator client
mvm coordinator status --node 127.0.0.1:4433

# Push updated desired state
mvm coordinator push --desired /tmp/desired.json --node 127.0.0.1:4433

# List instances via coordinator
mvm coordinator list-instances --node 127.0.0.1:4433 --tenant smoke-test

# Stop agent
kill $AGENT_PID
```

**Expected**: QUIC connection succeeds with mTLS. Status returns node info. Push accepted.

## 8. Bridge Verification

```bash
mvm net verify
mvm net verify --json
```

**Expected**: clean report — all tenant bridges correct, subnets match, no cross-tenant leakage.

## 9. Operational Commands

```bash
# Disk usage
mvm node disk

# Garbage collection
mvm pool gc smoke-test/workers
mvm node gc

# Audit events
mvm events smoke-test
mvm events smoke-test --last 5 --json

# Shell completions (verify generation)
mvm completions bash > /dev/null && echo "bash completions OK"
mvm completions zsh > /dev/null && echo "zsh completions OK"
```

## 10. Teardown

```bash
# Destroy tenant (cascades to pools and instances)
mvm tenant destroy smoke-test --force

# Verify clean state
mvm tenant list                # should be empty
mvm net verify                 # no bridges remaining
mvm node disk                  # storage freed
```

## Troubleshooting

| Symptom | Check |
|---------|-------|
| `Lima VM not running` | `limactl list` — start with `limactl start mvm` |
| `/dev/kvm not found` | Enable nested virtualization in Lima config or use bare-metal Linux |
| `pool build` fails | Check Nix is installed inside VM: `nix --version` |
| `instance start` hangs | Check FC logs: `mvm instance logs <path>` |
| Bridge not created | Run as root or check `CAP_NET_ADMIN` capability |
| QUIC connection refused | Verify cert paths and that agent is listening on the correct port |

## Notes

- On macOS, all Firecracker operations happen inside the Lima VM. Network bridges and TAP devices are Lima-internal.
- On native Linux, operations run directly. Ensure the user has appropriate capabilities (`CAP_NET_ADMIN`, access to `/dev/kvm`).
- The smoke test is inherently stateful — run teardown between test runs.
- Sleep/wake requires that Firecracker snapshot support is available (check FC version >= 1.0).
