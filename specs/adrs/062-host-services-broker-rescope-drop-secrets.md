# ADR-062 — Host services broker — drop `host.secrets.v1`, add `host.audit.v1`

- **Status:** Proposed — supersedes [ADR-049](049-secret-substitution-mechanism.md) in full, supersedes parts of [ADR-059 §"Architecture"](059-host-services-broker.md) and [ADR-061 §"Decision"](061-host-services-broker-hardening.md) (subprocess count + secrets-specific reasoning)
- **Date:** 2026-05-28
- **Owner:** MVM Project
- **Related:** [ADR-002 microvm security posture](002-microvm-security-posture.md), [ADR-049 secret substitution mechanism](049-secret-substitution-mechanism.md) (superseded), [ADR-059 host services broker](059-host-services-broker.md), [ADR-061 host services broker hardening](061-host-services-broker-hardening.md), [Plan 104 host services broker](../plans/104-host-services-broker.md), [threat model 02 host services broker](../threat-models/02-host-services-broker.md)

> **Consolidation (2026-05-31 — see [ADR-066](066-target-architecture.md) §"ADR consolidation"):** ADR-062 is the **canonical** host-services-broker ADR. It **consolidates** ADR-049 (secret substitution — dropped from v1), ADR-059 (broker architecture), and ADR-061 (broker hardening); those are **superseded** and physically archived to `archive/adrs/` in Stage E. Per ADR-066 §3 the broker / host-signer / audit-signer / supervisor remain four separate processes built from the one `mvm-hostd` crate.

## Context

[ADR-049](049-secret-substitution-mechanism.md) committed mvm to a vsock side-channel for runtime secret substitution. [ADR-059](059-host-services-broker.md) generalised that into the host services broker with `host.secrets.v1` as the forcing function. [ADR-061](061-host-services-broker-hardening.md) hardened the design with a four-subprocess architecture, where the dedicated `mvm-secrets-dispatcher` subprocess was the primary justification for the L1 TCB-minimization split.

Subsequent project-direction review (2026-05-28) decided to **drop runtime secret substitution as an mvm responsibility** in v1. Reasoning:

- The `host.secrets.v1` design pulls credential issuance into the host's trust boundary; the alternative ("workloads bring their own secret material") is materially simpler, and the security claims of mvm's broker are not load-bearing for whether *external* secret material is available to workloads.
- ADR-049's SDK-matrix cost (Python `requests`/`httpx`/`aiohttp`, TypeScript `fetch`/`axios`, Rust `reqwest`/`hyper`/`tonic` hook libraries) is substantial and the per-language hook surface is ongoing maintenance.
- The hostile-guest threat surface (raw socket bypass, library bypass, placeholder egress, S25 backstop) is large and growing.
- Workloads typically already have credential delivery mechanisms (env vars, file mounts, in-cloud IMDS, vault sidecars). Adding a fourth one in mvm's name is feature creep.

The hardening infrastructure that ADR-061 built around the secrets dispatcher (cosign-verified subprocesses, signed config envelopes, per-spawn ephemeral keys, isolated key holders) is still load-bearing for the *other* host-side responsibilities: signing `ExecutionPlan`s (Claim 8), writing chain-signed audit logs (Claim 8 audit chain), and the future host-services we *do* want (time, cost, audit-from-workload).

Separately, project-direction review wants **workloads to emit their own audit entries** as a first-class capability. Originally scoped to the host-logging follow-on plan; pulling into the main Plan 104 now keeps the audit infrastructure built in W1b useful from day one.

## Decision

**Drop `host.secrets.v1` and `mvm-secrets-dispatcher` from Plan 104 v1.** Delete the crate, delete the supervisor's `secrets_proxy.rs`, remove the secrets references from Plan 104 / ADR-049 / ADR-059 / ADR-061 / threat-model 02 / ADR-002 Claim 13.

**Add `host.audit.v1` as a workload-callable service in `mvm-broker`.** Verbs `emit` (one entry) + `emit_batch` (≤100 entries, ≤4 KiB each). Workload-emitted entries flow through `mvm-broker` → supervisor's `AuditSignerProxy` → `mvm-audit-signer`, chain-signed with a new `EventCategory::WorkloadAudit` variant so the chain verifier can distinguish workload-asserted from system-asserted entries.

**Keep all the subprocess hardening infrastructure** that ADR-061 built. The architecture becomes **3 subprocesses** (down from 4):

| Subprocess | UID | Role | Listens on |
| --- | --- | --- | --- |
| `mvm-broker` | 903 | `host.time.v1`, `host.cost.v1`, `host.audit.v1` (new), `broker.v1` | vsock 5300 + per-VM UDS |
| `mvm-host-signer` | 904 | Sole holder of host signer key; signs `ExecutionPlan`s via UDS RPC | per-VM UDS only |
| `mvm-audit-signer` | 905 | Sole writer to `~/.mvm/audit/<tenant>.jsonl`; sole holder of audit chain-signing key | per-VM UDS only |

