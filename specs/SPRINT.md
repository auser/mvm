# mvm Sprint 11: Dev Environment + External Project Support

Previous sprints:
- [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete)
- [SPRINT-2-production-readiness.md](sprints/SPRINT-2-production-readiness.md) (complete)
- [SPRINT-3-real-world-validation.md](sprints/SPRINT-3-real-world-validation.md) (complete)
- Sprint 4: Security Baseline 90% (complete)
- Sprint 5: Final Security Hardening (complete)
- [SPRINT-6-minimum-runtime.md](sprints/SPRINT-6-minimum-runtime.md) (complete)
- [SPRINT-7-role-profiles.md](sprints/SPRINT-7-role-profiles.md) (complete)
- [SPRINT-8-integration-lifecycle.md](sprints/SPRINT-8-integration-lifecycle.md) (complete)
- [SPRINT-9-openclaw-support.md](sprints/SPRINT-9-openclaw-support.md) (complete)
- [SPRINT-10-coordinator.md](sprints/SPRINT-10-coordinator.md) (complete)

---

## Motivation

mvm provides both a production runtime (coordinator, agents, microVMs) and a
development environment for building Nix-backed microVM workloads. Sprint 10
completed the coordinator. The workspace migration consolidated the codebase
into 7 focused crates.

Today there are two build paths — `mvm build` (Mvmfile.toml → chroot → bake ELF)
and `mvm pool build` (Nix flake → ephemeral FC builder → pool artifacts). Both
require significant ceremony. An external developer has to learn the multi-tenant
object model (tenant → pool → instance) just to build and test a microVM image.

Sprint 11 makes mvm useful as a development tool for external projects:
- A developer points mvm at their Nix flake, builds a microVM image, and
  iterates quickly — no tenant/pool boilerplate required
- `mvm shell` becomes a first-class development environment with Firecracker and
  Nix tools on PATH
- The full coordinator → agent → instance pipeline can run locally for end-to-end
  testing
- The existing build pipelines become accessible through a simplified `mvm run`
  workflow

## What exists (pre-sprint)

**Dev mode:**
- `mvm shell` — drops into Lima VM (Nix + FC installed, `~` mounted writable)
- `mvm dev` — auto-bootstrap + launch single dev microVM + SSH
- `mvm setup --recreate` — rebuild rootfs from scratch
- Non-root `mvm` user in guest with kvm group + passwordless sudo

**Mvmfile.toml build path (`mvm build`):**
- `image.rs` — chroot-based build: apt install, run commands, inject services
- Packages into self-contained ELF via `bake` tool
- `mvm start <elf>` — runs built image with resource/volume overrides
- `RuntimeConfig` TOML for runtime defaults (cpus, memory, volumes)

**Nix flake build path (`mvm pool build`):**
- `build.rs` — ephemeral FC builder VM: boots, runs `nix build`, extracts artifacts
- `nix_manifest.rs` — `mvm-profiles.toml` resolves (role, profile) → Nix modules
- Requires tenant + pool to exist first
- Artifacts stored per-revision in `/var/lib/mvm/tenants/.../artifacts/revisions/`

**Coordinator + Agent:**
- `mvm coordinator serve --config <toml>` — TCP proxy with on-demand wake
- `mvm agent serve` — reconcile loop + QUIC API
- Full lifecycle: create → build → scale → wake/sleep

## Baseline

| Metric            | Value           |
| ----------------- | --------------- |
| Workspace crates  | 7 + root facade |
| Lib tests         | 366             |
| Integration tests | 10              |
| Total tests       | 376             |
| Clippy warnings   | 0               |
| Tag               | v0.2.0          |

---

## Phase 1: Dev Shell Polish
**Status: COMPLETE**

Make `mvm shell` a productive development environment. Currently it drops into
a bare Lima shell — developers must manually source Nix and know where tools
live.

- [x] Lima provisioning: add Firecracker to PATH (`/etc/profile.d/mvm-tools.sh`)
- [x] Lima provisioning: source Nix profile automatically in login shells
  (`/etc/profile.d/mvm-nix.sh`)
- [x] `mvm shell` prints a welcome banner with Firecracker, Nix, and Lima versions
- [x] `mvm shell --project <path>` option: cd into the project directory inside
  the VM (Lima maps `~` → `~`)
- [x] `mvm status` shows Nix version and Firecracker version when Lima is running
- [x] Tests: shell help (--project flag), lima template (nix profile, mvm-tools)

## Phase 2: Simplified Build Workflow
**Status: COMPLETE**

A developer with a Nix flake should be able to build a microVM image without
creating tenants or pools. Introduce `mvm build --flake <ref>` as a thin
wrapper that handles the boilerplate.

