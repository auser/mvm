# mvm Sprint 4: Security Baseline (90%)

Previous sprints:
- [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete)
- [SPRINT-2-production-readiness.md](sprints/SPRINT-2-production-readiness.md) (complete)
- [SPRINT-3-real-world-validation.md](sprints/SPRINT-3-real-world-validation.md) (complete)

Sprints 1-3 built the full multi-tenant system with observability, CLI polish, error handling, and operational tooling. Sprint 4 implements a "90% secure" production baseline — strict runtime isolation, network isolation, secrets hardening, data-at-rest encryption (LUKS for data volumes), hardened agent API, systemd sandboxing, installer verification, and snapshot hardening. Based on [specs/plans/6-security.md](plans/6-security.md) with pragmatic scoping.

---

## Phase 1: Runtime Isolation Hardening
**Status: NOT STARTED**

Make jailer mandatory in production mode, enforce cgroup limits, ensure clean teardown.

- [ ] `src/infra/config.rs` — add `PRODUCTION_MODE` env var check (`MVM_PRODUCTION=1`)
- [ ] `src/vm/instance/lifecycle.rs` — in production mode, refuse to start without jailer available
- [ ] `src/security/cgroups.rs` — verify cgroup writes succeed (read back `memory.max` after write)
- [ ] `src/security/cgroups.rs` — add `kill_cgroup_processes()` helper for reliable teardown
- [ ] `src/vm/instance/lifecycle.rs` — call `kill_cgroup_processes()` during stop before `rmdir`
- [ ] Agent reconcile — enforce per-tenant quotas before every start/wake/create (already done, verify)
- [ ] Tests: verify jailer refusal in production mode, cgroup teardown

## Phase 2: Secrets Hardening
**Status: NOT STARTED**

Secrets drive must be ephemeral, tmpfs-backed, and mounted with restrictive options.

- [ ] `src/vm/instance/disk.rs` — `create_secrets_disk()` use tmpfs-backed ext4 image
- [ ] `src/vm/instance/disk.rs` — set file permissions 0600 on secrets disk
- [ ] `src/vm/instance/fc_config.rs` — set `is_read_only: true` for secrets drive (vdc)
- [ ] Guest mount: document `ro,noexec,nodev,nosuid` mount options for `/run/secrets`
- [ ] `src/vm/instance/lifecycle.rs` — recreate fresh secrets disk on every start and wake
- [ ] Audit: verify secrets paths are never included in log/audit output
- [ ] Tests: verify secrets disk is recreated, permissions correct

## Phase 3: LUKS Data Volume Encryption
**Status: NOT STARTED**

Encrypt persistent tenant data volumes (vdb) with LUKS. No snapshot encryption yet.

- [ ] `src/security/encryption.rs` — new module:
  - `create_encrypted_volume(path, size_mib, key) -> Result<()>` — `cryptsetup luksFormat`
  - `open_encrypted_volume(path, name, key) -> Result<String>` — `cryptsetup luksOpen`, returns `/dev/mapper/<name>`
  - `close_encrypted_volume(name) -> Result<()>` — `cryptsetup luksClose`
  - `is_luks_volume(path) -> Result<bool>` — check if already LUKS formatted
- [ ] `src/security/keystore.rs` — new module:
  - `KeyProvider` trait: `get_data_key(tenant_id) -> Result<Vec<u8>>`
  - `EnvKeyProvider` — reads `MVM_TENANT_KEY_<TENANT_ID>` env var (dev/staging)
  - `FileKeyProvider` — reads from `/var/lib/mvm/keys/<tenant_id>.key` (node-local provisioning)
- [ ] `src/vm/instance/lifecycle.rs` — integrate LUKS:
  - `instance_start` → open LUKS volume before FC launch (if data disk configured)
  - `instance_stop` → close LUKS volume after FC shutdown
  - `instance_destroy` → close + wipe LUKS header
- [ ] Add `zeroize` crate for key material clearing
- [ ] Tests: unit tests for encryption module, key provider trait

## Phase 4: Agent API Hardening
**Status: NOT STARTED**

Lock down the coordinator→agent API to strictly declarative operations.

- [ ] `src/agent.rs` — add `#[serde(deny_unknown_fields)]` to `DesiredState`, `DesiredTenant`, `DesiredPool`
- [ ] `src/agent.rs` — cap `desired_counts` values (max 100 per pool per state)
- [ ] `src/agent.rs` — validate all IDs against `naming::validate_id()` in `validate_desired_state()`
- [ ] `src/agent.rs` — add request type logging with explicit DENY for any future imperative requests
- [ ] Document: API surface is strictly `Reconcile`, `NodeInfo`, `NodeStats`, `TenantList`, `InstanceList`, `WakeInstance` — nothing else
- [ ] Verify: no code path allows arbitrary command execution via the QUIC API

