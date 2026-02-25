# mvm — Sprint 13: Ship It

## Context

After a full codebase audit (7 crates, ~27,000 LOC, 385+ tests, 0 clippy warnings, sprints 1-12 in progress), the project is substantially more complete than Sprint 12's pending checklist suggests. Many items marked "pending" already have working implementations — CI workflows, release tooling, template migration, deploy guard, Etcd state store.

**The core system is production-ready for single-node fleets.** What remains is 6 genuine code gaps, essential-path integration tests (~20 new tests), and operational documentation. This sprint closes those gaps and produces a shippable v0.3.0.

## Baseline

| Metric            | Value           |
| ----------------- | --------------- |
| Workspace crates  | 7 + root facade |
| Lib tests         | 366             |
| Integration tests | 19              |
| Total tests       | 385             |
| Clippy warnings   | 0               |
| Tag               | v0.2.0          |
| Only TODO in code | `crates/mvm-coordinator/src/server.rs:34` (Etcd config wiring) |

---

## What's Already Done (Correcting Sprint 12)

These were marked pending in Sprint 12 but are actually implemented:

- [x] **CI workflows**: ci.yml, release.yml, publish-crates.yml, pages.yml all exist and work
- [x] **Deploy guard**: `scripts/deploy-guard.sh` verifies tag matches workspace version
- [x] **Publish workflow**: `publish-crates.yml` publishes in dependency order with dry-run support
- [x] **Release artifacts**: `release.yml` builds 4 platform targets + GitHub Release
- [x] **Installer**: `install.sh` with dev/node/coordinator modes, platform detection
- [x] **Template migrate-from-pool**: `template_cmd.rs:341` — fully implemented
- [x] **Template push/pull/verify**: `lifecycle.rs` with SHA256 checksums and S3 registry
- [x] **Etcd state store**: `state.rs` — full `EtcdStateStore` implementation
- [x] **Doctor command**: checks rustup, cargo, limactl, nix, firecracker
- [x] **Bootstrap idempotency**: each step checks if already done before acting
- [x] **Systemd units**: agent, agentd, hostd service files exist

---

## Phase 1: Close Code Gaps (3-5 days)

### 1a. Template cache key composite check
**Status: PENDING**

`TemplateRevision` records `flake_lock_hash`, `profile`, and `role` individually, but `template_reuse.rs` doesn't compare all three when deciding to reuse artifacts. A pool with a different profile could incorrectly reuse a template built for another profile.

- [ ] Add `cache_key() -> String` method to `TemplateRevision` that computes `sha256(flake_lock_hash + profile + role)`
- [ ] In `template_reuse::reuse_template_artifacts()`, compare cache keys before copying
- [ ] Add unit test: two revisions with same flake but different profiles produce different cache keys

**Files:**
- `crates/mvm-core/src/template.rs` — add `cache_key()` method
- `crates/mvm-build/src/template_reuse.rs` — use cache key in skip logic

### 1b. Wire Etcd config in coordinator
**Status: PENDING**

The `EtcdStateStore` is fully implemented but the server hard-codes `MemStateStore`.

- [ ] Add `etcd_endpoints: Option<Vec<String>>` and `etcd_prefix: Option<String>` to `CoordinatorConfig` (with `#[serde(default)]`)
- [ ] In `server.rs:32-35`, if `etcd_endpoints` is `Some`, instantiate `EtcdStateStore::connect()` instead of `MemStateStore::new()`
- [ ] Add test: config with etcd_endpoints parses correctly

**Files:**
- `crates/mvm-coordinator/src/config.rs` — add fields
- `crates/mvm-coordinator/src/server.rs` — conditional store creation

### 1c. Surface builder VM failure logs
**Status: PENDING**

When `nix build` fails inside the builder VM, the user sees a generic error. The actual Nix error output is lost.

- [ ] In SSH backend (`backend/ssh.rs`), on build failure, capture stderr and include last 50 lines in error context
- [ ] In vsock backend (`vsock_builder.rs`), collect `Log { line }` frames and include in error on failure
- [ ] Test: trigger a build error path and verify error message contains build output

**Files:**
- `crates/mvm-build/src/backend/ssh.rs`
- `crates/mvm-build/src/vsock_builder.rs`

### 1d. Add `--log-format` global CLI flag
**Status: PENDING**

The JSON logging layer exists in `mvm-core::observability::logging` but isn't exposed via CLI.

