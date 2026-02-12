# Role-Based VM Profiles

## Overview

Every pool has a **role** that determines its instances' services, drive requirements,
reconcile ordering, and NixOS module composition. Roles are set at pool creation and
cannot be changed afterward.

## Roles

| Role | Priority | Description |
|------|----------|-------------|
| `gateway` | 0 (first) | Routes traffic between tenants and the external network |
| `builder` | 1 | Runs Nix builds to produce guest images |
| `worker` | 2 (default) | Executes user workloads via the guest agent |
| `capability-imessage` | 3 (last) | Placeholder for iMessage capability integration |

### Gateway

Gateways read their configuration from the config drive (`/etc/mvm-config/gateway.json`)
and set up IP forwarding and routing rules. They are reconciled first so that network
infrastructure is ready before workers start.

- **Config drive**: required
- **Secrets drive**: required
- **NixOS services**: `mvm-gateway` (reads config, sets up routing)
- **Kernel sysctl**: `net.ipv4.ip_forward = 1`
- **Packages**: socat, iptables

### Worker

Workers execute user workloads and communicate with the host agent via vsock. They
support the sleep/wake lifecycle with a cooperative drain protocol.

- **Config drive**: not required
- **Secrets drive**: required
- **NixOS services**: `mvm-worker-agent` (vsock host communication), `mvm-sleep-prep-vsock` (drain listener on port 5200)
- **Packages**: socat

### Builder

Builders have Nix installed with flakes enabled and can build guest images inside
ephemeral Firecracker VMs. They get larger resource allocations and no secrets drive.

- **Config drive**: not required
- **Secrets drive**: not required
- **NixOS services**: Nix daemon
- **Packages**: git, nix

### CapabilityImessage

Placeholder role that currently extends the worker baseline with an iMessage-specific
hostname. Services will be added when the iMessage integration is implemented.

- **Config drive**: not required
- **Secrets drive**: not required

## CLI Usage

```bash
# Create a gateway pool (role must be specified)
mvm pool create acme/gateways --flake . --profile minimal --role gateway --cpus 2 --mem 1024

# Create a worker pool (default role, --role can be omitted)
mvm pool create acme/workers --flake . --profile python --cpus 2 --mem 1024

# Create a builder pool
mvm pool create acme/builders --flake . --profile minimal --role builder --cpus 4 --mem 4096

# List pools — role column shown
mvm pool list acme
```

The `--role` argument accepts: `gateway`, `worker`, `builder`, `capability-imessage`.
Default is `worker`.

## Reconcile Ordering

The reconcile loop processes pools sorted by role priority within each tenant:

### Scale-up (Phases 2-3): gateway-first

```
Gateway (0) → Builder (1) → Worker (2) → CapabilityImessage (3)
```

This ensures network infrastructure (gateways) and build capacity (builders) are
ready before workers that depend on them.

### Sleep (Phase 6): worker-first (reversed)

```
CapabilityImessage (3) → Worker (2) → Builder (1) → Gateway (0)
```

Workers are slept first so they stop sending traffic before the gateway that
routes their packets is reclaimed.

## Drive Model

Each role has different drive requirements, declared in `mvm-profiles.toml`:

| Role | rootfs | data | secrets | config |
|------|--------|------|---------|--------|
| Gateway | yes | yes | yes | **yes** |
| Worker | yes | yes | yes | no |
| Builder | yes | yes | no | no |
| CapabilityImessage | yes | yes | no | no |

The config drive is a read-only ext4 image mounted at `/etc/mvm-config` inside the guest.
It contains non-sensitive pool and instance metadata. See [minimum-runtime.md](minimum-runtime.md)
for the full drive model and trust boundaries.

## NixOS Module System

### mvm-profiles.toml

The build system uses a TOML manifest (`mvm-profiles.toml`) to map role+profile
combinations to NixOS module paths:

```toml
[meta]
version = 1

[profiles.minimal]
module = "guests/profiles/minimal.nix"

[profiles.python]
module = "guests/profiles/python.nix"

[roles.gateway]
module = "roles/gateway.nix"
requires_config_drive = true
requires_secrets_drive = true

[roles.worker]
module = "roles/worker.nix"
requires_config_drive = false
requires_secrets_drive = true

[roles.builder]
module = "roles/builder.nix"
requires_config_drive = false
requires_secrets_drive = false
```

### Build Attribute Resolution

When `mvm-profiles.toml` is present in the flake directory:

```
nix build <flake_ref>#packages.<system>.tenant-<role>-<profile>
```

Example: `nix build .#tenant-gateway-minimal`

When the manifest is absent (legacy flakes):

```
nix build <flake_ref>#packages.<system>.tenant-<profile>
```

Example: `nix build .#tenant-minimal`

### Flake Composition

The Nix flake composes guest images by layering modules:

```
baseline.nix + [role module] + [profile module] = guest image
```

Role modules are optional in `mkGuest`. Legacy outputs (without role) omit the
role module entirely:

```nix
# Role-aware
tenant-gateway-minimal = mkGuest {
  roleModules = [ ./roles/gateway.nix ];
  guestModules = [ ./guests/profiles/minimal.nix ];
};

# Legacy (no role)
tenant-minimal = mkGuest {
  guestModules = [ ./guests/profiles/minimal.nix ];
};
```

### Exported NixOS Modules

User flakes can import mvm modules:

| Module | Path |
|--------|------|
| `mvm-baseline` | `guests/baseline.nix` |
| `mvm-microvm` | microvm.nix upstream |
| `mvm-role-gateway` | `roles/gateway.nix` |
| `mvm-role-worker` | `roles/worker.nix` |
| `mvm-role-builder` | `roles/builder.nix` |

## Extending Roles

To add a new role:

1. Add variant to `Role` enum in `src/vm/pool/config.rs`
2. Update `Display` impl, `parse_role()` in `main.rs`, `role_priority()` in `agent.rs`
3. Create `nix/roles/<role>.nix` with role-specific NixOS configuration
4. Add `[roles.<role>]` entry to `nix/mvm-profiles.toml`
5. Add role+profile outputs to `nix/flake.nix`
6. Export the module in `nixosModules` if user flakes should be able to import it
