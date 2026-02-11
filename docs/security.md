# Security Baseline

mvm implements a "90% secure" production baseline. This document covers the threat model, hardening measures, and what's explicitly deferred.

## Threat Model

**Attacker profiles:**
1. **Malicious tenant** — compromised guest VM attempting to escape or access other tenants
2. **Network attacker** — MitM on coordinator-agent link
3. **Local attacker** — non-root user on the host attempting to read tenant data

**Security boundaries:**
- Firecracker VMM provides the primary isolation boundary (KVM-based)
- Per-tenant bridges provide network isolation by construction
- Jailer provides chroot + UID/GID isolation per instance
- mTLS provides transport authentication and encryption

## Hardening Measures

### Runtime Isolation (Phase 1)

- **Production mode**: Set `MVM_PRODUCTION=1` to enforce strict security
- **Jailer mandatory**: In production mode, instances cannot start without the Firecracker jailer
- **Cgroup enforcement**: memory.max, cpu.max, pids.max limits with read-back verification
- **Clean teardown**: `kill_cgroup_processes()` ensures no orphaned processes after instance stop

### Secrets Hardening (Phase 2)

- **tmpfs-backed**: Secrets disk image is built in `/dev/shm` (never touches persistent storage)
- **Permissions**: Secrets disk file is chmod 0600, internal `secrets.json` is chmod 0400
- **Read-only mount**: Secrets drive is `is_read_only: true` in Firecracker config
- **Ephemeral**: Secrets disk is recreated fresh on every `start` and `wake`, deleted on `stop`
- **Guest mount**: Should be mounted with `ro,noexec,nodev,nosuid` at `/run/secrets`

### LUKS Data Volume Encryption (Phase 3)

- **AES-256-XTS**: Data volumes (vdb) encrypted with LUKS2 using `cryptsetup`
- **Key providers**: `EnvKeyProvider` (dev: `MVM_TENANT_KEY_<ID>` env vars) or `FileKeyProvider` (production: `/var/lib/mvm/keys/<tenant>.key`)
- **Key handling**: Keys passed via stdin/hex encoding (never on command line or in logs)
- **Lifecycle integration**: LUKS open on instance start, close on stop, close+wipe on destroy
- **Optional**: Encryption only activates when a key is available for the tenant

### Agent API Hardening (Phase 4)

- **Strict deserialization**: All desired state structs use `#[serde(deny_unknown_fields)]`
- **Count caps**: Maximum 100 instances per pool per state (running/warm/sleeping)
- **ID validation**: All tenant and pool IDs validated with `naming::validate_id()`
- **Typed protocol**: `AgentRequest` enum prevents any code execution — only 6 declarative operations exist
- **API surface**: Reconcile, NodeInfo, NodeStats, TenantList, InstanceList, WakeInstance

### Transport Security (Phase 5)

- **Production mTLS**: In production mode, `agent serve` refuses to start without TLS certificates
- **Certificate management**: `mvm agent certs init/request/rotate/status` commands
- **Dev exception**: Token-based auth allowed only in non-production mode on private interfaces

### Systemd Hardening (Phase 6)

- **Filesystem**: `ProtectHome=yes`, `ProtectSystem=strict`, `PrivateTmp=yes`
- **Capabilities**: Minimal set (NET_ADMIN, SYS_ADMIN, DAC_OVERRIDE, KILL, CHOWN, FOWNER)
- **Miscellaneous**: `MemoryDenyWriteExecute=yes`, `RestrictRealtime=yes`, `LockPersonality=yes`
- **Resource limits**: NOFILE=65536, NPROC=4096

### Installer Verification (Phase 7)

- **SHA256 checksums**: Binary + `.sha256` file downloaded separately
- **Verification**: Checksum verified before installation; abort on mismatch
- **No curl|bash**: Production docs use download-then-verify flow

### Snapshot Hardening (Phase 8)

- **Permissions**: Snapshot directories set to 0700 (root-only)
- **Cross-tenant rejection**: Path canonicalization prevents `../` traversal to other tenant snapshots
- **Audit logging**: All snapshot create/restore/delete operations are logged
- **Secure GC**: Files zero-filled before unlink to prevent data recovery
- **Capability detection**: `snapshot_capabilities()` reports Firecracker version

### Network Verification (Phase 9)

- **TAP validation**: Verify TAP devices match tenant net_id prefix (`tn<net_id>`)
- **Cross-bridge checks**: Verify iptables DROP rules between different tenant bridges
- **Deep verify**: `mvm net verify --deep` provides detailed rule content for manual audit
- **Existing checks**: Bridge exists, UP state, gateway assigned, NAT/FORWARD rules present

## Environment Variables

| Variable | Values | Description |
|----------|--------|-------------|
| `MVM_PRODUCTION` | `1` or `true` | Enable production security mode |
| `MVM_TENANT_KEY_<ID>` | hex-encoded 32 bytes | Per-tenant LUKS encryption key (dev) |

## File Paths

| Path | Permissions | Description |
|------|------------|-------------|
| `/var/lib/mvm/keys/<tenant>.key` | 0600 | Per-tenant encryption key (production) |
| `/var/lib/mvm/tenants/<t>/pools/<p>/snapshots/` | 0700 | Snapshot directories |
| `/etc/mvm/certs/` | 0700 | TLS certificates |
| `/etc/mvm/desired.json` | 0600 | Desired state configuration |

## Explicitly Deferred

| Item | Reason | Target |
|------|--------|--------|
| Privilege separation (agentd/hostd split) | High complexity, requires separate binaries + Unix socket RPC | Sprint 5 |
| Snapshot encryption | Depends on key management maturity; hardened permissions + GC covers 80% | Sprint 5 |
| Signed desired-state updates (Ed25519) | Requires coordinator-side signing infrastructure | Sprint 5 |
| Shared-bridge segmentation with nftables tagging | Per-tenant bridges already provide isolation by construction | Not planned |
