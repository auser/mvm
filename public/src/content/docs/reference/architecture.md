---
title: Architecture
description: Workspace structure, multi-backend design, dependency graph, and key abstractions.
---

## Multi-Backend Design

mvmctl supports multiple VM backends and auto-selects the best one for your platform:

| Backend | Platform | Selection Priority |
|---------|----------|-------------------|
| Firecracker | Linux with `/dev/kvm` | 1st (preferred) |
| libkrun | macOS Apple Silicon (Hypervisor.framework) | 2nd |
| Apple Container | macOS 26+ Apple Silicon | 3rd fallback |
| Docker | Any platform with Docker daemon | 4th (reduced isolation) |
| microvm.nix | Linux (NixOS-native QEMU) | Via `--hypervisor qemu` |

```
Linux (KVM):    mvmctl up  -->  Firecracker microVM (direct)
macOS:          mvmctl up  -->  libkrun microVM (Hypervisor.framework)
Docker:         mvmctl up  -->  Docker container (Tier 3 fallback)
```

All backends consume the same Nix-built ext4 rootfs. The rootfs is built through the builder VM first, then booted by the selected runtime backend. Override runtime auto-detection with `--hypervisor`:

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
- **`AppleContainerBackend`** -- Virtualization.framework (macOS 26+ Apple Silicon)
- **`LibkrunBackend`** -- libkrun-backed local VM support (Linux KVM, macOS Apple Silicon)
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
- **`BuilderVmEnv`** -- delegates Linux-only build work into the project builder VM
- **`NativeEnv`** -- runs Linux commands directly where the host itself is the Linux execution boundary

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

### Supervisor: the two layers

"Supervisor" is reused at two distinct layers in this codebase. Knowing which one a given file or PR talks about removes most of the confusion when reading the source.

#### 1. `mvm-supervisor` — host-side admission and audit substrate

The library at `crates/mvm-supervisor/`, consumed by `mvm-cli` when you run `mvmctl up`. It turns a "I want to run this workload" intent into a launched, audited microVM on a single host. Responsibilities, all on the host (not in the guest):

- **Admit an `ExecutionPlan`** — verify the Ed25519 signature, enforce the validity window, refuse replays via a nonce ledger. See `admit_for_run` and `host_signer::load_or_init_at`.
- **Chain-sign audit events** — append `plan.admitted` / `plan.launched` / `plan.failed` / `plan.oci_provenance` / `gateway.flow_opened` / `gateway.flow_closed` entries to `~/.mvm/audit/<tenant>.jsonl`, each hash-linked to the previous (`AuditEntry`, `FileAuditSigner` under cross-process `flock`, `verify_audit_chain`).
- **Run the gateway audit bridge** — splice every guest network byte through an in-process bridge that emits flow events into the chain (`gateway_bridge`, `gateway_audit`). The bridge is the load-bearing piece of [security claim 10](/security/ci-claims).
- **Verify sealed deps volumes**, **resolve policy bundles**, **enforce default-deny network policy**, **run the L4 / L7 egress proxies**.

It does not link libkrun, does not call `krun_start_enter`, and never owns the guest's lifetime directly. It tells a per-VM child process to do that.

#### 2. `mvm-libkrun-supervisor` (and `mvm-vz-supervisor`) — per-VM long-lived processes

The bin at `crates/mvm-libkrun-supervisor/` (and the Swift sibling at `crates/mvm-vz-supervisor/`) is the actual process that owns one running guest. **One process per VM**, by design:

> `krun_start_enter` calls `exit()` on the calling process when the guest powers off. An in-process registry would tear down every other libkrun guest the parent `mvmctl` is supervising. One process per VM scopes the `exit()` to a single supervisor, so the parent `mvmctl` returns immediately after spawning and survives a guest shutdown.

Lifecycle:

1. `mvm-backend::LibkrunBackend::start()` spawns the binary with a JSON `SupervisorConfig` piped to stdin.
2. The bin ad-hoc-codesigns itself for `Hypervisor.framework` on macOS, creates the per-VM state dir, writes its PID file.
3. Configures libkrun (`configure_with_gateway`), spawns the userspace network gateway (`passt` on Linux, `gvproxy` on macOS), spawns the gateway audit bridge.
4. Calls `krun_start_enter`, which blocks until the guest exits, then `exit()`s.

When `mvmctl stop <vm>` runs it reads the PID file and `SIGTERM`s this process. `mvm-vz-supervisor` is the parallel Swift binary that fills the same role on macOS 26+ Apple Silicon (Vz backend).

#### How they relate

The host-side `mvm-supervisor` (layer 1) builds a `SupervisorConfig`, spawns the per-VM bin (layer 2) with that JSON, and returns. The audit chain bridges the two layers: admission events (`plan.admitted` etc.) are written by layer 1 *before* layer 2 starts; runtime events (`gateway.flow_*`) are written by the bridge running *inside* layer 2. Both append to the same `~/.mvm/audit/<tenant>.jsonl` under cross-process `flock`, so the chain stays linear and `mvmctl audit verify` can validate it end-to-end.