- [x] `mvm build --flake <ref> [--profile <p>] [--role <r>]` — new CLI mode
  alongside existing Mvmfile.toml path
- [x] Under the hood: runs `nix build` directly in Lima VM, stores artifacts
  in a dev workspace (`/var/lib/mvm/dev/builds/<hash>/`)
- [x] Dev build artifacts: vmlinux + rootfs.ext4 (no full pool lifecycle needed)
- [x] `mvm build --flake . --profile minimal` works from inside a project
  directory
- [x] Print the artifact path on success so it can be passed to `mvm run`
- [x] Idempotent: re-running the same build with same Nix store hash is a
  cache hit
- [x] Tests: build command parsing, dev workspace path generation, cache check,
  manifest resolution, flake ref resolution

## Phase 3: Run + Iterate
**Status: PENDING**

Close the build → run → test → modify → rebuild loop. Introduce `mvm run` that
combines build + start for rapid iteration.

- [ ] `mvm run --flake <ref> [--profile <p>] [--cpus N] [--mem N]` — builds
  (or uses cached) then boots a local Firecracker VM with the result
- [ ] Uses the dev mode TAP/NAT network (172.16.0.x) — no tenant bridge needed
- [ ] `mvm run` drops into SSH on the booted VM (like `mvm dev` does)
- [ ] `mvm run --detach` — boots in background, prints connection info
- [ ] `mvm stop` works for both dev mode and `mvm run` instances
- [ ] `mvm status` distinguishes dev-mode VM from flake-built VMs
- [ ] Support `RuntimeConfig` TOML for persistent resource/volume overrides
- [ ] Tests: run command parsing, status reporting

## Phase 4: Local Coordinator Testing
**Status: PENDING**

Enable the full coordinator → agent → instance pipeline on a single developer
machine for end-to-end testing.

- [ ] `mvm dev cluster init` — generates a local coordinator config + dev CA
  certs + desired state file
  - Single agent node at `127.0.0.1:4433`
  - Coordinator listening on localhost ports
  - Dev tenant + gateway pool + worker pool
- [ ] `mvm dev cluster up` — starts agent + coordinator in background
  - `mvm agent serve` running as background process
  - `mvm coordinator serve` running as background process
  - Reconcile creates instances from desired state
- [ ] `mvm dev cluster status` — shows agent, coordinator, instances, routes
- [ ] `mvm dev cluster down` — graceful shutdown of all components
- [ ] Coordinator config template with sensible dev defaults (short timeouts,
  localhost bindings, auto-generated certs)
- [ ] Documented workflow: init → up → test requests → iterate → down
- [ ] Tests: config generation, cluster status parsing

## Phase 5: Build Pipeline Improvements
**Status: PENDING**

Make the Nix build pipeline more robust and useful for real external projects.

- [ ] Builder VM: install Nix during first boot (currently builder rootfs has
  no Nix — the build expects it pre-installed or uses the host's Nix)
- [ ] Builder VM: mount project directory from Lima via virtio-fs or 9p so
  `nix build .` works with local flakes
- [ ] Build progress streaming: show `nix build` output in real-time (not just
  final result)
- [ ] Build caching: detect when flake.lock hasn't changed and skip rebuild
- [ ] `mvm build --watch` — watch flake.lock for changes, auto-rebuild
- [ ] Builder resource tuning: `--builder-cpus`, `--builder-mem` flags
- [ ] Tests: builder artifact caching, resource flag parsing

---

## Non-goals (this sprint)

- **Multi-node deployment**: local dev only. Real multi-node clusters are
  future work.
- **Custom kernels**: builds use the upstream Firecracker kernel. Custom kernel
  support is out of scope.
- **GUI / web dashboard**: CLI-only. A web UI for the coordinator is future work.
- **Hot-reload inside guest**: rebuilds create new VMs. In-place code updates
  inside a running Firecracker guest are not supported.
- **Windows/WSL support**: Lima on macOS and native Linux only.

## Architecture

```
Developer machine (macOS or Linux)
  │
  ├─ mvm build --flake .     ← Phase 2: simplified build
  │    └─ Lima VM
  │         └─ Builder FC VM (ephemeral)
  │              └─ nix build → vmlinux + rootfs.ext4
  │
  ├─ mvm run --flake .       ← Phase 3: build + boot + SSH
  │    └─ Lima VM
  │         └─ Firecracker VM (dev network 172.16.0.x)
  │              └─ booted from build artifacts
  │
  └─ mvm dev cluster up      ← Phase 4: local end-to-end
       └─ Lima VM
            ├─ Agent (QUIC API, reconcile loop)
            ├─ Coordinator (TCP proxy, wake manager)
            └─ Firecracker VMs (tenant networks 10.240.x.x)
```