- [ ] Add `--log-format <human|json>` global flag to `Cli` struct in `commands.rs`
- [ ] Pass to `logging::init()` in `run()`

**Files:**
- `crates/mvm-cli/src/commands.rs` — add flag
- `crates/mvm-cli/src/logging.rs` — wire through

### 1e. Sync regression tests
**Status: PENDING**

Sprint 12 Phase 1 is done but missing the regression tests.

- [ ] Test Lima detection logic (present vs absent) using mock
- [ ] Test rustup/cargo path resilience (no `.cargo/env`)
- [ ] Test doctor report output format

**Files:**
- `crates/mvm-cli/src/doctor.rs` — add `#[cfg(test)]` module

---

## Phase 2: Essential-Path Integration Tests (~20 tests, 1 week)

### 2a. Instance lifecycle test (5 tests)
Using `shell_mock` infrastructure in `crates/mvm-runtime/src/shell_mock.rs`:

- [ ] `test_full_lifecycle_happy_path` — create → start → warm → sleep → wake → stop → destroy
- [ ] `test_invalid_transition_rejected` — Running → Sleeping (invalid, must go through Warm)
- [ ] `test_quota_enforcement` — start fails when tenant quota exceeded
- [ ] `test_instance_destroy_cleanup` — verify TAP, cgroup, disks cleaned up
- [ ] `test_network_identity_preserved` — IP and MAC same after sleep/wake

**Files:**
- `crates/mvm-runtime/tests/lifecycle.rs` (new integration test file)

### 2b. Agent reconcile test (5 tests)

- [ ] `test_reconcile_scale_up` — desired 3 running, actual 0 → creates 3
- [ ] `test_reconcile_scale_down` — desired 1 running, actual 3 → stops 2
- [ ] `test_reconcile_wake_sleeping` — desired 2 running, actual 0 running + 2 sleeping → wakes 2
- [ ] `test_reconcile_signed_required` — unsigned reconcile rejected in production mode
- [ ] `test_reconcile_quota_limit` — reconcile respects tenant quota during scale-up

**Files:**
- `crates/mvm-agent/tests/reconcile.rs` (new integration test file)

### 2c. Build pipeline test (5 tests)

- [ ] `test_cache_hit_skips_build` — flake.lock unchanged → no build
- [ ] `test_template_reuse_skips_build` — matching template → artifacts copied, no build
- [ ] `test_cache_key_mismatch_triggers_build` — different profile → forces rebuild
- [ ] `test_force_rebuild_ignores_cache` — `--force` always rebuilds
- [ ] `test_build_revision_recorded` — after build, revision.json exists with correct metadata

**Files:**
- `crates/mvm-build/tests/pipeline.rs` (new integration test file)

### 2d. Coordinator test (3 tests)

- [ ] `test_wake_coalescing` — 3 concurrent requests for same tenant share one wake
- [ ] `test_idle_sweep` — connection closes → idle timer starts → state transitions to Idle
- [ ] `test_route_lookup` — configured routes resolve correctly

**Files:**
- `crates/mvm-coordinator/tests/routing.rs` (new integration test file)

### 2e. CLI integration tests (2 tests)

- [ ] `test_tenant_pool_instance_commands` — create/list/info/destroy for all three levels
- [ ] `test_template_lifecycle_commands` — create/list/info/build/delete

**Files:**
- `crates/mvm-cli/tests/cli.rs` (extend existing)

---

## Phase 3: Operational Documentation (3-5 days)

### 3a. Deployment guide
- [ ] Write `docs/deployment.md`:
  - Single-node deployment flow (`install.sh node` → systemd → agent serve)
  - Multi-node: coordinator config + N agent nodes
  - TLS certificate setup (`mvm agent certs init`)
  - Etcd cluster for coordinator state persistence
  - Systemd service management reference
  - Environment variable reference (all `MVM_*` vars)

### 3b. Troubleshooting runbook
- [ ] Write `docs/runbook.md`:
  - Instance stuck in Warm/Sleeping → `mvm instance stop --force`
  - Build failures → inspect builder logs, clear cache, `mvm template build --force`
  - Network issues → `mvm net verify --deep`, bridge/TAP diagnostics
  - Stale PIDs → `mvm doctor`, health check detection
  - LUKS key rotation procedure
  - Coordinator failover (switch to standby, Etcd state)

