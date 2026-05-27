# ADR-059 — Host services broker over vsock

- **Status:** Proposed
- **Date:** 2026-05-27
- **Owner:** MVM Project
- **Related:** [ADR-002 microvm security posture](002-microvm-security-posture.md), [ADR-041 signed audited execution plans](041-signed-audited-execution-plans.md), [ADR-047 app deps audit pipeline](047-app-deps-audit-pipeline.md), [ADR-048 claim-safe sandbox parity](048-claim-safe-sandbox-parity.md), [ADR-049 secret substitution mechanism](049-secret-substitution-mechanism.md), [ADR-053 guest protocol versioning and readiness](053-guest-protocol-versioning-and-readiness.md), [ADR-055 passt virtio-net](055-passt-virtio-net.md), [ADR-058 claim-10 bytes leaving trust boundary](058-claim-10-bytes-leaving-trust-boundary.md), mvmd [ADR-0023 mvmd host services delegation](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md), [Plan 104 host services broker](../plans/104-host-services-broker.md), [threat model 02 host services broker](../threat-models/02-host-services-broker.md)

## Context

Today, anything a microVM needs from the host arrives one of two ways:

1. **Boot-time only** — read-only ext4 drive mounted at `/mnt/secrets` (`mvmctl up --volume host_dir:/mnt/secrets`). ADR-048 tags this `unsafe_guest_secret_materialization` and declines a non-leakage claim.
2. **A small fixed-verb reverse channel** — `HostBoundRequest` on vsock port 53 carries `WakeInstance`, `QueryInstanceStatus`, `QueryHostTime`. Each new verb is a code change to an enum.

ADR-049 committed to a vsock side-channel for secret substitution as v1's secrets mechanism, but stubbed the implementation. What's needed is broader than secrets: a **host-side services layer** microVMs call at runtime — secrets, time, cost, and (later) logging / audit / monitoring — with one auth model, one capability model, one audit chain, one extension point that supports built-in and addon-provided services without protocol churn.

This ADR records the architectural decisions for that broker. Plan 104 carries the implementation specifics.

## Decision

Ship a **host services broker** with the following committed shape:

### Architecture: four-subprocess design

The supervisor is a pure launcher + admission controller + IPC router. Four per-VM subprocesses share zero address space with the supervisor and zero address space with one another:

| Subprocess | UID | Role | Listens on |
| --- | --- | --- | --- |
| `mvm-broker` | 903 | General handlers: `host.time.v1`, `host.cost.v1`, `broker.v1` | vsock 5300 + per-VM UDS |
| `mvm-secrets-dispatcher` | 902 | `host.secrets.v1` only | vsock 5301 + per-VM UDS |
| `mvm-host-signer` | 904 | Sole holder of the host signer key; signs plans + signed credentials via UDS RPC | per-VM UDS only |
| `mvm-audit-signer` | 905 | Sole writer to `~/.mvm/audit/<tenant>.jsonl`; sole holder of the audit chain-signing key | per-VM UDS only |

Each subprocess runs under seccomp + setpriv + per-workload cgroup v2 + PID/mount namespaces. Each binary is cosign-verified at spawn with TOCTOU-resistant mmap-then-`fexecve`. Each subprocess's startup config is a JSON envelope signed under the same release-time key as the binary; subprocess refuses to start unless config signature verifies. Each subprocess has its own per-spawn ephemeral keypair and signs every response it produces; the supervisor verifies the signature before relaying. See [Plan 104 §Hardening posture](../plans/104-host-services-broker.md#hardening-posture-layers-111) for the per-subprocess hardening matrix.

### Protocol

- **Two vsock ports per VM:** 5300 (general broker) + 5301 (secrets).
- **Wire format:** 4-byte big-endian length-prefixed JSON; payload is `serde_json::Value` wrapped in `ServiceCall` / `ServiceResponse` envelopes with `#[serde(deny_unknown_fields)]`.
- **Authentication:** `AuthenticatedFrame` (Ed25519 + session id + sequence) on both ports from day one, with a 1-byte **algorithm-identifier** in the frame header (`0x01=Ed25519`, `0x02=ECDSA-P256` for the macOS SE host-signer path; reserved for future PQC).
- **Signed payloads:** JCS-canonicalized per RFC 8785 (`serde_jcs`).
- **No CBOR.** Decided 2026-05-26 (Plan 104 §T3) in favor of JSON for SDK-matrix friction and `jq` debuggability.

### Five-rule capability gating

