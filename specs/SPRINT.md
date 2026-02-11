# mvm Sprint 9: OpenClaw Support — Role + Wake API + Deploy Config

Previous sprints:
- [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete)
- [SPRINT-2-production-readiness.md](sprints/SPRINT-2-production-readiness.md) (complete)
- [SPRINT-3-real-world-validation.md](sprints/SPRINT-3-real-world-validation.md) (complete)
- Sprint 4: Security Baseline 90% (complete)
- Sprint 5: Final Security Hardening (complete)
- [SPRINT-6-minimum-runtime.md](sprints/SPRINT-6-minimum-runtime.md) (complete)
- [SPRINT-7-role-profiles.md](sprints/SPRINT-7-role-profiles.md) (complete)
- [SPRINT-8-integration-lifecycle.md](sprints/SPRINT-8-integration-lifecycle.md) (complete)

Sprint 9 adds OpenClaw as a first-class deployment target on mvm. OpenClaw is a
personal AI assistant gateway (Telegram/Discord -> Claude AI -> local CLI tools)
that runs as per-user worker VMs with sleep/wake on demand.

Full spec: [specs/plans/11-openclaw-support.md](plans/11-openclaw-support.md)

---

## Phase 1: CapabilityOpenclaw Role + Template Update
**Status: COMPLETE**

New `CapabilityOpenclaw` role that combines worker patterns (vsock, integration
manager, sleep-prep) with gateway patterns (config drive, secrets drive) plus
OpenClaw-specific services (Node.js gateway, environment assembler, wake handler).

- [x] `src/vm/pool/config.rs` — `CapabilityOpenclaw` variant in Role enum + Display + serde tests
- [x] `src/main.rs` — `parse_role()` accepts `capability-openclaw`, updated help text
- [x] `src/agent.rs` — `role_priority(CapabilityOpenclaw) = 3` (same as capabilities)
- [x] `src/vm/pool/nix_manifest.rs` — SAMPLE_TOML + resolve/requirements tests for new role
- [x] `nix/mvm-profiles.toml` — `[roles.capability-openclaw]` with config_drive + secrets_drive
- [x] `nix/roles/openclaw.nix` — **NEW** NixOS module: config drive mount, env assembler,
  Node.js gateway service, worker agent, integration manager, sleep-prep, wake handler,
  TCP keepalive sysctl, openclaw system user
- [x] `nix/flake.nix` — `tenant-capability-openclaw-{minimal,python}` outputs + `mvm-role-openclaw` module export
- [x] `src/templates.rs` — openclaw template workers changed to `Role::CapabilityOpenclaw`, mem_mib bumped to 2048

## Phase 2: Vsock Wake Protocol
**Status: COMPLETE**

Guest-to-host communication so gateway VMs can tell the host agent to wake sleeping worker VMs.

- [x] `src/worker/vsock.rs` — `HostBoundRequest` enum: `WakeInstance`, `QueryInstanceStatus`
- [x] `src/worker/vsock.rs` — `HostBoundResponse` enum: `WakeResult`, `InstanceStatus`, `Error`
- [x] `src/worker/vsock.rs` — `read_frame()` / `write_frame()` helpers for generic length-prefixed JSON
- [x] `src/worker/vsock.rs` — `HOST_BOUND_PORT = 53` constant
- [x] Tests: request/response serde roundtrip, port constant (3 new tests)

## Phase 3: Config File + Deploy Command
**Status: COMPLETE**

Config file for `mvm new --config` and standalone `mvm deploy manifest.toml`.

- [x] `src/templates.rs` — `DeployConfig`, `SecretRef`, `OverrideConfig`, `PoolOverride` types
- [x] `src/templates.rs` — `DeploymentManifest`, `ManifestTenant`, `ManifestPool` types
- [x] `src/main.rs` — `--config <path>` flag on `Commands::New`
- [x] `src/main.rs` — `Commands::Deploy { manifest, watch, interval }` command
- [x] `src/main.rs` — `cmd_new()` applies config overrides (flake, vcpus, mem, instances)
- [x] `src/main.rs` — `cmd_deploy()` creates tenant/pools from manifest, supports `--watch`
- [x] Tests: deploy config parse, minimal config, manifest parse, manifest defaults (4 new tests)

## Phase 4: Documentation
**Status: COMPLETE**

- [x] `docs/roles.md` — added CapabilityOpenclaw section
- [x] `docs/cli.md` — added `mvm new --config` and `mvm deploy` sections
- [x] `specs/SPRINT.md` — Sprint 9 current

---

## Summary

| Metric | Value |
|--------|-------|
| Lib tests | 315 (+19) |
| Integration tests | 10 |
| Total tests | 325 |
| Clippy warnings | 0 |
| New files | `nix/roles/openclaw.nix`, `specs/sprints/SPRINT-8-integration-lifecycle.md` |

## Files Created/Modified

| File | Changes |
|------|---------|
| `specs/plans/11-openclaw-support.md` | **NEW** — full OpenClaw support spec |
| `src/vm/pool/config.rs` | `CapabilityOpenclaw` variant in Role enum |
| `src/main.rs` | `parse_role`, `--config`, `Deploy` command, `cmd_deploy()` |
| `src/agent.rs` | `role_priority` for CapabilityOpenclaw |
| `src/vm/pool/nix_manifest.rs` | SAMPLE_TOML + tests for new role |
| `src/templates.rs` | Template update + DeployConfig + DeploymentManifest types |
| `nix/mvm-profiles.toml` | `[roles.capability-openclaw]` section |
| `nix/roles/openclaw.nix` | **NEW** — OpenClaw NixOS role module |
| `nix/flake.nix` | Flake outputs + nixosModules for openclaw |
| `src/worker/vsock.rs` | HostBoundRequest/Response + frame helpers |
| `docs/roles.md` | CapabilityOpenclaw documentation |
| `docs/cli.md` | `mvm deploy` and `--config` docs |