### 3c. CHANGELOG and release
- [ ] Write `CHANGELOG.md` with entries for sprints 1-13
- [ ] Version bump to v0.3.0 across workspace
- [ ] Tag, push, verify publish-crates dry-run passes

---

## Reference: How Nix Builds Work

### Build Flow

```
mvm template build <name>  (or)  mvm pool build <tenant>/<pool>
     │
     ├─ 1. Cache check ─── hash flake.lock → compare to last_flake_lock.hash
     │      └─ Match? → SKIP (fast path)
     │
     ├─ 2. Template check ─── pool has template_id?
     │      └─ Template current revision matches (profile, role, resources)?
     │         └─ Yes? → COPY artifacts from template (fast path)
     │
     ├─ 3. Backend select ─── MVM_BUILDER_MODE env (default: "auto")
     │      └─ Auto: try vsock → fall back to SSH
     │
     ├─ 4. Boot ephemeral Firecracker VM
     │      ├─ 4 vCPUs, 4 GiB RAM (configurable)
     │      ├─ Output disk: 8 GiB ext4 → /build-out
     │      ├─ Input disk: local flake → /build-in (optional)
     │      └─ Vsock CID 3 (for vsock backend)
     │
     ├─ 5. Run nix build inside VM
     │      ├─ Vsock: mvm-builder-agent receives Build{flake, attr, timeout}
     │      │    └─ nix build <flake>#packages.<system>.<profile>
     │      └─ SSH: host SSHes in, syncs flake, runs nix build
     │
     ├─ 6. Extract artifacts from output disk
     │      ├─ vmlinux (kernel)
     │      ├─ rootfs.ext4 (root filesystem)
     │      └─ fc-base.json (Firecracker base config)
     │
     ├─ 7. Record revision
     │      ├─ Copy to <artifacts>/revisions/<hash>/
     │      ├─ Atomic symlink update: current → revisions/<hash>
     │      └─ Save flake.lock hash for future cache checks
     │
     └─ 8. Cleanup: kill builder VM, teardown TAP, remove run dir
```

### Template Registry (S3/MinIO)

```bash
# Push built template to registry (with SHA256 integrity)
mvm template push <name>

# Pull template on another node
mvm template pull <name>

# Verify local integrity
mvm template verify <name>
```

Config via env: `MVM_TEMPLATE_REGISTRY_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`

---

## Reference: Runtime Configuration and Security

### Configuring microVMs

```bash
# 1. Create tenant (network + quota boundary)
mvm tenant create acme --net-id 3 --subnet 10.240.3.0/24 \
  --max-vcpus 64 --max-mem 65536

# 2. Build a template (shared across pools)
mvm template create base --flake github:org/repo --profile minimal \
  --role worker --cpus 2 --mem 1024
mvm template build base

# 3. Create pool under tenant (references template)
mvm pool create acme/workers --template base --cpus 2 --mem 1024

# 4. Scale instances
mvm pool scale acme/workers --running 5 --warm 2 --sleeping 10

# 5. Or reconcile via agent (production)
mvm agent reconcile --desired desired-state.json
mvm agent serve  # Long-running daemon with QUIC+mTLS API
```

### Configuration Delivery (no SSH)

| Drive | Contents | Lifecycle | Permissions |
|-------|----------|-----------|-------------|
| **Config disk** | instance_id, pool_id, tenant_id, guest_ip, vcpus, mem_mib, runtime_policy | Read-only, recreated per boot | 0444 |
| **Secrets disk** | tenant secrets.json | Ephemeral (tmpfs), recreated per boot, deleted on stop | 0400, read-only mount |
| **Vsock** | Host ↔ guest communication | Always available (port 52) | No network exposure |

### Security Stack (applied at instance start)

```
instance_start()
  │
  ├── 1. State machine validation (enforce valid transitions)
  ├── 2. Quota check (tenant vCPU/memory limits)
  ├── 3. Bridge setup (per-tenant L2 isolation)
  ├── 4. TAP device (unique per instance, attached to tenant bridge)
  ├── 5. Cgroups v2 (memory.max, cpu.max, pids.max)
  ├── 6. Data disk (optional LUKS encryption with AES-256-XTS)
  ├── 7. Secrets disk (tmpfs-backed, ephemeral, read-only)
  ├── 8. Config disk (read-only metadata)
  ├── 9. FC config generation (boot args, drives, vsock)
  ├── 10. Seccomp (BPF filter, ~33 allowed syscalls in strict mode)
  ├── 11. Jailer (chroot + uid/gid isolation, production mode)
  ├── 12. Metadata endpoint (nftables DNAT, optional)
  └── 13. Audit log entry
```