Every call traverses, in order: (1) schema gate (size, depth, parse timeout), (2) authentication gate (`AuthenticatedFrame` verify, replay rejection), (3) binding gate (`ExecutionPlan.services` lookup), (4) profile + rate-limit + quota gate, (5) handler-specific policy (typed `parse_payload` with `deny_unknown_fields`; destination-URL match for `host.secrets.v1`). Gates 1–4 run in the supervisor; gate 5 runs in the subprocess that owns the service. See Plan 104 §"Capability gating" for the full failure-mode walk.

### Audit chain

`EventCategory::ServiceCall` extends `mvm-audit-signer`'s entry set. Every dispatch — allowed or denied — emits one entry: `(service, verb, outcome, correlation_id)`. Payload content is never logged (ADR-053 §4 redaction invariant). Operator actions (`mvmctl services revoke`, `mvmctl host-key rotate`, `mvmctl audit verify`, `mvmctl up --insecure-host`) emit chain-signed entries via `mvm-audit-signer`. Chain entries are JCS-canonical, self-contained, `O_APPEND`-only on a dir-immutable FS, with `chain_head` persisted to a second location on every append. Per-tenant ChaCha20-Poly1305 encryption at rest, key derived from a TPM/SE-bound master (Layer 5).

### Cross-VM via mvmd

Cross-VM data goes through mvmd-agent's existing iroh ALPN transport (`crates/mvmd-agent/src/transport.rs`) with new typed `AgentRequest` variants — NOT a new HTTP route, NOT raw QUIC+mTLS. Cross-tenant authorization happens mvmd-side via tenant-scoped-authz (ADR-0008). The mvmd-side work lives in [mvmd Plan 52 + ADR-0023](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md). The supervisor pins mvmd's public key at `~/.mvm/keys/mvmd-pubkey`; admission refuses without a pin.

### `ExecutionPlan` schema bump 4 → 5

`ExecutionPlan` gains a `services: Vec<ServiceBinding>` field. Old (v4) plans hard-fail at verification per the project's no-backcompat rule; no shim, no silent default. `crates/mvm-plan/src/plan.rs:45` `SCHEMA_VERSION: u32 = 4` becomes `5`. Migration: any in-flight v4 plans must be re-synthesized + re-signed under v5 to keep running.

## Implementation choices

These are pinned-now-so-they-don't-drift:

| Concern | Crate / mechanism | Why |
| --- | --- | --- |
| Signing | `ed25519-dalek` v2.x | RustCrypto, constant-time verified |
| Canonical JSON | `serde_jcs` (pin exact version; CI runs RFC 8785 conformance corpus on every PR) | Required for cross-implementation signature verification |
| AEAD for audit-at-rest | `chacha20poly1305` (RustCrypto, audited) | Audit logs at rest under H-L5.4 |
| TPM (Linux) | `tpm2-tss` (Intel) | Linux host-signer key isolation under H-L2.1 |
| Secure Enclave (macOS) | Swift bridge to `SecKeyCreateRandomKey` with `kSecAttrTokenIDSecureEnclave` (P-256) | macOS host-signer key isolation under H-L2.1 |
| Constant-time comparisons | `subtle::ConstantTimeEq` | H-L4.5; CI grep lint enforces |
| Seccomp filters | `seccompiler` | Per-arch (x86_64 + aarch64) deny-lists under H-L3.3 |
| FIDO (W11) | `webauthn-authenticator-rs` | Operator FIDO ceremony under G2 / H-L11.6 |

All crates are present in `deny.toml` with advisory + license enforcement.

## Deployment modes

The broker is invoked in three deployment modes. Threats and mitigations apply differently:

| Mode | Description | Threats in scope | Notes |
| --- | --- | --- | --- |
| **single-dev** | A developer running `mvmctl` on their laptop | Hostile guest workload; hostile network for mvmd path | Insider threat NOT in scope (developer is the operator) |
| **CI** | A CI runner executing `mvmctl up` from a PR or branch | Above + hostile-PR insider (PR author cannot be trusted with prod credentials) | `mvmctl up --prod` should be gated by the FIDO ceremony (W11); CI auto-runs are `--no-prod` |
| **fleet (multi-tenant via mvmd)** | mvmd-orchestrated workloads across many hosts | Above + hostile mvmd-agent (S15), hostile multi-tenant (S4), hostile-network for mvmd transport, hostile insider with host shell (newly in-scope per §Threat model below) | Full hardening stack including W8 hardware enclave |

`mvmctl doctor` surfaces which mode the current host is operating in.

## Dependency CVE surface

The broker's isolation rests on vsock + the underlying VMM + kernel paths. Each is a CVE surface we must respond to. The doctor command refuses admission on known-affected versions; the affected-version list ships in `mvmctl` and is refreshed per release.