The `mvm-secrets-dispatcher` subprocess (uid 902) is removed. The vsock listener on port 5301 (which was the secrets dispatcher's port) is removed too; only port 5300 is bound per VM.

## What this supersedes

| ADR / artifact | Status under ADR-062 |
| --- | --- |
| ADR-049 (entire) | **Superseded.** The vsock-substitution-vs-TLS-proxy comparison stays as historical context but the design itself is not being implemented. ADR-049's "Implementation: lands as `host.secrets.v1` in the host services broker" line is now false. |
| ADR-059 §"Architecture" (two-process design) | **Already superseded by ADR-061**; further narrowed to three subprocesses here. |
| ADR-061 §"Decision" (four-subprocess table) | **Superseded** by the three-subprocess table above. The reasoning for splitting `mvm-secrets-dispatcher` (credential-minting threat surface) is no longer applicable. The reasoning for the other three subprocesses (key isolation, audit isolation) **remains valid and is the basis for keeping them**. |
| ADR-061 §"Decision" — additional Layer-1 reasoning | **Preserved.** Host-signer isolation (H-L1.1) still load-bearing for Claim 8. Audit-signer isolation (H-L1.2) still load-bearing for chain integrity. General broker isolation (H-L1.3) still load-bearing for parser-bug containment. |
| ADR-061 §"Threat model" — software insider clause | **Preserved with edits.** Software-insider attacks on the host signer key and audit chain key are still in scope. The "secrets in process memory" threat goes away — there are no secrets to extract. |
| ADR-002 Claim 13 (no raw secret over broker) | **Rewritten** (see §"Security claims" below). |

## What remains unchanged from ADR-059 / ADR-061

- Wire format: JSON via `serde_json` for envelopes; JCS (RFC 8785) via `serde_jcs` for signed payloads.
- Algorithm-identifier byte in `AuthenticatedFrame` (§H-L4.1).
- Pre-spawn binary integrity check (§H-L3.1 — cosign verify; lands via #483).
- Signed config envelope (§H-L3.6 — wraps `SubprocessConfig` bytes; lands via #486).
- Per-spawn ephemeral subprocess response signing (§H-L4.2).
- Capability gating (five rules: schema, auth, binding, profile + quota, handler policy).
- Audit chain shape: JCS-canonical entries, `O_APPEND`-only FD, dir-immutable, anti-rollback chain-head persistence.
- Cross-VM via mvmd (iroh ALPN; mvmd Plan 52 + ADR-0023).
- `ExecutionPlan.services` schema bump 4 → 5 (`services: Vec<ServiceBinding>`).
- Hardware-enclave host signer (W8 — Apple SE + Linux TPM 2.0).
- Per-workload cgroup + namespace isolation (§H-L1.4).
- All §S* threats that aren't secrets-specific.

## What goes away

- `crates/mvm-secrets-dispatcher/` (entire crate)
- `crates/mvm-supervisor/src/services/secrets_proxy.rs`
- Plan 104 W5 (secrets dispatcher wiring)
- Plan 104 W7 (ADR-049 SDK matrix — Python/TS/Rust hook libraries)
- §H-L4.3 per-call session-key rotation (was secrets-specific timing-oracle defense)
- §S22 (audit batch durability for secrets) — replaced by generic audit-durability discussion
- §S24 (privileged composition leaks secrets) — no secrets to leak
- §S25 (SDK integrity / placeholder egress backstop) — no placeholders to bypass
- §S26 (cold-cache timing oracle on `host.secrets.v1`) — no service to oracle against
- §S27 (signed-plan revocation when host signer rotated for cause) — keep for plan signing context; rewrite to drop secrets framing
- §S28 (JCS for signed credentials) — keep but reframe: JCS is for *audit-entry* bytes-to-sign + future signed payloads, not for credentials specifically

## `host.audit.v1` service shape

New handler in `mvm-broker` (uid 903) implementing `ServiceHandler`:

| Verb | Verb-payload | Returns |
| --- | --- | --- |
| `emit` | One typed audit entry (category + fields) | `chain_head` after append |
| `emit_batch` | Vector of up to `BROKER_AUDIT_BATCH_MAX = 100` entries, total ≤ `BROKER_AUDIT_BATCH_BYTES = 256 KiB` | `chain_head` after final entry; per-entry status array |

**Per-record cap:** 4 KiB per entry (`BROKER_AUDIT_RECORD_BYTES = 4096`).
**Rate limit:** token-bucket with `BROKER_AUDIT_TOKENS_PER_SEC = 20` per workload (vs the broker-wide rate limit).
**Audit durability:** `PerCall` — the chain entry must be fsync'd before the response returns.
**EventCategory:** new `EventCategory::WorkloadAudit` variant in `mvm-audit-signer`'s allow-list, distinct from `ServiceCall` and `Admission` so the verifier can compute workload-asserted vs system-asserted entry rates separately.

The handler forwards each entry to the supervisor's `AuditSignerProxy::append_entry` with the `WorkloadAudit` category prefix. The audit-signer's existing chain-drift detection (Plan 104 §H-L5.1+H-L5.2) handles all the integrity invariants.

**Workload trust boundary:** entries are *workload-asserted* — the verifier records "workload X claimed this happened" semantics, not "supervisor observed this happened". Tooling that consumes the chain (`mvmctl audit verify`, future SIEM connectors) should display the category alongside the entry so operators can tell the source.

## Implementation choices unchanged from ADR-061

- Same Cargo dep pinning (`ed25519-dalek` v2.x, `serde_jcs`, `chacha20poly1305`, `tpm2-tss`, `subtle`).
- Same `subtle::ConstantTimeEq` discipline for security-byte comparisons.
- Same per-arch (x86_64 + aarch64) `seccompiler` deny-lists.
- Same `webauthn-authenticator-rs` for the W11 operator FIDO ceremony.

## Security claims

ADR-002's claim 12 stays (binding-gated service dispatch). **Claim 13 is rewritten** to apply to workload-emitted audit entries:

> **Claim 13 (rewritten).** Every workload-emitted audit entry (via `host.audit.v1`) is chain-signed by `mvm-audit-signer` under the `WorkloadAudit` category, distinguishable from supervisor-emitted entries in the audit chain. An entry whose bytes are tampered with after signing fails `mvmctl audit verify`; an entry claiming a workload id the caller doesn't own is refused at admission.

Two new tests verify the claim: `workload_audit_entries_chain_signed_with_workload_audit_category` + `workload_audit_entry_workload_id_mismatch_refused`.

The ADR-002 framework-references row for Claim 13 is rewritten too — drops the credential-exfiltration MITRE references; adds T1078 (Valid Accounts — unauthorized audit attribution) under `D3FEND: Authentication`.

## Consequences

**Positive:**

- Smaller v1 scope. Three subprocesses, not four. No SDK-matrix maintenance burden. No per-language hook library to fuzz.
- `host.audit.v1` becomes available from day one — workloads have a first-class audit emission path on the same chain as system events.
- Threat surface reduced: no hostile-guest matrix for SDK bypass, no placeholder-egress backstop in gvproxy/passt, no cold-cache timing oracle defense.
- All the W1a–W1b.2b.3 hardening (subprocess scaffolds, UDS proxies, spawn lifecycle, binary integrity check, signed config envelope) is preserved — none of that work is wasted.
- Single forcing function (audit) is simpler to reason about than two competing ones (audit + secrets).

**Negative:**

- ADR-049's substantial design work is now historical — superseded but not deleted (kept for future reference if the question of mvm-managed credentials comes up again).
- Operators who *want* a managed-credential service have to look elsewhere. mvm's stance becomes: "bring your own secret material; mvm's job is to launch and audit the workload."
- The W1b.1 `mvm-secrets-dispatcher` crate (PR #480, already merged) gets deleted as dead code under the no-backcompat rule. Mechanical work but visible in git history as scaffold-then-removal.
- Claim 13 changes meaning between this rewrite and any external references to its prior form (none known in the wild as of 2026-05-28; project-internal references will be updated in the same PR sequence).

## Non-goals (additions over ADR-061)

- **No "BYOK" secret-delivery path** in mvm's name. Workloads use their own credential pipelines.
- **No drop-in `host.secrets.v2`** placeholder. If a future ADR brings secrets back, it gets a fresh design rather than picking up where ADR-049 left off.
- **No backwards-compat shim** for callers expecting `host.secrets.v1`. Per the no-backcompat rule, callers either don't exist (the service was never deployed) or get a `NotBound` envelope.
- **No `host.logging.v1` in this rescope.** That stays in the host-logging follow-on plan. `host.audit.v1` is specifically the audit-chain emission path, not general structured logging.

## Migration

- Mechanical: `cargo build --workspace` no longer compiles `mvm-secrets-dispatcher`; tests no longer exercise `secrets_proxy.rs`.
- `mvmctl doctor` no longer reports the secrets dispatcher's status.
- The four-subprocess Plan 104 W1b series (PRs #480, #481, #482, #483, #486) stays merged; the secrets-specific crate is removed in a follow-on PR (PR C of the rescope sequence).
- No data migration — the secrets service was never deployed, so no live workloads depend on it.

## See also

- [Plan 104 — host services broker](../plans/104-host-services-broker.md) §"Rescope (ADR-062)" — the spec changes that land alongside this ADR
- [threat model 02 — host services broker](../threat-models/02-host-services-broker.md) §"Per-service threat walk" — `SECRET-*` tables removed; new `AUDIT-*` tables added
- [ADR-049 — secret substitution mechanism](049-secret-substitution-mechanism.md) — superseded
- [ADR-061 — host services broker — four-subprocess hardening](061-host-services-broker-hardening.md) — partially superseded (subprocess count reduced)
