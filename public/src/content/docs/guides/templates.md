---
title: Templates
description: Build reusable microVM images and share them via a registry.
---

Templates are reusable microVM images built from Nix flakes. Build once, run anywhere. Share via an S3-compatible registry. Template snapshots (`--snapshot`) are available on the Firecracker backend only.

## Scaffold a Template

```bash
mvmctl template init my-service --local
```

Creates a minimal directory:

```
my-service/
├── flake.nix       # Nix flake via mkGuest
├── .gitignore
└── README.md
```

### Scaffold Presets

Use `--preset` to start from a language-specific template:

```bash
mvmctl template init my-api --local --preset python   # Python HTTP service
mvmctl template init my-web --local --preset http      # HTTP server
mvmctl template init my-db --local --preset postgres   # PostgreSQL
mvmctl template init my-job --local --preset worker    # Background worker
mvmctl template init my-vm --local --preset minimal    # Bare minimum (default)
```

## From an Existing Flake

If you already have a Nix flake, register it directly:

```bash
mvmctl template create openclaw --flake ../openclaw --profile minimal --role worker
mvmctl template build openclaw
```

All flags have defaults: `--flake .`, `--profile default`, `--role worker`, `--cpus 2`, `--mem 1024`. Local flake paths are resolved to absolute paths at creation time.

## Build

```bash
mvmctl template build my-service
mvmctl template build my-service --force    # Rebuild even if cached
```

Builds run `nix build` inside the Lima VM to produce kernel + rootfs artifacts. On success, artifact sizes are reported:

```
Template 'my-service' built successfully (revision: abc123, rootfs: 45.2 MiB, kernel: 12.1 MiB)
```

## Snapshots

Build with `--snapshot` to capture a fully booted, healthy VM state. Subsequent runs restore from this snapshot instead of cold-booting — **1-2 second startup instead of minutes**.

```bash
# Build + snapshot (one-time, waits for all services to be healthy)
mvmctl template build my-service --snapshot

# Every subsequent run auto-detects the snapshot and restores instantly:
mvmctl up --template my-service --name svc
```

The snapshot process:
1. Builds the template normally (`nix build`)
2. Boots a temporary VM from the built artifacts
3. Waits for the guest agent to respond (health check)
4. Waits for all integrations to report healthy (e.g., gateway listening)
5. Pauses vCPUs and captures a full Firecracker snapshot (`vmstate.bin` + `mem.bin`)
6. Stores the snapshot alongside the template revision

No flags are needed on `run` — snapshot detection is automatic. If a template has a snapshot, it's used; otherwise the VM cold-boots.

### Dynamic Configuration with Snapshots

**Key insight:** Snapshots preserve the OS and application state, but **config and secrets drives are created fresh at runtime** from your host directories. This means you get instant boots **and** flexible per-instance configuration.

**What's stored in the snapshot:**
- OS and kernel state
- Installed packages and applications
- Memory contents (running services, compiled code caches)
- Network stack configuration

**What's NOT stored (created fresh each run):**
- Config drive (`/mnt/config`) — built from your `--volume` or `--config-dir`
- Secrets drive (`/mnt/secrets`) — built from your `--volume` or `--secrets-dir`
- Data volumes — built from your `--volume host:guest:size`

#### Multiple Instances from One Snapshot

Run production, staging, and dev instances from the **same template snapshot** with **different configurations**:

```bash
# Production: real API keys, strict config
mvmctl up --template my-app --name prod \
    -v ./prod/config:/mnt/config \
    -v ./prod/secrets:/mnt/secrets

# Staging: test API keys, relaxed config
mvmctl up --template my-app --name staging \
    -v ./staging/config:/mnt/config \
    -v ./staging/secrets:/mnt/secrets

# Dev: no API keys, debug logging enabled
mvmctl up --template my-app --name dev \
    -v ./dev/config:/mnt/config
```

All three VMs restore from the **same snapshot** (1-2 second boot) but get **different configs/secrets** at runtime. The guest agent automatically remounts config/secrets drives after restore and restarts services with the fresh data.

**Benefits:**

✅ **Instant boots** — 1-2 seconds from snapshot instead of minutes cold-booting
✅ **Consistent base** — all instances run identical OS/app versions
✅ **Flexible configuration** — each instance gets its own runtime config
✅ **No config baked into image** — same template works for prod, staging, dev, testing
✅ **Easy testing** — spin up throwaway instances with test configs in seconds

## Share via Registry

Push and pull templates to S3-compatible storage:

```bash
mvmctl template push my-service
mvmctl template pull my-service
mvmctl template verify my-service     # Verify checksums
```

Configure the registry with environment variables:

```bash
export MVM_TEMPLATE_REGISTRY_ENDPOINT="https://s3.amazonaws.com"
export MVM_TEMPLATE_REGISTRY_BUCKET="mvm-templates"
export MVM_TEMPLATE_REGISTRY_ACCESS_KEY_ID="..."
export MVM_TEMPLATE_REGISTRY_SECRET_ACCESS_KEY="..."
```

## Multiple Roles

Create templates for multiple roles at once:

```bash
mvmctl template create-multi my-app --flake . --roles worker,gateway
mvmctl template build my-app-gateway
mvmctl template build my-app-worker
```

## Edit

Update an existing template's configuration:

```bash
# Increase memory for an existing template
mvmctl template edit openclaw --mem 2048

# Update multiple settings at once
mvmctl template edit my-service --cpus 4 --mem 4096

# Change the flake reference
mvmctl template edit my-service --flake /new/path
```

After editing, rebuild the template for changes to take effect:

```bash
mvmctl template build my-service --force
```

Available edit options:
- `--flake` - Update the Nix flake reference
- `--profile` - Change the flake package variant
- `--role` - Update the VM role (worker, gateway)
- `--cpus` - Change vCPU count
- `--mem` - Update memory in MiB
- `--data-disk` - Change data disk size in MiB

## Inspect

`template info` shows the full picture — spec, current revision, artifact sizes, and snapshot status:

```bash
mvmctl template info my-service
```

```
Template: my-service
  Flake:   /path/to/flake
  Profile: minimal
  Role:    worker
  vCPUs:   2
  Memory:  1024 MiB

Current Revision: abc123
  Built:   2026-03-14T12:00:00Z
  Kernel:  12.1 MiB
  Rootfs:  45.2 MiB
  Snapshot: present (vmstate: 1.5 MiB, mem: 1024.0 MiB)
```

Use `--json` for machine-readable output (includes full revision data).

## Manage

```bash
mvmctl template list                   # List all templates
mvmctl template info my-service        # Show details, sizes, snapshot status
mvmctl template edit my-service --mem 2048  # Edit template settings
mvmctl template delete my-service      # Remove a template
```