| Dependency | Surface | Doctor check |
| --- | --- | --- |
| Linux kernel `vhost-vsock` | Guest-to-host channel | Refuse admission on known-vulnerable kernel versions |
| Firecracker `virtio-vsock` | Linux runtime path | Refuse admission on known-vulnerable Firecracker versions |
| libkrun `virtio-vsock` | macOS-as-host runtime path | Refuse admission on known-vulnerable libkrun versions |
| cloud-hypervisor `virtio-vsock` | Builder VM path on macOS | Refuse admission on known-vulnerable cloud-hypervisor versions |
| Apple `vz` virtio sockets | macOS 26+ Apple Silicon runtime + builder | Refuse admission on macOS versions with known Apple-vz CVEs |
| gvproxy / passt | Userspace virtio-net gateway (egress secret backstop in S25) | Refuse admission on known-affected gateway versions |
| `ed25519-dalek`, `serde_json`, `serde_jcs`, `chacha20poly1305`, `tpm2-tss` | Crypto + parsing | `cargo deny` advisory + license gate on every PR |

A vsock CVE = emergency host upgrade required. The response posture is documented in `SECURITY.md`'s CVE response runbook (W10).

## Considered and rejected threats

Named here so future readers don't re-litigate:

- **Subprocess-restart accumulation attack.** Concern: attacker induces 3 restarts to harvest ephemeral subprocess response keys. **Dismissed:** no traffic encryption to decrypt; per-spawn keys give the attacker no cryptographic leverage; 3 keys per workload is far below cryptanalytic accumulation threshold.
- **Workload-set correlation IDs as a forensic-trail attack.** Concern: workload sets a `correlation_id` matching another workload's known sequence to confuse the audit chain. **Mitigated** (H-L4.6 / G4) by having the supervisor assign correlation IDs at frame ingress; workload-supplied IDs are rewritten or rejected.
- **PFS-via-broker-encryption.** Considered adding AEAD on the broker channel for forward secrecy. **Dismissed:** vsock is a host-local process-to-process channel; there is no network attacker against whom PFS would help. Adding AEAD here is security theater against the actual threat (memory-resident-key extraction).

## Threat model

ADR-002's "malicious host" out-of-scope clause is **narrowed**, not removed.

**Physical attacks remain out of scope:** cold-boot DRAM extraction, DMA via Thunderbolt/PCIe, hardware tampering (chip-off, side-channel power analysis), unauthorized firmware flashing. ADR-002 still applies to these.

**Software insider attacks are newly in scope** thanks to the L1+L2+L5 hardening:

- **Host signer key extraction by shell-on-host attacker** — defeated by H-L1.1 (key never loaded into the supervisor) + H-L2.1 (key never extractable from HW enclave on enclave-equipped hosts).
- **Audit chain forgery by shell-on-host attacker** — defeated by H-L1.2 (chain-signing key isolated to `mvm-audit-signer`) + H-L5.1 (O_APPEND-only FD) + H-L5.2 (anti-rollback chain-head persistence).
- **Audit log content extraction by shell-on-host attacker** — defeated by H-L5.4 (per-tenant ChaCha20-Poly1305 at rest, key derived from TPM/SE master).
- **Secret extraction from process memory by shell-on-host attacker** — defeated by H-L1.4 (per-workload cgroup + PID/mount namespace) + H-L3.9 (`PR_SET_DUMPABLE=0` / `PT_DENY_ATTACH` + mlock) + H-L3.11 (anti-debug startup check refuses to run under ptrace).

On **non-enclave hosts** (no Apple SE, no Linux TPM), the host signer is **trust-on-first-use (TOFU)**: whatever's on disk after first run is "the" key, and an attacker who wipes the host before first run can mint their own. `mvmctl doctor` surfaces this as a security-claim downgrade. Honest naming, not a hidden flaw.

## Security claims (proposed numbers — assigned at write-time against ADR-002's live list)

Sprint 56 holds claim 10 (bytes leaving trust boundary). This ADR proposes the next two claims; final numbers are assigned in the same PR as ADR-002 is updated.

> **Claim N (proposed).** Every host-side service the broker exposes is bound to a signed `ExecutionPlan.services` binding, enforced before handler dispatch, and audited via the chain-signed log. A tampered binding fails plan verification; an unbound call is refused with an audited deny. Tested by the W2 security regression set (`service_call_denied_when_unbound`, `service_call_denied_outside_profile`).

> **Claim N+1 (proposed).** No raw secret value crosses the broker channel. `host.secrets.v1` returns destination-bound, time-bound signed credentials only. Raw secret bytes never leave `mvm-host-signer`'s process boundary (under H-L2.1 / Layer 2 hardware enclave, never the host's address space at all). Tested by the W5 security regression set + ADR-049 hostile-guest matrix.

## Consequences

**Positive:**

