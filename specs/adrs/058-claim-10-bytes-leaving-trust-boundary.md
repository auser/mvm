# ADR-058 — Claim 10: bytes leaving the trust boundary are encrypted, attested, and audited

**Status:** Proposed
**Sprint:** 56 (W2, W3, W4)
**Plan:** [Plan 101](../plans/101-in-guest-volume-encryption-and-gateway-audit.md)

## Context

ADR-002 §"Out of scope" today carves out *a malicious host* — "mvmctl trusts the host with the hypervisor and private build keys." That carve-out is correct as stated, but the current implementation is too permissive: it grants the host more capability than the threat model requires.

Specifically:

- **RW tenant volumes are plaintext.** `mvmctl volume create` produces an AES-256-GCM archive (`crates/mvm-security/src/secret_store.rs`) that protects the volume's host-side file at rest *when locked*. But once a volume is opened and mounted into a guest, the backing storage on the host is decrypted ext4 / virtiofs. A host process with read access to the volume directory can read every byte the workload is writing or reading.
- **Gateway flows are not in the audit chain.** `crates/mvm-core/src/policy/audit.rs` (`LocalAuditKind` enum) has plan/admission events and Stage 0 boot events, but no flow events. gvproxy (macOS) and passt (Linux) handle all guest network I/O at the host level; neither emits attested flow metadata. A compromised host can route, log, or exfil any traffic with no record landing in the chain.
- **App-deps volumes have integrity, not confidentiality.** dm-verity-sealed deps volumes (claim 9, [Plan 73](../plans/73-app-deps-audit-pipeline.md) / [ADR-047](047-app-deps-audit-pipeline.md)) prove "what's on disk hasn't been tampered with." They don't hide what's on disk. Different property.

Result: a host whose userland (not kernel — that's a different threat tier) has been compromised can read tenant data and silently exfil network traffic, and nothing in the audit chain notices.

## Threat model

This ADR narrows ADR-002's "out of scope: a malicious host" carve-out. The new posture:

- **Still trusted:** the hypervisor binary, the host kernel, the host signer's private key. Compromise of any of these defeats mvmctl's isolation by definition; out of scope here as in ADR-002.
- **No longer trusted (this ADR):** the host userland's *passive* read access. A user-space process on the host should not be able to (a) read tenant volume bytes at rest, or (b) exfil tenant network traffic, *without that fact being attested in the audit chain.*

Adversary capability assumed: read access to host filesystem and host network namespace. Not assumed: kernel module load, hypervisor hijack, signer key exfiltration (those are ADR-002's still-out-of-scope tier).

## Decision

Add claim 10 to ADR-002's CI-enforced security claims, in three legs.

### Leg 1 — Volume confidentiality

Every RW tenant volume is dm-crypt / LUKS-2 inside the guest. The host's view of the volume backing store is ciphertext at all times, even while the workload is running.

Key delivery: the signed `ExecutionPlan` carries a `Vec<EncryptedVolumeKey>` — per-volume symmetric keys, each wrapped under the tenant's pubkey. The mvm-supervisor never sees plaintext key material; it materializes the wrapped keys to an in-VM ramfs (never to host disk) and hands the ramfs path to the guest initramfs via kernel cmdline. The guest unwraps inside the VM using the tenant private key, escrowed by mvmd (cross-repo dep).

### Leg 2 — Network traffic audit

gvproxy (macOS) and passt (Linux) are wrapped with a control-socket listener that streams per-flow events to `mvm-supervisor`. Events: `flow_opened`, `flow_closed`, aggregated `flow_bytes` (every N seconds or on close), and `flow_policy_decision` (every deny / allow). All events land in the chained `~/.mvm/audit/<tenant>.jsonl`. The existing `mvm_supervisor::verify_audit_chain` mechanism extends to flow events with no schema break beyond the new enum variants.

No L7 inspection. No TLS termination. Only connection metadata and byte counts.

### Leg 3 — Crypto state attestability

Every key fingerprint, key rotation, and key-unwrap-failure event lands in the audit chain. `mvmctl audit verify` covers volume-key events alongside flow events alongside the existing plan events. A new CI lane `claim-10-audit-tamper` exercises tamper detection: emit a known sequence, byte-flip one entry, assert `mvmctl audit verify` exits non-zero.

## Out of scope (named, like ADR-002)

- **TLS termination / L7 packet inspection.** Different threat model (needs decryption, key material).
- **Host filesystem encryption (FDE).** That's the user's concern — full-disk encryption protects host backups; this ADR protects per-volume at-rest exposure during active workload runs.
- **Hardware-backed key attestation.** Post claim-10 future work.
- **Per-byte traffic audit.** Aggregated flow_bytes only ([Plan 101](../plans/101-in-guest-volume-encryption-and-gateway-audit.md) W8).

## Consequences

- `ExecutionPlan` grows: `volume_keys: Vec<EncryptedVolumeKey>` and `network_audit: NetworkAuditConfig` fields. PROTOCOL_VERSION bump in `crates/mvm-core/src/protocol/protocol.rs`.
- mvmd cross-repo work: tenant root key, key derivation, rotation policy. Tracked as Plan 101 W5.
- `mvmctl doctor` gains a `claim_10` row reporting LUKS-in-guest active + gateway audit emitting + audit chain valid.
- New CI lane: `claim-10-audit-tamper` byte-flip test gates every PR.
- Performance: dm-crypt overhead measurable on hot-path volume reads. Plan 101 W14 validates within threshold; backs out if pathological.

## References

- [ADR-002](002-microvm-security-posture.md) — microVM security posture (claim list extends to 10)
- [ADR-041](041-signed-audited-execution-plans.md) — claim 8, signed ExecutionPlans (volume keys ride this signing path)
- [ADR-047](047-app-deps-audit-pipeline.md) — claim 9, app-deps audit (analogous structure for the audit chain extension)
- [ADR-055](055-cross-platform-backends.md) — gvproxy / passt gateway choice
- [Plan 101](../plans/101-in-guest-volume-encryption-and-gateway-audit.md) — implementation rollout