### Security Configuration Reference

| Control | How to enable | Default |
|---------|--------------|---------|
| Network isolation | `--net-id` + `--subnet` on tenant create | Always (per-tenant bridges) |
| Resource limits | `--cpus`, `--mem` on pool/template create | Always (cgroups v2) |
| Tenant quotas | `--max-vcpus`, `--max-mem` on tenant create | No limit if unset |
| Jailer chroot | `MVM_PRODUCTION=1` env | Off in dev mode |
| Seccomp strict | `seccomp_policy: strict` in pool spec | Baseline (FC built-in) |
| Disk encryption | `MVM_TENANT_KEY_<TENANT>` env or key in `/var/lib/mvm/keys/` | Off |
| Snapshot encryption | Automatic when tenant key present | Off (follows disk encryption) |
| Signed state | Use `ReconcileSigned` API endpoint | Required in production |
| mTLS | `mvm agent certs init` + coordinator config | Required for QUIC |
| Audit logging | Automatic | Always on |

### Privilege Separation

```
mvm-agentd (user=mvm, unprivileged)
  ├── QUIC API → receives signed desired state
  ├── Validates + reconciles
  └── IPC to hostd ──→ /var/run/mvm/hostd.socket

mvm-hostd (root, minimal)
  └── Executes: start/stop/sleep/wake/destroy, bridge/TAP setup
```

---

## Success Criteria

