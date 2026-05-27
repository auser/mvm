# ADR-061 — Host services broker — four-subprocess hardening

- **Status:** Proposed — supersedes [ADR-059](059-host-services-broker.md) §Architecture and §Security model
- **Date:** 2026-05-27
- **Owner:** MVM Project
- **Related:** [ADR-002 microvm security posture](002-microvm-security-posture.md), [ADR-041 signed audited execution plans](041-signed-audited-execution-plans.md), [ADR-047 app deps audit pipeline](047-app-deps-audit-pipeline.md), [ADR-048 claim-safe sandbox parity](048-claim-safe-sandbox-parity.md), [ADR-049 secret substitution mechanism](049-secret-substitution-mechanism.md), [ADR-053 guest protocol versioning and readiness](053-guest-protocol-versioning-and-readiness.md), [ADR-058 claim-10 bytes leaving trust boundary](058-claim-10-bytes-leaving-trust-boundary.md), [ADR-059 host services broker (original)](059-host-services-broker.md), [Plan 104 host services broker](../plans/104-host-services-broker.md), mvmd [ADR-0023 mvmd host services delegation](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md)

## Context

[ADR-059](059-host-services-broker.md) shipped a **two-process design** for the host services broker: the supervisor hosts the general broker in-process (`host.time.v1`, `host.cost.v1`, `broker.v1`), and `mvm-secrets-dispatcher` runs in a separate subprocess for `host.secrets.v1`. ADR-059 records that decision and the JSON wire format, JCS-canonical signing, capability-gating, and audit-chain shapes.

Subsequent threat-modeling under the directive to make this design "as tight as practical" identified four key isolation gaps the two-process design does not address:

1. **Host signer key extraction.** The supervisor reads `~/.mvm/keys/host-signer.ed25519` to sign `ExecutionPlan`s. A supervisor UAF therefore extracts the key, which compromises *all future* plans (claim 8) across the entire host until the key is rotated.
2. **Audit chain forgery.** The supervisor holds the audit chain-signing key and is the sole writer to `~/.mvm/audit/<tenant>.jsonl`. A supervisor compromise can forge entries arbitrarily, defeating claim 8's chain-signed audit invariant.
3. **General broker bug pivots into the supervisor TCB.** A use-after-free or integer overflow in the in-process broker's JSON parser, registry, or quota logic runs in the supervisor's address space — it can pivot into admission, plan signing, or audit signing code paths.
4. **Software insider attacks.** ADR-002's "malicious host" out-of-scope clause assumes the host operator is trusted. With shell access to the host on the two-process design, an insider can read all of the above: the host signer key, the audit chain key, the audit log plaintext, and the in-flight secrets in process memory.

This ADR records the decision to pivot to a **four-subprocess design** that addresses all four gaps and narrows ADR-002's "malicious host" clause to exclude software insiders. Plan 104's "Hardening posture (Layers 1–11)" section carries the implementation specifics.

## Decision

The broker architecture moves from two processes (supervisor + secrets dispatcher) to **four discrete subprocesses**, each in its own uid + seccomp + setpriv + per-workload cgroup + PID/mount namespace. The supervisor becomes a pure launcher + admission controller + IPC router.

| Subprocess | UID | Role | Listens on |
| --- | --- | --- | --- |
| `mvm-broker` | 903 | `host.time.v1`, `host.cost.v1`, `broker.v1` | vsock 5300 + per-VM UDS |
| `mvm-secrets-dispatcher` | 902 | `host.secrets.v1` only | vsock 5301 + per-VM UDS |
| `mvm-host-signer` | 904 | Sole holder of host signer key; signs plans + signed credentials via UDS RPC | per-VM UDS only |
| `mvm-audit-signer` | 905 | Sole writer to `~/.mvm/audit/<tenant>.jsonl`; sole holder of audit chain-signing key | per-VM UDS only |

