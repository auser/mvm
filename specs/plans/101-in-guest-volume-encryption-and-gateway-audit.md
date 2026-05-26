# Plan 101 — Claim 10: in-guest volume encryption + gateway audit

**Sprint:** 56 (W2, W3, W4)
**ADR:** [ADR-058](../adrs/058-claim-10-bytes-leaving-trust-boundary.md)
**Status:** Proposed

## Goal

Add a CI-enforced security claim 10 to ADR-002: bytes leaving the trust boundary are encrypted at rest, attested through the audit chain, and undetectable network exfil is impossible. Three legs (volume confidentiality, gateway audit, crypto attestability), 14 waves total.

## Wave breakdown

### Leg 1 — Volume confidentiality

- [ ] **W1 — LUKS-in-guest substrate.** Add `cryptsetup` to the guest initramfs (via `nix/images/builder-vm/` initramfs builder). Add a new `mvm-luks-init` binary alongside the existing `mvm-verity-init`. `mvm-luks-init` reads a key file path from kernel cmdline (`mvm.luks_keys=/run/keys/...`), unlocks one or more LUKS-2 volumes via `/dev/disk/by-id/`, and exits before pivot-root.

- [ ] **W2 — ExecutionPlan schema.** Add `volume_keys: Vec<EncryptedVolumeKey>` to `crates/mvm-plan/src/plan.rs`. Each `EncryptedVolumeKey` carries `{ volume_id, wrapped_key: Vec<u8>, kdf_params, integrity_tag }`. Wrap under tenant pubkey at deploy time, sign the surrounding plan with the host signer. Extend `mvm_plan::verify_plan` to validate the wrapped-key envelope before admission. Bump `PROTOCOL_VERSION` in `crates/mvm-core/src/protocol/protocol.rs`.

- [ ] **W3 — Supervisor wiring.** `mvm-supervisor` materializes wrapped keys to an in-VM ramfs at `/run/mvm/keys/` (never on host disk) and points `mvm-luks-init` at them via kernel cmdline. Extend `xtask check-no-display-on-secret-types` to cover the new `EncryptedVolumeKey` type so accidental `Debug` / `Display` impls fail CI.

- [ ] **W4 — mvm-storage backend.** Storage backend recognizes `VolumeAttach { encryption: Some(EncryptionConfig) }` (extend `VolumeAttach` in `crates/mvm-core/src/protocol/protocol.rs:65-79`). On create, emit a LUKS-2 header sidecar at `~/.mvm/volumes/<id>/header.luks2`; on attach, validate the sidecar matches the wrapped key in the plan.

- [ ] **W5 — Cross-repo gate (mvmd).** Coordinate with mvmd: tenant root key derivation (HKDF from tenant secret), per-volume key wrapping API, rotation policy. Tracked as the cross-repo dependency for Sprint 56. mvm side ships with a `mvmctl deploy --tenant-key-source <path>` fallback for local-dev / single-tenant flows that bypass mvmd.

### Leg 2 — Gateway traffic audit

- [ ] **W6 — Gateway audit substrate.** Wrap gvproxy (macOS) and passt (Linux) with a control-socket listener that streams flow events to `mvm-supervisor`. gvproxy wiring lands in the area already TODO'd at `crates/mvm-backend/src/vz.rs:809-811`; passt wiring extends the existing `PasstHandle` in `crates/mvm-libkrun/sys`. Socket path: `~/.mvm/audit/gateway-<instance>.sock` (mode 0700, supervisor-only).

- [x] **W7 — Audit event schema.** Extend `LocalAuditKind` in `crates/mvm-core/src/policy/audit.rs` with `flow_opened`, `flow_closed`, `flow_bytes`, `flow_policy_decision`. Each variant carries `{ instance_id, tenant_id, 5-tuple, bytes_sent, bytes_recv, started_at, ended_at }`. Hash-chain integrity preserved — new variants don't break existing `verify_audit_chain`.

- [ ] **W8 — Sample-rate / aggregation policy.** Per-byte audit is too noisy; emit aggregated `flow_bytes` on flow close + every 30s for long-lived flows. Configurable per tenant in the `NetworkAuditConfig` ExecutionPlan field. Default: 30s.

- [ ] **W9 — CLI surface.** `mvmctl audit traffic --tenant X --since Y --format json` to surface flow events without re-parsing JSONL by hand. Extend `mvmctl audit verify` to validate the chain across all event kinds (plan + Stage 0 + flow + volume-key).

