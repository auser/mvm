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

### Module Map

**Infrastructure (unchanged from dev mode):**
- `infra/config.rs` -- constants (VM_NAME, FC_VERSION, ARCH, dev-mode network config), MvmState
- `infra/shell.rs` -- command helpers: `run_host`, `run_in_vm`, `run_in_vm_stdout`, `replace_process`
- `infra/bootstrap.rs` -- Homebrew + Lima installation
- `infra/ui.rs` -- colored output, spinners, confirmations
- `infra/upgrade.rs` -- self-update

**Dev mode (unchanged):**
- `vm/microvm.rs` -- single-VM lifecycle (start, stop, ssh)
- `vm/firecracker.rs` -- FC binary install, asset download
- `vm/network.rs` -- dev-mode TAP/NAT (172.16.0.x)
- `vm/lima.rs` -- Lima VM lifecycle
- `vm/image.rs` -- Mvmfile.toml build pipeline

**Multi-tenant model (new):**
- `vm/naming.rs` -- ID validation, instance_id gen, TAP naming
- `vm/bridge.rs` -- per-tenant bridge (br-tenant-<net_id>) create/destroy/verify
- `vm/tenant/{config,lifecycle,quota,secrets}.rs` -- tenant management
- `vm/pool/{config,lifecycle,build,artifacts}.rs` -- pool management + ephemeral FC builds
- `vm/instance/{state,lifecycle,net,fc_config,disk,snapshot}.rs` -- instance lifecycle API
- `security/{jailer,cgroups,seccomp,audit,metadata}.rs` -- hardening
- `sleep/{policy,metrics}.rs` -- sleep heuristics
- `worker/hooks.rs` -- guest worker signals
- `agent.rs` -- reconcile loop + QUIC daemon
- `node.rs` -- node identity + stats

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

- `docs/architecture.md` -- full module map, data model, filesystem layout
- `docs/networking.md` -- cluster-wide subnets, bridges, isolation
- `docs/cli.md` -- complete command reference
- `docs/agent.md` -- desired state schema, reconcile loop, QUIC API
- `specs/plans/` -- implementation specs and plan

## Sprint Management

- Active sprint spec: `specs/SPRINT.md`
- Completed sprints archived to: `specs/sprints/` (e.g. `specs/sprints/SPRINT-1-foundation.md`)
- When a sprint is completed, rename `specs/SPRINT.md` to `specs/sprints/SPRINT-<N>-<name>.md` and create a new `specs/SPRINT.md` for the next sprint