## Phase 5: Transport Security Defaults
**Status: NOT STARTED**

Ensure mTLS in production, allow token mode for dev/staging.

- [ ] `src/agent.rs` — in production mode, refuse to start `agent serve` without TLS certs
- [ ] `src/agent.rs` — add `--require-mtls` flag (default true in production)
- [ ] `src/infra/config.rs` — stable node-id: read/write `/var/lib/mvm/node_id` (already exists, verify)
- [ ] Document staging exception: token-based auth only on private interfaces

## Phase 6: Systemd Hardening
**Status: NOT STARTED**

Systemd unit file with sandboxing directives. Agent runs as root (privilege split deferred).

- [ ] `deploy/systemd/mvm-agent.service` — unit file with sandboxing
- [ ] `scripts/install-systemd.sh` — install unit, create dirs, set permissions
- [ ] `docs/systemd.md` — capabilities rationale, customization guide

## Phase 7: Installer Hardening
**Status: NOT STARTED**

SHA256-verified binary downloads.

- [ ] `scripts/mvm-install.sh` — download binary + `.sha256` checksum file
- [ ] Verify SHA256 before installing; abort on mismatch
- [ ] Support `--version` flag for pinned installs
- [ ] Never `curl | bash` in production docs — document download-then-verify flow

## Phase 8: Snapshot Hardening
**Status: NOT STARTED**

Secure snapshot storage without encryption (encryption deferred).

- [ ] `src/vm/instance/snapshot.rs` — set per-tenant snapshot dirs to 0700 root-only
- [ ] `src/vm/instance/snapshot.rs` — validate tenant_id in restore path (reject cross-tenant)
- [ ] `src/vm/instance/snapshot.rs` — path canonicalization before snapshot ops
- [ ] `src/vm/instance/snapshot.rs` — `snapshot_capabilities()` — detect FC version
- [ ] `src/vm/disk_manager.rs` — GC: zero-fill files before unlink
- [ ] Audit: log all snapshot create/restore/delete operations
- [ ] Tests: cross-tenant rejection, permission enforcement

## Phase 9: Network Verification Enhancement
**Status: NOT STARTED**

Strengthen `mvm net verify` checks.

- [ ] `src/vm/bridge.rs` — check no TAP attached to wrong tenant's bridge
- [ ] `src/vm/bridge.rs` — verify instance IPs are within coordinator-assigned subnets
- [ ] `src/vm/bridge.rs` — verify nftables rules deny cross-bridge forwarding
- [ ] `mvm net verify --deep` flag for rule content inspection
- [ ] Tests: mock-based verify tests

## Phase 10: Documentation Update
**Status: NOT STARTED**

- [ ] `docs/security.md` — security baseline overview, threat model, hardening checklist
- [ ] Update `docs/cli.md` — document `MVM_PRODUCTION=1`, `--require-mtls`
- [ ] Update `CLAUDE.md` — new modules (security/encryption, security/keystore)
- [ ] Document deferred items with rationale

---

## Implementation Order

```
Phase 1 (Runtime Isolation) → foundational, do first
Phase 2 (Secrets Hardening) → independent
Phase 3 (LUKS Encryption) → independent, can overlap with Phase 2
Phase 4 (API Hardening) → independent
Phase 5 (Transport Security) → depends on Phase 1 (production mode flag)
Phase 6 (Systemd) → independent
Phase 7 (Installer) → independent
Phase 8 (Snapshot Hardening) → independent
Phase 9 (Net Verify) → independent
Phase 10 (Documentation) → do last
```

## New Dependencies

```toml
zeroize = "1"      # Phase 3: secure key material clearing
```

## Explicitly Deferred

| Item | Reason | Future Sprint |
|------|--------|---------------|
| Privilege separation (agentd/hostd split) | High complexity, requires separate binaries + Unix socket RPC | Sprint 5 |
| Snapshot encryption | Depends on key management maturity; hardened permissions + GC covers 80% | Sprint 5 |
| Signed desired-state updates (Ed25519) | Requires coordinator-side signing infrastructure | Sprint 5 |
| Shared-bridge segmentation with nftables tagging | Per-tenant bridges already provide isolation by construction | Not planned |

## Future Sprints

### Sprint 5: Full Security (Deferred Items)
- Privilege separation (agentd/hostd split)
- Snapshot encryption (AES-256-GCM)
- Signed desired-state updates
- Full pre-flight checks module

### Guest Agent & vsock
- vsock guest agent for lifecycle signals
- Structured health probes over vsock
- Log streaming from guest to host

### Scale & Multi-Node
- Coordinator server
- Node registration
- Fleet-wide commands
- Cross-node tenant migration