- [ ] **W10 — CI tamper gate.** New job `claim-10-audit-tamper` in `.github/workflows/security.yml`: emit a known sequence of flow events into a temp audit log, byte-flip one entry, assert `mvmctl audit verify` exits non-zero. Run on every PR.

### Leg 3 — Crypto state attestability

- [ ] **W11 — Key-fingerprint events.** Add `volume_key_bound`, `volume_key_rotated`, `volume_unwrap_failed` to `LocalAuditKind`. Chain entries on every key event. Surfaces in `mvmctl audit verify`.

- [ ] **W12 — Doctor probe.** New `claim_10` row in `mvmctl doctor`: reports (a) LUKS-in-guest substrate present in current builder VM image, (b) gateway audit socket connectable, (c) most-recent audit chain valid. Each leg fail-closed with actionable error.

- [ ] **W13 — Docs.** Add claim 10 to CLAUDE.md security model (extending the existing 1–9 list). Reference ADR-058 + Plan 101. Update `public/src/content/docs/guides/security.md` (if it exists) with the new threat model.

- [ ] **W14 — Performance validation.** Measure dm-crypt overhead on hot-path volume reads (random 4K reads, sequential 1M reads). Document threshold (target: <10% on random, <5% on sequential). Back out the leg or re-evaluate KDF/cipher choice if pathological.

### Cross-leg

- [ ] **W15 — Hypervisor guest-memory pinning.** Add `mlockall` to libkrun and Firecracker launch wrappers to prevent host swap of guest plaintext. Without this, claim 10 leg 1 (volume confidentiality) has a residual gap: if a host runs with swap enabled, plaintext volume contents currently resident in the guest page cache can land on host swap files. `mvmctl doctor` probe warns if host swap is active without memory locking. libkrun pins guest memory by default on macOS; Firecracker on Linux needs explicit `mlockall` at launch.

## Critical files

- `crates/mvm-plan/src/plan.rs` (W2: ExecutionPlan schema)
- `crates/mvm-core/src/protocol/protocol.rs` (W2: PROTOCOL_VERSION bump; W4: VolumeAttach extension)
- `crates/mvm-core/src/policy/audit.rs` (W7, W11: event-kind extensions)
- `crates/mvm-supervisor/` (W3: key materialization; W6: gateway socket)
- `crates/mvm-storage/` (W4: LUKS header sidecar)
- `crates/mvm-libkrun/` (W6: passt wrapper; W15: mlockall on launch)
- `crates/mvm-backend/src/vz.rs` (W6: gvproxy wrapper)
- `crates/mvm-backend/src/firecracker.rs` (W15: mlockall on launch)
- `crates/mvm-cli/src/commands/audit.rs` (W9: new subcommand)
- `crates/mvm-cli/src/commands/doctor.rs` (W12: claim_10 row; W15: host-swap probe)
- `nix/images/builder-vm/` (W1: initramfs additions for `mvm-luks-init`)
- `CLAUDE.md` (W13: claim 10 in security model)
- `specs/adrs/002-microvm-security-posture.md` (W13: claim list extension)
- `.github/workflows/security.yml` (W10: tamper-gate CI lane)

## Out of scope

- TLS termination / L7 packet inspection (different threat tier).
- Host filesystem encryption (user's FDE concern, not mvm's).
- Hardware-backed key attestation (post claim-10 future work).
- Per-byte traffic audit (W8 aggregates to keep audit volume sane).

## Verification

- W1: `mvmctl dev shell` followed by `cryptsetup --version` returns a version string inside the workload.
- W2: PROTOCOL_VERSION incremented; a downgraded mvmd refuses the new plan with a clear error.
- W6: `nc -U ~/.mvm/audit/gateway-<instance>.sock` shows a stream of flow events while a workload makes outbound HTTP.
- W9: `mvmctl audit traffic --tenant X` returns a JSON array of flow events for a recent workload run.
- W10: CI lane green on the PR introducing the test; goes red if a developer breaks chain validation.
- W12: `mvmctl doctor` reports `claim_10 ✓` on a clean install; reports `claim_10 ✗ (LUKS substrate missing)` if `mvm-luks-init` is removed from the builder VM image.
- W14: benchmark numbers attached as a follow-up comment to this plan.
- W15: `mvmctl doctor` reports `claim_10_no_swap_leak ✓` on a host with swap disabled or `mlockall` succeeded; reports `✗` (with actionable error) on a host where swap is active and memory locking failed.