Each subprocess is cosign-verified at spawn with TOCTOU-resistant mmap-then-`fexecve`. Each receives a release-key-signed JSON config envelope on stdin and refuses to start unless the signature verifies. Each has its own per-spawn ephemeral keypair and signs every response it produces; the supervisor verifies before relaying. The full hardening matrix is documented in [Plan 104 §Hardening posture (Layers 1–11)](../plans/104-host-services-broker.md#hardening-posture-layers-111).

Additional new decisions in this hardening:

- **Algorithm-identifier byte** in `AuthenticatedFrame` (`0x01=Ed25519`, `0x02=ECDSA-P256` reserved for the macOS SE host-signer path in Plan 104 W8). Lets us swap algorithm later without a hard fork.
- **Hardware-enclave host signer** in W8 (Apple Secure Enclave on macOS; TPM 2.0 on Linux via `tpm2-tss`). Software fallback retained with a loud `mvmctl doctor` downgrade row; TOFU honesty for non-enclave hosts.
- **TPM monotonic counter for rotation rollback resistance.** Each `mvmctl host-key rotate` increments the counter; the value embeds in admission audit entries.
- **Per-call ephemeral session-key rotation** (`BROKER_SESSION_REKEY_CALLS=1000`, `BROKER_SESSION_REKEY_MS=60000`).
- **Audit-log encryption at rest** — per-tenant ChaCha20-Poly1305 key derived from a TPM/SE-bound master.
- **Anti-rollback chain-head persistence** — `chain_head` written to a second location on every entry.
- **Supervisor-assigned correlation IDs** (rewrites or rejects workload-supplied IDs to prevent cross-workload forensic-trail confusion).
- **`O_APPEND`-only audit FD + dir-immutable** (`chattr +a` on Linux, `UF_APPEND` on macOS).
- **Tenant-level secret call quotas** (mvmd-enforced cap; in addition to per-workload quotas).
- **mvmd identity pinning** in `~/.mvm/keys/mvmd-pubkey`; admission refuses without a pin.
- **Composition width cap** (`BROKER_COMPOSITION_WIDTH=5`).
- **TLS-1.3-only + single suite** (ChaCha20-Poly1305-SHA256, X25519) to mvmd.
- **Operator FIDO touch on `mvmctl up --prod`** — stub in Plan 104 W1; full implementation in W11.
- **Sigstore/Rekor transparency log per subprocess release**; in-toto attestations alongside SLSA; per-binary reproducibility-double-build lane.
- **`cargo-mutants` mutation testing** lane targeting the four subprocess crates + supervisor services module.
- **`mvmctl doctor` refuses admission on weak hosts** (KASLR, KPTI, SMEP/SMAP, Spectre-v2, KSM, THP, LSM, kernel hardening sysctls; macOS SIP+AMFI+kext). `--insecure-host` audits + warns.

## What this supersedes from ADR-059

| ADR-059 section | Status under ADR-061 |
| --- | --- |
| §Architecture (two-process design) | **Superseded.** Replaced by the four-subprocess design above. The narrative in ADR-059 still applies as the *original* design; readers should treat ADR-061 as the current architectural source of truth. |
| §Security model | **Extended.** ADR-059's threat model assumed the supervisor was a single trust boundary; ADR-061 splits it into four subprocesses and adds software insider attacks to the in-scope set (see §Threat model below). |
| §Decision (high-level) | **Refined.** The high-level "we ship a broker" decision stands. The architectural specifics under it are superseded. |

## What remains from ADR-059 unchanged

| ADR-059 section | Status |
| --- | --- |
| §Wire format (JSON; `serde_json` envelopes; `deny_unknown_fields`) | Unchanged. |
| §JCS canonical signing (RFC 8785; `serde_jcs`) | Unchanged. |
| §Capability gating (five rules: schema, auth, binding, profile + quota, handler policy) | Unchanged in structure; gates 1–4 still run in the supervisor before forwarding to the appropriate subprocess. |
| §Audit chain (one new `EventCategory::ServiceCall`; chain-signed JSONL; payload bytes never logged) | Unchanged in shape; mechanism moves to `mvm-audit-signer` per Layer 1. |
| §Cross-VM via mvmd (iroh ALPN; new `AgentRequest` variants; mvmd-side Plan 52 + ADR-0023) | Unchanged. |
| §ExecutionPlan schema bump 4→5 (`services: Vec<ServiceBinding>`; no shim) | Unchanged. |
| §Comparison of SDK-hook vsock vs TLS-terminating proxy (ADR-049 alternatives) | Unchanged. |
| Claims 12 + 13 numbering | Unchanged. ADR-061 carries the implementation details under which Claim 13's "supervisor's address space" reads through as a strict tightening (raw secrets are now in `mvm-secrets-dispatcher`'s + `mvm-host-signer`'s subprocess address spaces — both subsets of the supervisor's previous responsibility). |

## Implementation choices

Pinned now so they don't drift:

| Concern | Crate / mechanism | Why |
| --- | --- | --- |
| Signing | `ed25519-dalek` v2.x | RustCrypto, constant-time verified |
| Canonical JSON | `serde_jcs` (pin exact version; CI runs RFC 8785 conformance corpus on every PR) | Required for cross-implementation signature verification |
| AEAD for audit-at-rest | `chacha20poly1305` (RustCrypto, audited) | Audit logs at rest under Plan 104 §H-L5.4 |
| TPM (Linux) | `tpm2-tss` (Intel) | Linux host-signer key isolation under H-L2.1 |
| Secure Enclave (macOS) | Swift bridge to `SecKeyCreateRandomKey` with `kSecAttrTokenIDSecureEnclave` (P-256) | macOS host-signer key isolation under H-L2.1 |
| Constant-time comparisons | `subtle::ConstantTimeEq` | H-L4.5; CI grep lint enforces |
| Seccomp filters | `seccompiler` | Per-arch (x86_64 + aarch64) deny-lists under H-L3.3 |
| FIDO (W11) | `webauthn-authenticator-rs` | Operator FIDO ceremony under Plan 104 §H-L11.6 |

All crates are present in `deny.toml` with advisory + license enforcement.

## Deployment modes

Threats and mitigations apply differently across deployment shapes; ADR-061 inherits ADR-059's framing and adds the insider-threat distinction:

| Mode | Description | Threats in scope | Notes |
| --- | --- | --- | --- |
| **single-dev** | A developer running `mvmctl` on their laptop | Hostile guest workload; hostile network for mvmd path | Insider threat NOT in scope (developer is the operator) |
| **CI** | A CI runner executing `mvmctl up` from a PR or branch | Above + hostile-PR insider (PR author cannot be trusted with prod credentials) | `mvmctl up --prod` gated by the FIDO ceremony (W11); CI auto-runs are `--no-prod` |
| **fleet (multi-tenant via mvmd)** | mvmd-orchestrated workloads across many hosts | Above + hostile mvmd-agent (Plan 104 §S15), hostile multi-tenant (§S4), hostile network for mvmd transport, **hostile insider with host shell** (newly in scope per §Threat model below) | Full hardening stack including W8 hardware enclave |

`mvmctl doctor` surfaces which mode the current host is operating in.

## Dependency CVE surface

The broker's isolation rests on vsock + the underlying VMM + kernel paths. Each is a CVE surface that requires a response. Doctor refuses admission on known-affected versions; the affected-version list ships in `mvmctl` and is refreshed per release.

| Dependency | Surface | Doctor check |
| --- | --- | --- |
| Linux kernel `vhost-vsock` | Guest-to-host channel | Refuse admission on known-vulnerable kernel versions |
| Firecracker `virtio-vsock` | Linux runtime path | Refuse admission on known-vulnerable Firecracker versions |
| libkrun `virtio-vsock` | macOS runtime path | Refuse admission on known-vulnerable libkrun versions |
| cloud-hypervisor `virtio-vsock` | Builder VM path on macOS | Refuse admission on known-vulnerable cloud-hypervisor versions |
| Apple `vz` virtio sockets | macOS 26+ Apple Silicon runtime + builder | Refuse admission on macOS versions with known Apple-vz CVEs |
| gvproxy / passt | Userspace virtio-net gateway (egress secret backstop in Plan 104 §S25) | Refuse admission on known-affected gateway versions |
| `ed25519-dalek`, `serde_json`, `serde_jcs`, `chacha20poly1305`, `tpm2-tss` | Crypto + parsing | `cargo deny` advisory + license gate on every PR |

A vsock CVE = emergency host upgrade required. Response posture documented in `SECURITY.md`'s CVE response runbook (PR-i).

## Considered and rejected threats

Named here so future readers don't re-litigate:

- **Subprocess-restart accumulation attack.** Concern: attacker induces 3 restarts to harvest ephemeral subprocess response keys. **Dismissed:** no traffic encryption to decrypt; per-spawn keys give the attacker no cryptographic leverage; 3 keys per workload is far below cryptanalytic accumulation threshold.
- **Workload-set correlation IDs as a forensic-trail attack.** **Mitigated** (Plan 104 §H-L4.6 / G4) by supervisor-assigned correlation IDs at frame ingress; workload-supplied IDs are rewritten or rejected.
- **PFS-via-broker-encryption.** Considered adding AEAD on the broker channel for forward secrecy. **Dismissed:** vsock is a host-local process-to-process channel; there is no network attacker against whom PFS would help. Adding AEAD here is security theater against the actual threat (memory-resident-key extraction).

## Threat model

ADR-002's "malicious host" out-of-scope clause is **narrowed**, not removed.

**Physical attacks remain out of scope:** cold-boot DRAM extraction, DMA via Thunderbolt/PCIe, hardware tampering (chip-off, side-channel power analysis), unauthorized firmware flashing.

**Software insider attacks are newly in scope** thanks to Layer 1 + 2 + 5 hardening:

- **Host signer key extraction by shell-on-host attacker** — defeated by H-L1.1 (key never loaded into the supervisor) + H-L2.1 (key never extractable from HW enclave on enclave-equipped hosts).
- **Audit chain forgery by shell-on-host attacker** — defeated by H-L1.2 (chain-signing key isolated to `mvm-audit-signer`) + H-L5.1 (`O_APPEND`-only FD) + H-L5.2 (anti-rollback chain-head persistence).
- **Audit log content extraction by shell-on-host attacker** — defeated by H-L5.4 (per-tenant ChaCha20-Poly1305 at rest, key derived from TPM/SE master).
- **Secret extraction from process memory by shell-on-host attacker** — defeated by H-L1.4 (per-workload cgroup + PID/mount namespace) + H-L3.9 (`PR_SET_DUMPABLE=0` / `PT_DENY_ATTACH` + mlock) + H-L3.11 (anti-debug startup check refuses to run under ptrace).

On **non-enclave hosts** (no Apple SE, no Linux TPM), the host signer is **trust-on-first-use (TOFU)**: whatever's on disk after first run is "the" key. `mvmctl doctor` surfaces this as a security-claim downgrade. Honest naming, not a hidden flaw.

## Consequences

**Positive:**

- TCB minimization: a supervisor UAF no longer extracts host signer keys, audit signing keys, or in-flight credentials.
- Threat-model expansion: software insider attacks newly in scope.
- Extensibility unchanged from ADR-059: built-in handlers and v2 addon handlers share one substrate.
- Observability unchanged: every call audited; operator actions audited; `broker.v1/list_services` exposes the runtime catalog.
- Falsifiability: a fourth service `host.dev.echo.v1` can land in one handler file in `mvm-broker` without touching envelope, registry, or auth — verified at Plan 104 W6.

**Negative:**

- Scope: roughly 3–4 sprints of work where the original ADR-059 / Plan 104 v1 was 1.
- Operational surface: four new subprocess binaries per VM (was 1 under ADR-059); new doctor checks; new release-pipeline lanes (cosign per binary, Sigstore, in-toto, reproducibility-per-binary).
- Single points of availability: `mvm-host-signer` and `mvm-audit-signer` are now load-bearing for admission and audit respectively; restart-with-backoff is the v1 mitigation, with m-of-n quorum deferred.
- Cross-backend complexity: the vz (Apple Silicon) backend needs a new `VZVirtioSocketListener` Swift class — substantial sub-task.
- Hardware-enclave dependency (W8): macOS SE + Linux TPM 2.0 integration is first-time work in `mvm`; software fallback retained but flagged as a downgrade.

## Non-goals

(Inherits ADR-059's non-goals; lists only the *additions* this hardening makes explicit so they don't quietly become assumed-covered.)

- **m-of-n quorum for host signer key rotation.** Operationally heavy. Future plan once W11 FIDO ceremony exists.
- **Hybrid Ed25519 + Dilithium signatures (PQC).** The algorithm-identifier byte (above) is sufficient preparation; full hybrid signing waits until CRQC pressure is real.
- **Remote attestation of workload identity** (TPM PCR-bound workload signing). Research-grade; existing signed-`ExecutionPlan` + cosigned-image sufficient.
- **Full memory-snapshot encryption** for paused workloads. Realistic threat (disk-imaging the snapshot file) mitigated by host FDE (operator's responsibility).
- **Detection / alerting** (Plan 104 §G10). Audit logs are forensics, not detection. `host.alert.v1` reserved as a future broker service in the host-logging follow-on plan.
- **Disaster recovery / key escrow** (Plan 104 §G11). Future plan once W11 lands FIDO.
- **Supervisor split** (admission verifier + IPC router as separate processes). v1 supervisor remains the single launcher + IPC router + admission controller. Deferred to v2.

## Migration from ADR-059's two-process design

Per the project's no-backcompat rule: there is no shim, no migration path, no transitional period. ADR-059's two-process design has not yet been implemented (Plan 104 v1 is the implementation plan; nothing was built yet). Implementation begins directly under ADR-061's four-subprocess design. Plan 104 W1 scaffolds all four subprocesses from day one.

## See also

- [Plan 104 — host services broker](../plans/104-host-services-broker.md) §Hardening posture (Layers 1–11) for the per-subprocess hardening matrix and the build sequence W1–W11.
- [ADR-059 — host services broker (original)](059-host-services-broker.md) for the JSON wire format, JCS signing, capability gating, audit chain, and cross-VM delegation decisions that ADR-061 inherits unchanged.
- [ADR-002 §"Security claims"](002-microvm-security-posture.md) for Claims 12 + 13 (pending merge from `worktree-adr-002-claims-12-13`).
- [ADR-049 — secret substitution mechanism](049-secret-substitution-mechanism.md) for the `host.secrets.v1` substitution flow that lands inside `mvm-secrets-dispatcher`.
- [mvmd ADR-0023 — mvmd host services delegation](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md) for the cross-VM trust model.
