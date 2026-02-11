# Security Architecture

mvm implements a fully hardened production security model. This document covers the threat model, hardening measures, privilege separation, and cryptographic protections.

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

## Privilege Separation (Hostd/Agentd Split)

Production deployments split the agent into two processes:

- **mvm-hostd** — Privileged executor daemon. Runs as root with a minimal capability bounding set. Listens on `/run/mvm/hostd.sock` (Unix domain socket, 0660, group `mvm`). Executes only pre-defined operations: start/stop/sleep/wake/destroy instances, setup/teardown network bridges.
- **mvm-agentd** — Unprivileged reconciler. Runs as the `mvm` user with zero capabilities. Handles QUIC API, desired state validation, reconcile logic. Delegates all privileged operations to hostd via typed IPC.

**IPC Protocol**: Length-prefixed JSON over Unix domain socket (4-byte BE length + JSON body). Same frame protocol as the QUIC API. Request/response types are fully typed enums (`HostdRequest`/`HostdResponse`).

**Systemd Units**:
- `deploy/systemd/mvm-hostd.service` — Root, `CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_KILL CAP_CHOWN CAP_FOWNER CAP_SYS_CHROOT CAP_SETUID CAP_SETGID CAP_MKNOD CAP_DAC_OVERRIDE`
- `deploy/systemd/mvm-agentd.service` — User `mvm`, `CapabilityBoundingSet=` (empty)

**Backwards Compatibility**: The `--hostd-socket` flag is opt-in. Without it, the agent runs as a single process (current dev-mode behavior).

## Snapshot Encryption (AES-256-GCM)

Snapshot memory files are encrypted per-tenant using AES-256-GCM:

- **Algorithm**: AES-256-GCM with random 12-byte nonce per file
- **Format**: `[12-byte nonce][ciphertext][16-byte authentication tag]`
- **Key source**: Per-tenant key from `KeyProvider` (env var or file-based)
- **In-place encryption**: After snapshot capture, plaintext is encrypted to `.enc`, then the plaintext file is deleted
- **Authentication**: GCM tag provides tamper detection — decryption fails if ciphertext is modified

Module: `security/snapshot_crypto.rs`

## Signed Desired State (Ed25519)

Coordinator-pushed desired state can be cryptographically signed:

- **Algorithm**: Ed25519 (via `ed25519-dalek` crate)
- **Trusted keys**: Stored in `/etc/mvm/trusted_keys/*.pub` (base64-encoded 32-byte public keys)
- **Production enforcement**: When `MVM_PRODUCTION=1`, unsigned `Reconcile` requests are rejected with HTTP 403. Only `ReconcileSigned` requests are accepted.
- **Dev mode**: Both signed and unsigned requests are accepted (backwards compatible)
- **Signed data covers**: The entire `DesiredState` JSON (tenant IDs, pool IDs, resource limits, desired counts, subnet allocations)

Module: `security/signing.rs`

## Memory Hygiene

All cryptographic key material is wrapped in `Zeroizing<Vec<u8>>` (from the `zeroize` crate):

- `KeyProvider::get_data_key()` returns `Zeroizing<Vec<u8>>` — keys are zeroed on drop
- Hex-encoded key strings in `encryption.rs` are wrapped in `Zeroizing<String>`
- Ed25519 `SigningKey` from `ed25519-dalek` already implements `Zeroize`
- No key material appears in `tracing::info!` or `tracing::debug!` logs

## Node Attestation (Extension Point)

`security/attestation.rs` provides an `AttestationProvider` trait for future TPM2/SEV-SNP/TDX integration:

- `NoopAttestationProvider` — default, reports `provider: "none"`
- `NodeInfo.attestation_provider` field reports the active attestation mechanism
- When hardware attestation is available, implement the trait and register the provider

## Explicitly Deferred

| Item | Reason |
|------|--------|
| `mlock` for key memory pages | Platform-specific complexity, `Zeroizing` covers the common case |
| Full TPM2/SEV-SNP attestation implementation | Requires hardware; trait + extension point ready |
| Shared-bridge segmentation with nftables tagging | Per-tenant bridges already provide isolation by construction |
