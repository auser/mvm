# Coordinator — On-Demand Gateway Proxy

## Overview

The coordinator is a TCP proxy that sits between external clients and tenant
gateways running inside Firecracker microVMs. It routes inbound connections to
the correct tenant's gateway, waking the gateway from a warm snapshot on demand
if it isn't already running.

```
Client → Coordinator (TCP proxy) → Gateway VM (Firecracker)
                                  ↕
                            Agent (QUIC API)
```

## Architecture

### Port-Based Routing

Each tenant gets a dedicated listen port on the coordinator. The routing table
maps `listen_address → (tenant, pool, node)`. When a connection arrives on a
port, the coordinator knows which tenant it belongs to without inspecting
application-layer data.

### On-Demand Wake

Gateways don't need to run continuously. They can sit in a **warm** state
(snapshot taken, VM paused) and be restored in ~200ms when the first request
arrives:

1. Connection arrives on tenant's port
2. Coordinator checks if gateway is Running → fast-path proxy
3. If gateway is Idle/Warm → send `WakeInstance` to agent via QUIC
4. Poll agent until instance status is Running
5. TCP-probe gateway service port for readiness
6. Proxy the connection

Concurrent requests for the same tenant during wake are **coalesced** — only
one wake operation runs, and all waiters share the result via a broadcast
channel.

### Idle Sleep

When the last connection for a tenant closes, an idle timer starts. If no new
connection arrives within the configured timeout, the coordinator marks the
gateway for sleep. On the next wake, the gateway restores from its snapshot.

### Health Checking

A background loop periodically TCP-probes all gateways marked as Running. If a
probe fails (gateway crashed, network issue), the coordinator marks it Idle so
the next connection triggers a re-wake.

## Configuration

The coordinator reads a TOML config file:

```toml
[coordinator]
idle_timeout_secs = 300        # Default idle timeout (5 minutes)
wake_timeout_secs = 10         # Max time to wait for a wake
health_interval_secs = 30      # Background health check interval
max_connections_per_tenant = 1000

# Agent nodes the coordinator can talk to
[[nodes]]
address = "10.0.1.1:4433"
name = "node-1"

[[nodes]]
address = "10.0.1.2:4433"
name = "node-2"

# Route: listen address → tenant gateway pool on a node
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
idle_timeout_secs = 600        # Per-route override
```

### Config Fields

| Field | Default | Description |
|-------|---------|-------------|
| `idle_timeout_secs` | 300 | Seconds idle before sleeping gateway |
| `wake_timeout_secs` | 10 | Max seconds to wait for wake |
| `health_interval_secs` | 30 | Background health check interval |
| `max_connections_per_tenant` | 1000 | Connection limit per tenant |

### Route Fields

| Field | Required | Description |
|-------|----------|-------------|
| `tenant_id` | yes | Tenant ID to route to |
| `pool_id` | yes | Gateway pool on the target node |
| `listen` | yes | Listen address (e.g. `0.0.0.0:8443`) |
| `node` | yes | Agent QUIC address (e.g. `10.0.1.1:4433`) |
| `idle_timeout_secs` | no | Per-route idle timeout override |

## CLI Usage

```bash
# Start the coordinator
mvm coordinator serve --config coordinator.toml

# Display the routing table
mvm coordinator routes --config coordinator.toml
```

## Module Map

| Module | Purpose |
|--------|---------|
| `coordinator/config.rs` | TOML config parsing + validation |
| `coordinator/routing.rs` | Port-based route lookup table |
| `coordinator/server.rs` | TCP accept loop + connection dispatch |
| `coordinator/wake.rs` | On-demand wake with coalescing |
| `coordinator/proxy.rs` | L4 bidirectional TCP proxy |
| `coordinator/idle.rs` | Per-tenant connection counting + idle timers |
| `coordinator/health.rs` | Background health probes + readiness checks |
| `coordinator/client.rs` | QUIC client for agent communication |

## Connection Lifecycle

```
1. TCP accept on tenant's listen port
2. Route lookup → tenant_id, pool_id, node
3. Track connection in IdleTracker
4. WakeManager.ensure_running()
   a. Running → return gateway address (fast path)
   b. Waking → subscribe to existing wake channel
   c. Idle → initiate wake:
      - Send WakeInstance to agent
      - Poll InstanceList until Running
      - Broadcast result to all waiters
5. proxy_connection() → bidirectional TCP splice
6. Connection closes → IdleTracker.connection_closed()
7. If last connection for tenant → idle timer starts
8. If idle timeout expires → mark gateway Idle
```

## Deployment

The coordinator runs as a long-lived process, typically on a dedicated host or
the same host as the agent:

```bash
# Generate config
cat > coordinator.toml << 'EOF'
[coordinator]
idle_timeout_secs = 300

[[nodes]]
address = "127.0.0.1:4433"
name = "local"

[[routes]]
tenant_id = "acme"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
EOF

# Start coordinator (foreground)
mvm coordinator serve --config coordinator.toml

# Or with systemd
[Unit]
Description=mvm coordinator
After=network.target

[Service]
ExecStart=/usr/local/bin/mvm coordinator serve --config /etc/mvm/coordinator.toml
Restart=always

[Install]
WantedBy=multi-user.target
```

## Limitations (current sprint)

- **Single coordinator**: no HA or failover. Run one instance per deployment.
- **Fixed routing**: routes are static from config file. No dynamic registration.
- **L4 proxy only**: no TLS termination, HTTP inspection, or SNI-based routing.
- **No agent-side sleep**: coordinator marks gateways idle but doesn't actively
  send sleep commands to the agent. The agent's own reconcile loop handles
  sleep transitions.
