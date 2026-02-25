# mvm -- Firecracker MicroVM Fleet Manager

## Project Overview

Rust CLI that orchestrates multi-tenant Firecracker microVM fleets on Linux (or macOS via Lima on Apple Silicon and x86_64). It provides Nix-based reproducible builds, snapshot-based sleep/wake, per-tenant network isolation, and coordinator-driven reconciliation.

```
macOS / Linux Host (this CLI) -> Lima VM (Ubuntu) -> Firecracker microVMs (/dev/kvm)
```

## Object Model

```
Tenant (security/quota/network boundary)
  -> WorkerPool (homogeneous workload group, shared artifacts)
       -> Instance (individual Firecracker microVM with state machine)
```

- **Tenant** -- isolation boundary. Owns quotas, network (coordinator-assigned subnet), secrets, audit log.
- **WorkerPool** -- workload definition. Owns flake ref, profile, desired counts, shared artifacts + base snapshot.
- **Instance** -- runtime entity. Owns state machine, TAP device, data disk, delta snapshot, PID.

## Architecture

The CLI runs on the host. All Linux operations run inside the Lima VM via `limactl shell mvm bash -c "..."`. Firecracker microVMs run inside Lima using nested virtualization (/dev/kvm).

### Workspace Structure

7-crate Cargo workspace with root facade:

- `mvm-core` -- pure types, IDs, config, protocol, signing, routing (NO runtime deps)
- `mvm-guest` -- vsock protocol, integration manifest/state (OpenClaw)
- `mvm-build` -- Nix builder pipeline (depends on mvm-core via BuildEnvironment trait)
- `mvm-runtime` -- shell execution, security ops, VM lifecycle, bridge, pool/tenant/instance management
- `mvm-agent` -- reconcile engine, coordinator client, sleep policy, templates
- `mvm-coordinator` -- gateway load-balancer, TCP proxy, wake manager, idle tracker
- `mvm-cli` -- Clap CLI, UI, bootstrap, upgrade (depends on all other crates)

Root package: `src/lib.rs` (facade re-exports) + `src/main.rs` (thin CLI entry → `mvm_cli::run()`)

Binaries: `mvm` (from root, delegates to mvm-cli), `mvm-hostd` (from mvm-runtime), `mvm-builder-agent` (from mvm-guest)

**Dependency graph:**
```
mvm-core (foundation, no mvm deps)
├── mvm-guest (core)
├── mvm-build (core, guest)
├── mvm-runtime (core, guest, build)
├── mvm-agent (core, runtime, build, guest)
├── mvm-coordinator (core, runtime)
└── mvm-cli (core, agent, runtime, coordinator, build)
```

**Key module locations (within crates):**

mvm-core: `tenant.rs`, `pool.rs`, `instance.rs`, `agent.rs`, `protocol.rs`, `build_env.rs`, `signing.rs`, `routing.rs`, `naming.rs`, `template.rs`

mvm-runtime: `shell.rs`, `vm/lima.rs`, `vm/firecracker.rs`, `vm/microvm.rs` (dev mode), `vm/bridge.rs`, `vm/tenant/`, `vm/pool/`, `vm/instance/`, `vm/template/`, `security/`, `hostd/`, `sleep/`, `worker/`, `build_env.rs`

mvm-build: `build.rs`, `orchestrator.rs`, `vsock_builder.rs`, `scripts.rs`, `cache.rs`, `template_reuse.rs`

mvm-guest: `vsock.rs`, `integrations.rs`, `builder_agent.rs`

mvm-agent: `agent.rs` (reconcile + QUIC), `hostd.rs`, `node.rs`, `sleep/policy.rs`, `templates.rs`

mvm-coordinator: `server.rs`, `proxy.rs`, `routing.rs`, `wake.rs`, `idle.rs`, `state.rs`

mvm-cli: `commands.rs`, `bootstrap.rs`, `template_cmd.rs`, `dev_cluster.rs`, `doctor.rs`, `display.rs`, `upgrade.rs`

### Key Design Decisions

