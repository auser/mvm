# Threat model 02 — Host services broker over vsock

- **Status:** Proposed
- **Date:** 2026-05-27
- **Owner:** MVM Project
- **Related:** [ADR-002 microvm security posture](../adrs/002-microvm-security-posture.md), [ADR-059 host services broker (original two-process design)](../adrs/059-host-services-broker.md), [ADR-061 host services broker — four-subprocess hardening (supersession)](../adrs/061-host-services-broker-hardening.md), [Plan 104 host services broker](../plans/104-host-services-broker.md), [ADR-049 secret substitution mechanism](../adrs/049-secret-substitution-mechanism.md), [SECURITY.md (CVE response runbook)](../../SECURITY.md), [mvmd ADR-0023](../../../mvmd/specs/adrs/0023-mvmd-host-services-delegation.md), [threat model 01 — runtime baseline](01-runtime-baseline.md) (TBD)

This document is the STRIDE walk for the host services broker introduced by ADR-059 / Plan 104 and refined by [ADR-061's four-subprocess hardening](../adrs/061-host-services-broker-hardening.md). The ADRs are the decision records (architecture, choices); this document is the structured-threat enumeration with mitigation cross-references into Plan 104's §Hardening posture (Layers 1–11).

## Scope

**In scope:**

- The four broker subprocesses (`mvm-broker`, `mvm-secrets-dispatcher`, `mvm-host-signer`, `mvm-audit-signer`) and their per-VM lifecycle.
- The vsock channel (ports 5300 + 5301) between the guest microVM and the host subprocesses.
- The per-VM UDS channels between the supervisor and each subprocess.
- The cross-VM path from the supervisor to mvmd-agent over iroh ALPN, with respect to the four services Plan 104 ships.
- The `ExecutionPlan.services` admission ceremony and audit chain entries it generates.

**Out of scope** (per ADR-002, ADR-059, and ADR-061):

- Physical attacks on the host (cold-boot DRAM, DMA via Thunderbolt/PCIe, chip-off, side-channel power analysis, unauthorized firmware flashing).
- Multi-tenant guests (one guest = one workload).
- Hardware-backed key attestation by the workload itself.
- Vulnerabilities in the hypervisor's vsock implementation (KVM `vhost-vsock`, Firecracker, libkrun, cloud-hypervisor, Apple `vz`) — these are dependency-CVE-managed per [ADR-061 §"Dependency CVE surface"](../adrs/061-host-services-broker-hardening.md#dependency-cve-surface).

## Adversary model

Three adversary classes, in order of decreasing access:

| Class | Description | Capabilities |
| --- | --- | --- |
| **G — Hostile guest** | A workload running inside a microVM (the primary adversary). Has full control over guest userspace; cannot break out of the VM. | Sends arbitrary bytes to vsock 5300 + 5301; receives responses; observes timing |
| **N — Hostile network peer** | A network attacker on the path between the supervisor and mvmd-agent. | Observes + tampers with iroh ALPN traffic (mitigated by mvmd identity pinning + TLS 1.3) |
| **I — Software insider** | An unauthorized human with shell access to the host as some Unix user. **Newly in scope** per [ADR-061's §Threat model](../adrs/061-host-services-broker-hardening.md#threat-model) narrowing of ADR-002's "malicious host" clause (which remains true for *physical* attacks). | Executes arbitrary code on the host; cannot escalate to root if not already root; cannot perform physical attacks |

For each service below, the STRIDE table notes which adversary class the threat applies to in the **Adv.** column.

## Cross-cutting threats (apply to all services)

| ID | STRIDE | Adv. | Threat | Mitigation |
| --- | --- | --- | --- | --- |
| X-S1 | S | G | Guest spoofs another workload's session by forging session id | `AuthenticatedFrame` Ed25519/P-256 verify under per-workload session key (minted at admission, discarded at workload stop); session id rotated per H-L4.3 |
| X-S2 | S | I | Insider runs a fake `mvm-secrets-dispatcher` binary | Cosign-verify at spawn (H-L3.1); TOCTOU-resistant verify-then-`fexecve` (H-L3.2); subprocess config signed under the same release key (H-L3.6) |
| X-S3 | S | N | mvmd-agent identity spoofed during initial bootstrap | mvmd public key pinned in `~/.mvm/keys/mvmd-pubkey`; admission refuses without pin (H-L6.4) |
| X-T1 | T | G | Guest tampers with response bytes before guest userspace consumes them | Out of scope at the broker boundary — guest controls its own userspace |
| X-T2 | T | I | Insider tampers with the audit chain JSONL on disk | `O_APPEND`-only FD held by `mvm-audit-signer` (H-L5.1); dir-immutable (`chattr +a` / `UF_APPEND`); `chain_head` persisted to a second location and verified by `mvmctl audit verify` (H-L5.2); per-tenant AEAD encryption at rest (H-L5.4) means insider sees only ciphertext |
| X-T3 | T | I | Insider tampers with the host signer key on disk | On enclave-equipped hosts (H-L2.1) the key never leaves the enclave; on non-enclave hosts (TOFU) the key file is mode 0600 + `chattr +i` once written + monotonic-counter (H-L2.2) detects rollback |
| X-T4 | T | I | Insider modifies a subprocess binary between cosign-verify and exec | TOCTOU-resistant mmap-then-`fexecve` (H-L3.2) narrows the window to a kernel syscall; subprocess refuses to start if its config signature doesn't verify (H-L3.6) |
| X-R1 | R | G | Guest denies having made a call later | Every dispatch — allowed or denied — emits a chain-signed audit entry with `(service, verb, outcome, correlation_id)` (Plan 104 §Audit chain); audit chain is JCS-canonical and chain-signed (H-L5.1+H-L5.2) |
| X-R2 | R | I | Insider operator denies having taken a privileged action | Operator actions (`mvmctl services revoke`, `mvmctl host-key rotate`, `mvmctl up --insecure-host`) emit chain-signed entries via `mvm-audit-signer` (H-L6.1) |
| X-I1 | I | G | Guest reads bytes from another workload's UDS path | Per-VM UDS paths under `~/.mvm/vms/<vm>/services/` with mode 0600; supervisor-owned (uid 0) — guest in the microVM never has host-side FS access regardless |
| X-I2 | I | G | Guest infers state from response timing | Rate limit applies to read-only services; `host.secrets.v1` pads to latency floor `BROKER_SECRETS_LATENCY_FLOOR_MS=5` (S26); per-workload total-call/minute budget escalates to `ServiceCallAbuse` audit |
| X-I3 | I | I | Insider reads audit log contents | Per-tenant ChaCha20-Poly1305 at rest, key derived from TPM/SE-bound master (H-L5.4) |
| X-I4 | I | I | Insider reads in-memory secrets from a running subprocess | Per-workload cgroup + PID/mount namespace (H-L1.4); `mlock` on secret-bearing pages (H-L3.9); `PR_SET_DUMPABLE=0` / `PT_DENY_ATTACH` + anti-debug startup check (H-L3.9, H-L3.11); seccomp denies `process_vm_readv` (H-L3.3) |
| X-I5 | I | I | Insider extracts host signer key from process memory | On enclave-equipped hosts: key never in process memory (H-L2.1). On non-enclave hosts: key in `mvm-host-signer` process only (H-L1.1), confined by the cgroup + namespace + seccomp + mlock stack |
| X-D1 | D | G | Guest floods broker with calls to exhaust CPU/memory | Per-`(workload_id, service_id)` token-bucket; in-flight cap; lifetime quota (S12); per-workload broker-CPU budget (`BROKER_CPU_BUDGET_MS_PER_MIN=50`); memory cap (`BROKER_INFLIGHT_MEM_CAP_BYTES=1048576`); bounded vsock receive queue (`BROKER_QUEUE_DEPTH=16`) (S6, S21) |
| X-D2 | D | G | Guest forces subprocess restart loop | 3-restart cap per workload lifetime; beyond → audit `<subprocess>.crashed_repeatedly` and workload pause (Plan 82 harness) |
| X-D3 | D | N | mvmd unavailable blocks cross-tenant cost queries | Circuit breaker per handler (S13); `host.cost.v1::tenant` returns `Err(Unavailable)` rather than stale data (R2 in mvmd Plan 52) |
| X-D4 | D | G | Guest exploits amplification attack (small request → large response) | Per-handler `response_size_cap()` default 64 KiB; `Err(ResponseTooLarge)` + audited (S11) |
| X-E1 | E | G | Guest exploits a parser bug in the schema gate to elevate within the subprocess | Frame size cap (64 KiB) enforced before parse; recursion cap 8; 50ms parse timeout; `serde_json` is the fuzzed parser (W6 `fuzz_service_call.rs`); subprocess address space is fully isolated from the supervisor's |
| X-E2 | E | G | Guest exploits a logic bug in the binding-gate to call an unbound service | Binding-gate refuses; `service_call_denied_when_unbound` regression test in W2 |
| X-E3 | E | I | Insider replaces a subprocess binary and waits for the next workload | Cosign-verify at spawn (H-L3.1) refuses tampered binary; Sigstore/Rekor transparency log (H-L8.1) exposes any secretly-signed builds |
| X-E4 | E | G | Guest triggers a use-after-free in the general broker that pivots into the secrets dispatcher | Out of scope of the pivot — the four subprocesses share zero address space (Layer 1). A UAF in `mvm-broker`'s parser cannot reach `mvm-secrets-dispatcher`'s grant table |

## Per-service threat walk

### `host.time.v1` (returns wall + monotonic time)

| ID | STRIDE | Adv. | Threat | Mitigation |
| --- | --- | --- | --- | --- |
| TIME-I1 | I | G | Wall clock leaks host's NTP-synced time, useful for cross-workload correlation | Considered low-impact; tenant-private fleets already correlate via mvmd. `host.time.v1` returns canonical UTC. |
| TIME-T1 | T | I | Insider moves host clock backward, making `mvm-audit-signer` log backdated entries | `audit.clock.jump_detected` audit emitted on negative jump (H-L5.5); audit timestamps anchored to TPM monotonic counter or kernel boottime |
| TIME-D1 | D | G | Guest spams `time/now` calls to consume broker CPU | Token-bucket per workload (X-D1) |

### `host.cost.v1` (workload + tenant verbs)

| ID | STRIDE | Adv. | Threat | Mitigation |
| --- | --- | --- | --- | --- |
| COST-S1 | S | G | Workload spoofs workload-id to read another workload's cost | `correlation_id` is supervisor-assigned (H-L4.6); supervisor passes workload-id from its own state, not from workload-supplied data |
| COST-S2 | S | N | mvmd response forged by network attacker | mvmd identity pinned (H-L6.4); TLS 1.3 + ChaCha20-Poly1305 + X25519 (H-L4.4); mvmd responses parsed with `deny_unknown_fields`; mvmd-signed catalog response (S23) |
| COST-I1 | I | G | `tenant` verb leaks cross-tenant data | mvmd-side tenant-scoped-authz (ADR-0008); supervisor refuses mvmd response if tenant_id ≠ workload.tenant_id |
| COST-I2 | I | G | Cost numeric values quantize-leak workload behavior to a multi-step attacker | Considered low-impact for v1; future plan may quantize values to coarse units |
| COST-D1 | D | N | mvmd slow → blocks broker thread | Per-handler call timeout (`host.cost.v1::tenant=150ms`); circuit breaker after 5 failures (S13) |

### `host.audit.v1` (workload-emitted audit entries — new in ADR-062)

> **Note.** This section replaces the previous `host.secrets.v1` table. `host.secrets.v1` and the entire `SECRET-*` threat set are dropped by [ADR-062](../adrs/062-host-services-broker-rescope-drop-secrets.md). `host.audit.v1` becomes the load-bearing workload-callable service in its place.

| ID | STRIDE | Adv. | Threat | Mitigation |
| --- | --- | --- | --- | --- |
| AUDIT-S1 | S | G | Guest emits an entry claiming a workload id it doesn't own (impersonation) | Handler refuses with `ServiceErrorCode::BadRequest` when entry's `workload_id` ≠ `ctx.workload_id`; supervisor-assigned `workload_id` (H-L4.6) is the authoritative source |
| AUDIT-S2 | S | G | Guest forges a `workload_audit` entry that looks like a `Admission` (system-asserted) entry | New `EventCategory::WorkloadAudit` variant is *distinct* from `Admission` and `ServiceCall`; `mvm-audit-signer`'s category allow-list pins the variant to the handler that produced it; `mvmctl audit verify` displays category alongside entry |
| AUDIT-T1 | T | G | Guest tampers with an emitted entry after signing | **Architectural impossibility:** chain entries are signed by `mvm-audit-signer` before append; tamper fails `mvmctl audit verify` per chain integrity (X-T2) |
| AUDIT-R1 | R | G | Guest denies having emitted a particular entry | Every `host.audit.v1` call emits a chain-signed entry with `(workload_id, correlation_id, ts, fields)`; the chain ties the workload id to the entry cryptographically |
| AUDIT-I1 | I | G | Entry leaks sensitive workload-internal state through `fields` | The chain entry's `fields` are workload-controlled — they can include whatever the workload chooses. Plan 104 §H-L5.6 (PII invariant) applies to *system-asserted* `correlation_id`s; workload-emitted `fields` are workload's responsibility. **Operator-facing tooling should display a privacy warning** when filtering by `WorkloadAudit` category. |
| AUDIT-D1 | D | G | Guest fills the chain with garbage entries to obscure forensics or exhaust audit storage | Per-handler rate limit (`BROKER_AUDIT_TOKENS_PER_SEC=20`); per-record cap (4 KiB); per-batch cap (100 entries / 256 KiB); per-workload lifetime quota (§S12); tenant-level cap (§H-L6.3); audit-log rotation policy (§H-L6.2) keeps chain bounded |
| AUDIT-D2 | D | G | Guest's `emit_batch` triggers `mvm-audit-signer` crash via malformed bytes | Per-record JSON schema validation in handler before batch forward; batch is rejected with `ServiceErrorCode::BadRequest` if any record fails. Audit-signer's own envelope parse (X-E1 equivalent) is the second line of defense |
| AUDIT-E1 | E | G | Guest exploits handler parser bug to inject a non-`WorkloadAudit` category entry | Handler signature pins `category = WorkloadAudit` literal at call-time; the category field is *not* forwarded from workload input. CI lint asserts the handler doesn't accept a workload-supplied `category`. Tested by `host_audit_v1_workload_cannot_set_category` |

### `broker.v1` (introspection: `list_services`)

| ID | STRIDE | Adv. | Threat | Mitigation |
| --- | --- | --- | --- | --- |
| BROKER-I1 | I | G | Guest enumerates services to discover unbound services and probe for them | Bindings are workload-supplied at admission; `list_services` returns only the workload's bound set. An unbound service is invisible. |
| BROKER-D1 | D | G | Guest spams `list_services` for amplification | Response size cap 64 KiB (S11); per-handler call timeout |

## Threats by adversary class — summary

**Hostile guest (G):** Capped by the five-rule capability gating (Plan 104 §"Capability gating") + per-handler `parse_payload` + token-bucket quotas + circuit breaker + latency floor + audit chain. The most credible escape path is a parser bug in `serde_json` exploited via the schema gate; W6 fuzz (`fuzz_service_call.rs`) closes this. A pivot from `mvm-broker` to `mvm-secrets-dispatcher` is architecturally impossible (Layer 1).

**Hostile network peer (N):** Limited to the mvmd path. Mitigated by TLS 1.3 + ChaCha20-Poly1305 + X25519 + mvmd identity pinning + signed catalog responses. The supervisor-to-subprocess UDS paths are not network-reachable.

**Software insider (I):** Newly in scope per ADR-061 (supersedes ADR-059's two-process design's threat-model boundary). The L1+L2+L5 hardening (key isolation + HW enclave + at-rest encryption + cgroup/namespace) means shell access yields neither the host signer key, nor the audit chain-signing key, nor the audit log plaintext, nor in-flight secrets. The remaining insider capability is "modify a subprocess binary on disk and wait for the next spawn," which is defeated by cosign-verify + Sigstore/Rekor transparency.

## Open issues / explicitly accepted residual risk

- **Non-enclave hosts retain TOFU posture for the host signer.** Plan 104 §H-L11.5 and ADR-059 §"Threat model" both acknowledge this. `mvmctl doctor` surfaces it as a downgrade row. Mitigation deferred to W8 hardware-enclave integration; software fallback retained for hosts without TPM/SE.
- **Single-tenant `mvm-audit-signer` per host.** All workloads on a host share the audit-signer subprocess (per-VM still, but one subprocess per VM). A `mvm-audit-signer` UAF affects all entries for that workload — mitigated by the audit-signer subprocess being minimal-code and security-reviewed.
- **`mvm-host-signer` is a single point of admission availability.** If down, no plans can be signed and no workloads can admit. Restart-with-backoff is the v1 mitigation; m-of-n quorum deferred. Documented operational behavior.
- **No alerting in v1 (G10).** Audit logs are forensics. Detection-time discovery of a compromise depends on downstream log-shipping which is out of scope.
- **No disaster recovery for lost keys (G11).** Lost host signer key = broken workloads with no recovery path. Future plan once W11 FIDO ceremony exists.

## See also

- ADR-059 (decision record) for architecture + claims.
- Plan 104 (implementation specifics) for build sequence + verification.
- ADR-002 (microvm security posture) for the broader trust model this narrows.
- ADR-049 (secret substitution mechanism) for the `host.secrets.v1` design.
