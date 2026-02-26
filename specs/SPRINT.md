# mvm Sprint 12: Install & Release Reliability

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
- [SPRINT-11-dev-environment.md](sprints/SPRINT-11-dev-environment.md) (complete)

---

## Motivation

We hardened dev workflows in Sprint 11 but saw recurring friction around sync/bootstrap and release packaging (crates.io, GH Actions). Sprint 12 focuses on making installation, syncing, and publishing reliable on both macOS (Lima) and native Linux, with better diagnostics and documented escape hatches.

## Baseline

| Metric            | Value           |
| ----------------- | --------------- |
| Workspace crates  | 5 + root facade |
| Lib tests         | 366             |
| Integration tests | 10              |
| Total tests       | 376             |
| Clippy warnings   | 0               |
| Tag               | v0.3.0          |

---

## Phase 1: Sync/Bootstrap Hardening
**Status: COMPLETE**

- [x] Detect Lima presence/absence more robustly; avoid `limactl` calls inside guest
- [x] Make rustup/cargo pathing resilient (no `.cargo/env` required); add self-check
- [x] Add `mvm sync doctor` that reports deps (rustup, cargo, nix, firecracker, limactl)
- [ ] Add regression tests for sync on macOS host + Lima guest + native Linux

## Phase 2: Release + Publish Reliability
**Status: COMPLETE**

- [x] Dry-run and live crates.io publish via GH Actions (publish-crates workflow) — removed stale mvm-agent/mvm-coordinator from pipeline
- [x] Version bump tool/guard: `deploy-guard.sh` verifies workspace version, git tag, no hardcoded versions, inter-crate dep consistency, clippy
- [x] Release artifacts: SHA256 checksums generated per-platform and combined into `checksums-sha256.txt`; installer verifies checksums
- [x] Add a `mvm release --dry-run` command that exercises publish checks locally (also `--guard-only` for fast pre-publish verification)
- [x] Removed mvm-agent and mvm-coordinator crates (belong in mvmd repo, not dev CLI)
- [x] Fixed `mvm-install.sh` to match release archive format (tar.gz + target triples)

## Phase 2b: Global Templates (shared images, tenant-scoped pools)
**Status: COMPLETE**

- [x] Add `template` CLI group (create/list/info/delete/build) and global cache under `/var/lib/mvm/templates/<template>/`
- [x] Add `TemplateSpec`/`TemplateRevision` types and path helpers in `mvm-core`
- [x] Make `pool create` require `--template`; `pool build` reuses template artifacts (template `current` copied into pool). `--force` on pool rebuilds template first.
- [x] Config-driven template builds (`mvm template build --config template.toml`) to emit multiple role variants
- [x] Template build cache key on flake.lock/profile/role; `template_build()` now computes actual `nix hash path flake.lock` instead of using revision hash. Pool build links artifacts via cache key match, no per-tenant rebuild.
- [x] Doc polish — template CLI reference added to `docs/user-guide.md` (scaffold, create, build, config-driven variants, registry push/pull/verify, pool integration)
- ~~Migration helper~~ deferred (no existing pools to migrate)

## Phase 2c: Vsock CLI & Guest Agent
**Status: COMPLETE**

- [x] Add `mvm vm ping <name>` and `mvm vm status <name> [--json]` CLI commands
- [x] Enable vsock device (`PUT /vsock`) in dev-mode Firecracker configuration
- [x] Add VSOCK column to `mvm status` multi-VM table
- [x] Lima delegation for vsock commands (macOS → Lima VM re-invocation)
- [x] Fix vsock socket permissions after VM start (`chmod 0666`)
- [x] Create `mvm-guest-agent` binary with real system monitoring (load sampling, idle/busy detection)
- [x] Guest agent handles: Ping, WorkerStatus, SleepPrep (sync + drop caches), Wake
- [x] Guest agent accepts config file (`/etc/mvm/agent.json`) and CLI flags (`--port`, `--busy-threshold`, `--sample-interval`)
- [x] Shared NixOS module (`nix/modules/guest-agent.nix`) and package (`nix/modules/guest-agent-pkg.nix`)
- [x] OpenClaw flake imports guest agent module; agent starts automatically on boot
- [x] Template scaffold emits guest agent module files on `mvm template create`
- [x] CLI integration tests for `mvm vm` subcommands (help, parsing, graceful errors)
- [x] Add `rust-overlay` to Nix flakes for Rust 1.85+ (edition 2024 support)
- [x] Rebuild images with guest agent and validate `mvm vm ping` end-to-end

## Phase 3: Installer/Setup UX
**Status: COMPLETE**

- [x] Make `mvm setup`/`bootstrap` idempotent with clear re-run messaging and `--force` flag
- [x] Preflight check for KVM, virtualization, disk space, Lima status; actionable guidance via expanded `mvm doctor`
- [x] Improve error surfaces (`with_hints` wrapper for common failures: missing tools, KVM, permissions, Nix)
- [x] `mvm doctor --json` for machine-readable diagnostics
- [x] Create `docs/quickstart.md` with known-good host matrix, install steps, and troubleshooting

## Phase 4: Observability & Logs
**Status: PENDING**

- [ ] Structured logs for sync/build (timestamps, phases) with `--json` flag
- [ ] Capture and surface builder VM logs when nix build fails
- [x] Add `mvm doctor` summary (reuses sync doctor) to show overall health (done in Phase 3)

## Phase 5: QA & Documentation
**Status: PENDING**

- [ ] CLI help/examples refreshed for new flags (force, builder resources, doctor)
- [ ] Update sprint README/CHANGELOG section for release notes
- [ ] Add one end-to-end test covering: sync → build --flake → run --config

---

## Non-goals (this sprint)

- Multi-node deployment or cloud installers
- UI/dashboard work
- New feature areas outside install/release reliability

## Success criteria

- `cargo run -- sync` succeeds on macOS host + Lima guest and native Linux without manual fixes
- publish-crates GH workflow completes a dry-run and one live publish for the tagged version
- Documentation reflects install/release workflow and troubleshooting