- **Firecracker-only execution**: no Docker/containers. Builds run in ephemeral FC VMs with Nix.
- **Coordinator owns network allocation**: tenant subnets from cluster CIDR (10.240.0.0/12). Agents never derive IPs.
- **Per-tenant bridges**: network isolation by construction (separate L2 domains).
- **Pool-level base snapshots**: shared across instances. Instance-level delta snapshots on sleep.
- **Single lifecycle API**: all operations go through `instance/lifecycle.rs`. No direct FC manipulation elsewhere.
- **Dev mode isolation**: `mvm start/stop/ssh/dev` use a completely separate code path.
- **Persistent microVM**: `mvm start` launches Firecracker as a daemon. Exiting SSH does NOT kill the VM.
- **Shell scripts inside run_in_vm**: complex ops are bash scripts passed to `limactl shell`. Deliberate -- they run inside the Linux VM.
- **replace_process for SSH**: Unix process replacement for clean TTY pass-through.
- **Idempotent setup**: every step checks if already done before acting.
- **Vsock over SSH**: guest communication uses Firecracker vsock (UDS proxy), not SSH. No sshd in production guests.
- **Config drive for metadata**: non-secret instance/pool metadata delivered via read-only ext4 config drive, not SSH.
- **Minimum runtime enforcement**: host-side wall-clock timestamps prevent premature instance reclamation. Guest not involved in enforcement.
- **Reusable templates**: build once, share across tenants. Tenant customization via runtime volumes (secrets drive, config drive, data disk), not image rebuilds.
- **Multi-pool templates**: some workloads (e.g., OpenClaw) need gateway + worker pools. `mvm new openclaw myapp` creates both.
- **No `clippy::too_many_arguments`**: never suppress this lint. Instead, refactor into smaller functions or introduce a config/params struct. Smaller functions are easier to test in isolation.

### Networking

- Cluster CIDR: `10.240.0.0/12`, coordinator-assigned per-tenant /24 subnets
- Per-tenant bridge: `br-tenant-<tenant_net_id>` with gateway at .1
- TAP naming: `tn<net_id>i<ip_offset>` (e.g. `tn3i5`)
- Within-tenant east/west: allowed (same bridge)
- Cross-tenant: denied by construction (separate bridges)
- Sleep/wake preserves network identity

### Instance State Machine

Created -> Ready -> Running -> Warm -> Sleeping -> (wake) -> Running
Running/Warm/Sleeping -> Stopped -> Running (fresh boot)
Any -> Destroyed

All transitions enforced in `instance/state.rs`. Invalid transitions fail loudly.

## Build and Run

```bash
cargo build
cargo run -- --help

# Dev mode
cargo run -- dev         # auto-bootstrap + launch + SSH
cargo run -- status      # check what's running

# Multi-tenant
cargo run -- tenant create acme --net-id 3 --subnet 10.240.3.0/24
cargo run -- pool create acme/workers --flake . --profile minimal --cpus 2 --mem 1024
cargo run -- pool build acme/workers
cargo run -- instance list acme/workers
```

## Dev Network Layout

```
MicroVM (172.16.0.2, eth0)
    | TAP interface
Lima VM (172.16.0.1, tap0) -- iptables NAT -- internet
    | Lima virtualization
macOS / Linux Host
```

## Documentation

- `docs/architecture.md` -- workspace structure, multi-tenant template model, design decisions
- `docs/networking.md` -- cluster-wide subnets, bridges, isolation
- `docs/cli.md` -- complete command reference
- `docs/agent.md` -- desired state schema, reconcile loop, QUIC API
- `docs/security.md` -- threat model, hardening measures, env vars, deferred items
- `docs/minimum-runtime.md` -- minimum runtime policy, drain protocol, drive model
- `docs/development.md` -- contributor guide, testing, CI/CD
- `docs/onboarding.md` -- end-to-end deployment guide
- `docs/deployment.md` -- single/multi-node deployment, systemd, env vars
- `specs/plans/` -- implementation specs and plan

## Sprint Management

- Active sprint spec: `specs/SPRINT.md`
- Completed sprints archived to: `specs/sprints/` (e.g. `specs/sprints/SPRINT-1-foundation.md`)
- When a sprint is completed, rename `specs/SPRINT.md` to `specs/sprints/SPRINT-<N>-<name>.md` and create a new `specs/SPRINT.md` for the next sprint