- TCB minimization: a supervisor UAF no longer extracts host signer keys, audit signing keys, or in-flight credentials.
- Threat-model expansion: software insider attacks newly in scope.
- Extensibility: built-in handlers and v2 addon handlers share one substrate; new services land without protocol churn.
- Observability: every call audited; operator actions audited; `broker.v1/list_services` exposes the runtime catalog.
- Falsifiability: a fourth service `host.dev.echo.v1` can land in one handler file without touching envelope, registry, or auth — verified at W6.

**Negative:**

- Scope: roughly 3–4 sprints of work where the original draft was 1.
- Operational surface: four new subprocess binaries per VM; new doctor checks; new release-pipeline lanes (cosign, Sigstore, in-toto, reproducibility-per-binary).
- Single points of availability: `mvm-host-signer` and `mvm-audit-signer` are now load-bearing for admission and audit respectively; restart-with-backoff is the v1 mitigation, with m-of-n quorum deferred.
- Schema breakage: `ExecutionPlan` v4 plans hard-fail at v5; no shim. Operators must re-synthesize.
- Cross-backend complexity: the vz (Apple Silicon) backend needs a new `VZVirtioSocketListener` Swift class — substantial sub-task.
- Hardware-enclave dependency (W8): macOS SE + Linux TPM 2.0 integration is first-time work in `mvm`; software fallback retained but flagged as a downgrade.

## Non-goals

These are out of scope for v1 and named here so they don't quietly become assumed-covered. Future plans are listed where relevant.

- **Streaming responses.** Envelope is request/response only in v1. `host.monitoring.v1` deferred.
- **Addon-provided handlers shipping in v1.** v1 ships only the substrate; v2 ships actual addons.
- **`unsafe_guest_tls_inspection`** (the proxy-with-CA path from ADR-049). Ships separately.
- **Non-HTTP secret substitution.** ADR-049 already declares out of scope.
- **m-of-n quorum for host signer key rotation.** Operationally heavy. Future plan once W11 FIDO ceremony exists.
- **Hybrid Ed25519 + Dilithium signatures (PQC).** The algorithm-identifier byte (H-L4.1) is sufficient preparation; full hybrid signing waits until CRQC pressure is real.
- **Remote attestation of workload identity** (TPM PCR-bound workload signing). Research-grade; existing signed-`ExecutionPlan` + cosigned-image sufficient.
- **Full memory-snapshot encryption** for paused workloads. Realistic threat (disk-imaging the snapshot file) mitigated by host FDE (operator's responsibility).
- **Detection / alerting** (G10). Audit logs are forensics, not detection. `host.alert.v1` reserved as a future broker service in the host-logging follow-on plan.
- **Disaster recovery / key escrow** (G11). Lost host signer / TPM / audit-signer keys = workloads broken; no recovery in v1. Operator-held escrow signed under operator FIDO key is the right shape but a major operational lift. Future plan once W11 lands FIDO.
- **Supervisor split** (admission verifier + IPC router as separate processes). v1 supervisor remains the single launcher + IPC router + admission controller. Deferred to v2.
- **Cross-VM cost aggregation across tenants.** `host.cost.v1::tenant` is single-tenant.
- **Audit log rotation strategy concrete triggers** (size, age). Deferred to the host-logging follow-up plan / ADR-060; the continuity invariant (rotated-file last-entry hash = active-file first prev_hash) *is* in scope under H-L6.2.

## Reuse

- `AuthenticatedFrame` framing in `crates/mvm-guest/src/vsock.rs` — reused with an added algorithm-identifier byte (H-L4.1).
- `EventCategory` enum + chain-signed JSONL audit — reused; one new variant `ServiceCall`; chain-signing moves into `mvm-audit-signer`.
- `AgentProfile` enum — reused as the first capability gate.
- `ProtocolHello` capability negotiation (ADR-053) — reused; `GuestCapability::ServicesBroker` added.
- `ExecutionPlan.secrets: Vec<SecretBinding>` and `SecretReleasePolicy` — reused as policy blob for `host.secrets.v1`.
- Supervisor's per-workload cost accumulators — reused by `host.cost.v1` (W4a builds them; they don't exist today).

## See also

- [Plan 104 — host services broker](../plans/104-host-services-broker.md) for the implementation specifics (build sequence, critical files, verification set, hardening posture L1–L11).
- [threat model 02 — host services broker](../threat-models/02-host-services-broker.md) for the STRIDE walk per service.
- [ADR-049 — secret substitution mechanism](049-secret-substitution-mechanism.md) for the secrets-specific design that lands as `host.secrets.v1`.
- [mvmd ADR-0023 — mvmd host services delegation](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md) for the cross-VM trust model.
- [ADR-002 §Out of scope](002-microvm-security-posture.md#out-of-scope) — the "malicious host" clause this ADR narrows.