- [ ] All 6 code gaps closed (Phase 1)
- [ ] ~20 new integration tests passing (Phase 2)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo test --workspace` all green
- [ ] `docs/deployment.md` and `docs/runbook.md` exist
- [ ] `CHANGELOG.md` written, version bumped to v0.3.0
- [ ] `publish-crates` dry-run passes
- [ ] Manual smoke test: template build → pool create → instance lifecycle on Lima

## Non-goals (this sprint)

- Multi-node deployment testing
- UI/dashboard
- Prometheus metrics endpoint (infrastructure exists, wiring deferred)
- Performance benchmarking
- Cloud-specific installers

---

## Claude Prompts (copy-paste for each task)

Start a new Claude Code session for each task. Each prompt is self-contained.

### Phase 1: Code Gaps

**1a. Template cache key:**
```
In mvm-core/src/template.rs, add a `cache_key() -> String` method to `TemplateRevision` that computes sha256 of (flake_lock_hash + profile + role). Then in mvm-build/src/template_reuse.rs, update reuse_template_artifacts() to compare cache keys before copying artifacts — if the cache key doesn't match, don't reuse. Add a unit test showing two revisions with same flake but different profiles produce different cache keys. Run clippy and tests when done.
```

**1b. Wire Etcd config:**
```
In crates/mvm-coordinator/src/config.rs, add `etcd_endpoints: Option<Vec<String>>` and `etcd_prefix: Option<String>` fields to CoordinatorConfig (with #[serde(default)]). Then in crates/mvm-coordinator/src/server.rs at line 32-35 where it says "TODO: Add config support for EtcdStateStore", make it conditional: if etcd_endpoints is Some, call EtcdStateStore::connect() instead of MemStateStore::new(). Add a test that config with etcd_endpoints parses correctly. Run clippy and tests when done.
```

**1c. Surface builder failure logs:**
```
When nix build fails inside the builder VM, the user gets a generic error without seeing the Nix error output. Fix this in two places:
1. In crates/mvm-build/src/backend/ssh.rs — on build failure, capture stderr and include the last 50 lines in the error context
2. In crates/mvm-build/src/vsock_builder.rs — collect Log { line } frames during the build and include them in the error message on failure
Run clippy and tests when done.
```

**1d. Add --log-format CLI flag:**
```
The JSON logging layer exists in mvm-core::observability::logging but isn't exposed via CLI. Add a `--log-format <human|json>` global flag to the Cli struct in crates/mvm-cli/src/commands.rs and pass it to logging::init() in crates/mvm-cli/src/logging.rs during startup. Default should be "human". Run clippy and tests when done.
```

**1e. Sync regression tests:**
```
Add regression tests in crates/mvm-cli/src/doctor.rs (as a #[cfg(test)] module) that cover:
1. Lima detection logic (present vs absent)
2. Rustup/cargo path resilience (no .cargo/env needed)
3. Doctor report output format verification
Use mocking where needed to avoid requiring actual Lima. Run clippy and tests when done.
```

### Phase 2: Integration Tests

**2a. Instance lifecycle tests:**
```
Create crates/mvm-runtime/tests/lifecycle.rs with 5 integration tests using the shell_mock infrastructure from crates/mvm-runtime/src/shell_mock.rs:
1. test_full_lifecycle_happy_path — create → start → warm → sleep → wake → stop → destroy
2. test_invalid_transition_rejected — Running → Sleeping (must go through Warm first)
3. test_quota_enforcement — start fails when tenant quota exceeded
4. test_instance_destroy_cleanup — verify TAP, cgroup, disks cleaned up
5. test_network_identity_preserved — IP and MAC same after sleep/wake
Read the shell_mock.rs and instance/lifecycle.rs code first to understand the patterns. Run clippy and tests when done.
```

**2b. Agent reconcile tests:**
```
Create crates/mvm-agent/tests/reconcile.rs with 5 integration tests:
1. test_reconcile_scale_up — desired 3 running, actual 0 → creates 3
2. test_reconcile_scale_down — desired 1 running, actual 3 → stops 2
3. test_reconcile_wake_sleeping — desired 2 running, 0 running + 2 sleeping → wakes 2
4. test_reconcile_signed_required — unsigned reconcile rejected in production mode
5. test_reconcile_quota_limit — reconcile respects tenant quota during scale-up
Read the agent.rs reconcile loop and existing tests first to understand patterns. Run clippy and tests when done.
```

**2c. Build pipeline tests:**
```
Create crates/mvm-build/tests/pipeline.rs with 5 integration tests:
1. test_cache_hit_skips_build — flake.lock unchanged → no build
2. test_template_reuse_skips_build — matching template → artifacts copied, no build
3. test_cache_key_mismatch_triggers_build — different profile → forces rebuild
4. test_force_rebuild_ignores_cache — --force always rebuilds
5. test_build_revision_recorded — after build, revision.json exists with correct metadata
Read the existing test infrastructure (FakeEnv in build.rs, cache.rs tests) first. Run clippy and tests when done.
```

**2d. Coordinator tests:**
```
Create crates/mvm-coordinator/tests/routing.rs with 3 integration tests:
1. test_wake_coalescing — 3 concurrent requests for same tenant share one wake operation
2. test_idle_sweep — connection closes → idle timer starts → state transitions to Idle
3. test_route_lookup — configured routes resolve correctly
Read server.rs, wake.rs, idle.rs, and routing.rs first to understand the patterns. Run clippy and tests when done.
```

**2e. CLI integration tests:**
```
Extend crates/mvm-cli/tests/cli.rs with 2 new integration tests:
1. test_tenant_pool_instance_commands — create/list/info/destroy for all three entity levels
2. test_template_lifecycle_commands — create/list/info/build/delete
Use assert_cmd patterns consistent with the existing tests in that file. Run clippy and tests when done.
```

### Phase 3: Documentation

**3a. Deployment guide:**
```
Write docs/deployment.md covering: single-node deployment (install.sh node → systemd → agent serve), multi-node (coordinator + N agents), TLS certificate setup (mvm agent certs init), Etcd cluster for coordinator persistence, systemd service management, and environment variable reference for all MVM_* vars. Follow the style of existing docs in docs/ directory. Read docs/architecture.md and docs/security.md for reference.
```

**3b. Troubleshooting runbook:**
```
Write docs/runbook.md covering common failure scenarios: instance stuck in Warm/Sleeping (force-stop), build failures (inspect logs, clear cache, force rebuild), network issues (mvm net verify --deep, bridge/TAP diagnostics), stale PIDs (mvm doctor), LUKS key rotation, and coordinator failover. Follow the style of existing docs. Read the CLI commands and error handling code for accurate guidance.
```

**3c. CHANGELOG and release:**
```
Write CHANGELOG.md with entries for sprints 1-13 (read specs/sprints/ for history). Then bump the version to v0.3.0 in root Cargo.toml and verify it propagates. Run `cargo clippy --workspace -- -D warnings && cargo test --workspace` to verify everything passes. Commit the version bump and changelog.
```