```
mvmctl up <flake>
  └── mvm-supervisor (layer 1, in mvmctl process)
        │  • verifies ExecutionPlan signature
        │  • emits plan.admitted to ~/.mvm/audit/<tenant>.jsonl
        │  • resolves policy bundle, installs host firewall
        │  • spawns ↓
        └── mvm-libkrun-supervisor (layer 2, one process per VM)
              • boots libkrun guest
              • bridges guest network through gateway_bridge
              • emits gateway.flow_opened / flow_closed
                to the same ~/.mvm/audit/<tenant>.jsonl
              • exits when the guest exits
```

#### Why the split exists at all (the mvm / mvmd boundary)

This repo (`mvm`) is the single-host runtime — one host trusts itself with the hypervisor and the host-signer key. `mvm-supervisor` enforces the security claims at that boundary.

Multi-tenant fleet orchestration lives in the separate [mvmd](https://github.com/tinylabscom/mvmd) repo. mvmd does *not* consume `mvm-supervisor` as a library — it has its own gateway (`mvmd-gateway`), its own runtime (`mvmd-runtime`), and its own admission flow that ultimately calls down to mvm-tier backends. The cross-repo split is intentional per the [threat model](/security/threat-model) (ADR-002) and the [CI-enforced security claims](/security/ci-claims) (claim 10 substrate, ADR-058):

- `mvm-supervisor` ↔ per-VM, per-host, per-tenant admission + audit (security claims 8, 9, 10).
- `mvmd` ↔ cross-VM, cross-host, cross-tenant orchestration. mvmd's Plan 50 (network manager) layers per-tenant gateway pools, egress quotas, cross-tenant traffic isolation, and tenant-level audit rollup *above* mvm-supervisor's per-VM substrate.

A workload running through mvmd transits both layers: mvmd's admission decides which host the workload lands on and sets cross-tenant policy; mvm-supervisor on that host then runs the per-VM admission + audit substrate this doc describes.

### Supervisor Enforcement

`mvm-supervisor` owns the host-side policy slots used after plan admission:

- `L4Gate` evaluates policy-bundle `[[network.l4]]` rows with default-deny semantics
- `BackendLauncher::prepare_launch()` returns backend-owned runtime slot metadata before tenant launch, without starting tenant code
- `FirecrackerRunConfigLauncher` adapts a prebuilt Firecracker `FlakeRunConfig` into the supervisor backend slot, exposing its `VmSlot` during preparation and calling `run_from_build()` only after firewall install
- `Supervisor::with_*` assembly methods wire backend, policy, audit, artifact, and firewall slots without bypassing the launch-time firewall validation gate
- `FirewallSpec::from_vm_slot()` derives VM identity and TAP device from backend runtime `VmSlot` metadata, then validates identifiers before any platform rule generation
- `FirewallEnforcer` installs per-VM default-deny host firewall rules before backend launch and tears them down on failed launch or stop
- `LinuxNftFirewall` generates VM-scoped nftables tables that only allow TAP traffic to the supervisor proxy interface
- `NoopFirewallEnforcer` fails closed when no platform firewall is wired

## How It Works

At startup, mvmctl detects the platform and selects the appropriate runtime backend:

1. **Native Linux with `/dev/kvm`** -- uses `FirecrackerBackend` for runtime VM lifecycle
2. **macOS 26+ Apple Silicon** -- uses `AppleContainerBackend` for dev/runtime VM lifecycle; Nix builds run inside the libkrun-backed builder VM
3. **Other hosts** -- unsupported for local microVM isolation today; Docker is a Tier 3 convenience fallback only (see [Matryoshka model](/security/matryoshka))

WSL2 nested KVM and a Hyper-V managed Linux builder are future backend work, not part of the supported local platform matrix.

```
Host (macOS Apple Silicon / native Linux KVM)
  ├── Builder VM
  │     └── nix eval / nix build / artifact extraction
  └── Runtime backend (auto-selected)
        └── Runtime guest (your workload, headless, vsock where supported)
```

## Build Pipeline

`mvmctl build` and `mvmctl template build` are host commands that stage a build job for the builder VM. The builder VM invokes `nix build` inside Linux, producing:

- **vmlinux** -- Firecracker-compatible kernel
- **rootfs.ext4** or **rootfs.squashfs** -- guest root filesystem

No initrd is needed -- the kernel boots directly into a busybox init script on the rootfs.

The builder VM is not the runtime VM. Runtime commands such as `mvmctl up`, `mvmctl run`, and boot benchmarks consume the finished artifacts from the host cache. See [Builder VM](/guides/builder-vm/) for the detailed control-plane flow.

## Platform Support

| Platform | Architecture | Backend |
|----------|-------------|---------|
| macOS | Apple Silicon (aarch64) | libkrun (Hypervisor.framework) |
| macOS 26+ without libkrun | Apple Silicon (aarch64) | Apple Container fallback |
| Linux with `/dev/kvm` | x86_64, aarch64 | Firecracker (native) |
| Linux without `/dev/kvm` | x86_64, aarch64 | Docker (Tier 3 fallback) |
| macOS Intel | x86_64 | Unsupported for local microVMs |
| WSL2 | x86_64 | Future/experimental backend work |
| Any platform with Docker | x86_64, aarch64 | Docker (universal fallback) |
