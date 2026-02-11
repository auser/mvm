# Integration Lifecycle

## Overview

OpenClaw workers connect to external services (WhatsApp, Telegram, Slack, Signal,
iMessage, Discord, etc.) that maintain session state. This state must survive
sleep/wake cycles. Workers also produce artifacts and need inbound traffic
routed through the gateway.

## Integration State Model

Each integration's session state lives on the persistent data disk:

```
/data/integrations/
  whatsapp/
    state/           # Session files (Baileys keys, QR-paired session)
    checkpoint       # Timestamp of last successful checkpoint
  telegram/
    state/           # Bot session data
    checkpoint
  slack/
    state/
    checkpoint
```

### Integration Manifest

The config drive (`/etc/mvm-config/config.json`) includes an `integrations` array
telling the guest agent which integrations to manage:

```json
{
  "instance_id": "i-abc123",
  "pool_id": "workers",
  "tenant_id": "acme",
  "integrations": [
    {
      "name": "whatsapp",
      "checkpoint_cmd": "/opt/openclaw/bin/whatsapp-checkpoint",
      "restore_cmd": "/opt/openclaw/bin/whatsapp-restore",
      "critical": true
    },
    {
      "name": "slack",
      "critical": false
    }
  ]
}
```

Fields:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Integration name (used as directory name) |
| `checkpoint_cmd` | no | Command to run before sleep |
| `restore_cmd` | no | Command to run after wake |
| `critical` | no | If true, sleep blocks until checkpoint succeeds |

### Lifecycle

1. **Boot**: `mvm-integration-manager` reads config, creates `/data/integrations/<name>/state/` directories
2. **Running**: Integration services use their state directories for session data
3. **Sleep prep**: `mvm-sleep-prep-vsock` runs each integration's `checkpoint_cmd` before ACKing sleep
4. **Snapshot**: Host takes VM snapshot with state directories on the persistent data disk
5. **Wake**: `mvm-integration-restore` runs each integration's `restore_cmd`

## Gateway Routing

The gateway routes inbound traffic (webhooks, bot callbacks) to worker instances
using a routing table on the config drive.

### Routing Table Format

Written to `/etc/mvm-config/routes.json`:

```json
{
  "routes": [
    {
      "name": "slack-webhook",
      "match_rule": {
        "port": 8080,
        "path_prefix": "/webhook/slack"
      },
      "target": {
        "pool_id": "workers",
        "instance_selector": { "by_ip": "10.240.3.5" },
        "target_port": 8080
      }
    },
    {
      "name": "telegram-bot",
      "match_rule": {
        "port": 8443,
        "source_cidr": "149.154.160.0/20"
      },
      "target": {
        "pool_id": "workers",
        "instance_selector": "any"
      }
    }
  ]
}
```

### Match Rules

At least one criterion must be set per route:

| Field | Type | Description |
|-------|------|-------------|
| `port` | u16 | Match by TCP destination port |
| `path_prefix` | string | Match by HTTP path prefix |
| `source_cidr` | string | Match by source IP range |

### Instance Selectors

| Selector | Description |
|----------|-------------|
| `any` | Route to any running instance (default) |
| `by_ip` | Route to a specific instance by IP address |
| `least_connections` | Route to the instance with fewest active connections |

### Validation

- Each route must have at least one match criterion
- No two routes may match on the same port
- The routing table is validated when pushed via desired state

### How It Works

1. Coordinator pushes routing table via `DesiredPool.routing_table`
2. Agent writes `routes.json` to the gateway's config drive
3. Gateway `mvm-gateway` service reads the routes on boot
4. For each route with a port + target IP: creates an iptables DNAT rule
5. A healthcheck timer periodically verifies targets are reachable

## Secret Scoping

By default, the secrets drive contains a flat `secrets.json` with all tenant secrets.
With secret scoping, each integration receives only the keys it needs.

### Configuration

Set `secret_scopes` on the pool spec (via desired state):

```json
{
  "pool_id": "workers",
  "secret_scopes": [
    {
      "integration": "whatsapp",
      "keys": ["WHATSAPP_API_KEY", "WHATSAPP_SECRET"]
    },
    {
      "integration": "telegram",
      "keys": ["TELEGRAM_BOT_TOKEN"]
    }
  ]
}
```

When scopes are set, the secrets drive is structured as:

```
/run/secrets/
  whatsapp/
    secrets.json    # Only WHATSAPP_API_KEY and WHATSAPP_SECRET
  telegram/
    secrets.json    # Only TELEGRAM_BOT_TOKEN
```

When `secret_scopes` is empty (default), the flat format is used for backward
compatibility:

```
/run/secrets/
  secrets.json      # All tenant secrets
```

## Vsock Protocol Extensions

The host-to-guest vsock protocol includes integration-specific requests:

### IntegrationStatus

Query the status of all managed integrations.

**Request**: `GuestRequest::IntegrationStatus`

**Response**: `GuestResponse::IntegrationStatusReport { integrations }`

Each integration report includes:
- `name`: Integration name
- `status`: `active`, `paused`, `pending`, or `error`
- `last_checkpoint_at`: ISO timestamp of last checkpoint
- `state_size_bytes`: Bytes of state data on disk

### CheckpointIntegrations

Checkpoint named integrations before sleep. Sent before `SleepPrep`.

**Request**: `GuestRequest::CheckpointIntegrations { integrations: ["whatsapp", "telegram"] }`

**Response**: `GuestResponse::CheckpointResult { success, failed, detail }`

The sleep flow becomes:
1. Host sends `CheckpointIntegrations` with integration names
2. Guest runs checkpoint commands, returns result
3. Host sends `SleepPrep` (existing drain protocol)
4. Guest flushes, drops caches, ACKs
5. Host takes snapshot

## Reconcile Flow

Integration lifecycle hooks into the existing reconcile loop:

1. **Gateway pools reconciled first** (role priority 0) — routing is ready before workers start
2. **Worker pools reconciled second** (role priority 2) — integrations can connect
3. **Sleep**: Workers sleep first (reversed order), checkpoint integrations before snapshot
4. **Wake**: Config and secrets drives refreshed, integration restore runs
