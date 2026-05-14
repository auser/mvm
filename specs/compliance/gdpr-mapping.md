# GDPR — Mapping

**Status:** STUB. Filled out in Phase 9 of `specs/plans/60-mvm-libkrun-migration.md`.
**Last verified:** N/A (stub created 2026-05-07).
**Owner:** mvm + mvmd platform team.
**Scope:** the open-source `mvm` library + the hosted mvmd cloud (when offered to EU customers).

## Default posture: data-minimization-by-default

GDPR is largely operational (privacy notices, lawful basis, controller/processor agreements). The technical aspects mvm must support are limited to data minimization, right-to-erasure, and breach detection.

## Articles mapped to mvm capabilities (Phase 9 to fill)

### Article 5 — Principles of processing

- [ ] (TBD) Data minimization: PII redactor (ADR-020) reduces what's logged.
- [ ] (TBD) Storage limitation: snapshot retention policies (plan 60 §"Snapshots — first-class feature") + audit log rotation.
- [ ] (TBD) Integrity and confidentiality: encryption layers (ADR-027).

### Article 17 — Right to erasure ("right to be forgotten")

- [ ] (TBD) `mvmctl tenant destroy` (ADR-028) emits a destruction certificate signed by the host identity key.
- [ ] (TBD) LUKS keyslot revocation + zero-fill on volumes.
- [ ] (TBD) Snapshot DEK destruction (cryptographic erasure).
- [ ] (TBD) Per-tenant audit log entries retained as redacted-only or destroyed (configurable; legal-hold default keeps redacted forms).

### Article 20 — Right to data portability

- [ ] (TBD) `mvmctl snapshot export` produces a portable, signed bundle of the VM state.
- [ ] (TBD) `mvmctl audit export --tenant <id>` produces a portable, signed audit bundle.

### Article 25 — Data protection by design and by default

- [ ] (TBD) Default-deny network egress (ADR-017).
- [ ] (TBD) Encryption-everywhere (plan 60).
- [ ] (TBD) Opt-in telemetry only.

### Article 30 — Records of processing activities

- [ ] (TBD) Audit chain (ADR-019) provides authoritative records.
- [ ] (TBD) Per-tenant query: `mvmctl audit export`.

### Article 32 — Security of processing

- [ ] (TBD) Encryption (in transit + at rest) per ADR-027.
- [ ] (TBD) Pseudonymization where applicable (PII redactor's tokenization scheme).
- [ ] (TBD) Resilience (snapshot pool + supervisor restart).
- [ ] (TBD) Process for regularly testing (continuous fuzzing, reproducibility).

### Article 33 — Notification of personal data breach (to supervisory authority within 72 hours)

- [ ] (TBD) Operational; mvmd hosted cloud handles. mvm contribution: audit events (ADR-019) capture every flow and tool call, enabling reconstruction.

### Article 34 — Communication of breach to data subject

- [ ] (TBD) Operational; mvmd handles.

### Article 35 — Data Protection Impact Assessment

- [ ] (TBD) Operational artifact; templated alongside ADR-018, ADR-029.

## Cross-border transfer

- [ ] (TBD) Operational; mvmd's choice of relay servers (iroh) and storage regions impacts this. Out of mvm's scope.

## Data subject access requests (Articles 15-22)

- [ ] (TBD) `mvmctl audit export --tenant <id>` and `mvmctl snapshot export <id>` provide the technical primitives. mvmd hosted cloud wraps them in a self-service UX (post-launch).
