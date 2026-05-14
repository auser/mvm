---
title: Architecture
description: Workspace structure, multi-backend design, dependency graph, and key abstractions.
---

## Multi-Backend Design

mvmctl supports multiple VM backends and auto-selects the best one for your platform:

| Backend | Platform | Selection Priority |
|---------|----------|-------------------|
| Firecracker | Linux with `/dev/kvm` | 1st (preferred) |
| Apple Container | macOS 26+ (Apple Silicon) | 2nd |
| libkrun | macOS Intel / macOS pre-26 (Hypervisor.framework) | 3rd |
| Docker | Any platform with Docker daemon | 4th (reduced isolation) |
| microvm.nix | Linux (NixOS-native QEMU) | Via `--hypervisor qemu` |

```
Linux (KVM):    mvmctl up  -->  Firecracker microVM (direct)
macOS 26+:      mvmctl up  -->  Apple Container (Virtualization.framework)
macOS <26:      mvmctl up  -->  libkrun microVM (Hypervisor.framework)
Docker:         mvmctl up  -->  Docker container (Tier 3 fallback)
```

All backends consume the same Nix-built ext4 rootfs. Override auto-detection with `--hypervisor`:

```bash
mvmctl up --flake . --hypervisor apple-container
mvmctl up --flake . --hypervisor firecracker
mvmctl up --flake . --hypervisor docker
mvmctl up --flake . --hypervisor qemu    # microvm.nix
mvmctl doctor   # check available backends
```

### Backend Capabilities

| Capability | Firecracker | Apple Container | libkrun | microvm.nix | Docker |
|------------|:-----------:|:---------------:|:-------:|:-----------:|:------:|
| Snapshots | Yes | No | No | No | No |
| Pause/resume | Yes | No | No | No | Yes |
| vsock | Yes | Yes | Yes | Yes | No |
| TAP networking | Yes | No (vmnet) | TSI | Yes | No |
| Port forwarding (`-p`) | Yes | Yes | Yes | Yes | Yes |
| Detach mode (`-d`) | Yes | Yes | Yes | Yes | Yes |

Template snapshots (`--snapshot`) are only available on the Firecracker backend.

## Workspace Structure

mvmctl is a Cargo workspace with 7 crates plus a root facade:

| Crate | Purpose |
|-------|---------|
| **mvm-core** | Pure types, IDs, config, protocol, signing, routing (no runtime deps) |
| **mvm-guest** | Vsock protocol, integration health checks, guest agent binary |
| **mvm-build** | Nix builder pipeline (dev_build for local, pool_build for fleet) |
| **mvm-runtime** | Shell execution, VM lifecycle, UI, template management |
| **mvm-security** | Security posture evaluation, jailer operations, seccomp profiles |
| **mvm-apple-container** | Apple Virtualization.framework backend (macOS 26+) |
| **mvm-cli** | Clap CLI, bootstrap, update, doctor, template commands |

The root crate is a facade (`src/lib.rs`) that re-exports all sub-crates as `mvmctl::core`, `mvmctl::runtime`, `mvmctl::build`, `mvmctl::guest`. The binary entry point (`src/main.rs`) delegates to `mvm_cli::run()`.

## Dependency Graph

```
mvm-core (foundation, no mvm deps)
├── mvm-guest (core)
├── mvm-build (core, guest)
├── mvm-security (core)
├── mvm-apple-container (core)
├── mvm-runtime (core, guest, build, security)
└── mvm-cli (core, runtime, build, guest)
```

Changes to `mvm-core` affect all crates. Changes to `mvm-cli` affect nothing else.

## Key Abstractions

### VmBackend

VM lifecycle abstraction defined in `mvm-core`:

- `start()`, `stop()`, `status()`, `list()`
- `capabilities()` -- pause/resume, snapshots, vsock, TAP networking

Implementations:
- **`FirecrackerBackend`** -- KVM microVMs via Firecracker (Linux native)
- **`AppleContainerBackend`** -- Virtualization.framework (macOS 26+)
- **`LibkrunBackend`** -- Hypervisor.framework via libkrun (macOS Intel / pre-26)
- **`MicrovmNixBackend`** -- NixOS-native QEMU runner
- **`DockerBackend`** -- Container-based fallback, universal platform support
- **`AnyBackend`** -- enum dispatch, auto-selects at runtime

### LinuxEnv

Where Linux commands run. Defined in `mvm-core`:

- `run()` -- run a command, return Output
- `run_visible()` -- run with stdout/stderr forwarded
- `run_stdout()` -- run and return stdout as String
- `run_capture()` -- run and capture both stdout and stderr

Implementations:
- **`BuilderVmEnv`** -- delegates commands into the libkrun/libkrun builder VM (macOS hosts)
- **`NativeEnv`** -- runs commands directly (Linux with `/dev/kvm`)

### ShellEnvironment

Build-time shell abstraction:

- `shell_exec()`, `shell_exec_stdout()`, `shell_exec_visible()`
- `log_info()`, `log_success()`, `log_warn()`

Used by `dev_build()` for local Nix builds.

### BuildEnvironment

Extends `ShellEnvironment` for fleet orchestration:

- `load_pool_spec()`, `load_tenant_config()`
- `ensure_bridge()`, `setup_tap()`, `teardown_tap()`
- `record_revision()`

### Supervisor Enforcement

`mvm-supervisor` owns the host-side policy slots used after plan admission:

- `L4Gate` evaluates policy-bundle `[[network.l4]]` rows with default-deny semantics
- `FirewallEnforcer` installs per-VM default-deny host firewall rules before backend launch and tears them down on failed launch or stop
- `LinuxNftFirewall` generates VM-scoped nftables tables that only allow TAP traffic to the supervisor proxy interface
- `NoopFirewallEnforcer` fails closed when no platform firewall is wired

## How It Works

At startup, mvmctl detects the platform and selects the appropriate backend:

1. **Linux with `/dev/kvm`** -- uses `FirecrackerBackend` directly via `NativeEnv`
2. **macOS 26+** -- uses `AppleContainerBackend` for VM lifecycle; Nix builds run inside the builder VM
3. **macOS Intel / pre-26** -- uses `LibkrunBackend` via `Hypervisor.framework`; Nix builds run inside the builder VM
4. **No KVM / no Apple Container / no libkrun** -- falls back to `DockerBackend` (reduced isolation; see [Matryoshka model](/security/matryoshka))

```
Host (macOS/Linux)
  └── VM Backend (auto-selected)
        └── Guest (your workload, headless, vsock only)
```

## Build Pipeline

`mvmctl build` and `mvmctl template build` invoke `nix build` inside the Linux environment, producing:

- **vmlinux** -- Firecracker-compatible kernel
- **rootfs.ext4** or **rootfs.squashfs** -- guest root filesystem

No initrd is needed -- the kernel boots directly into a busybox init script on the rootfs.

## Platform Support

| Platform | Architecture | Backend |
|----------|-------------|---------|
| macOS 26+ | Apple Silicon (aarch64) | Apple Container (native) |
| macOS pre-26 | Apple Silicon (aarch64) | libkrun (Hypervisor.framework) |
| macOS pre-26 | Intel (x86_64) | libkrun (Hypervisor.framework) |
| Linux with `/dev/kvm` | x86_64, aarch64 | Firecracker (native) |
| Linux without `/dev/kvm` | x86_64, aarch64 | Docker (Tier 3 fallback) |
| WSL2 | x86_64 | Docker (may have KVM) |
| Any platform with Docker | x86_64, aarch64 | Docker (universal fallback) |
