# SOC 2 Type II — Controls Mapping

**Status:** STUB. Filled out in Phase 9 of `specs/plans/60-mvm-libkrun-migration.md`.
**Last verified:** N/A (stub created 2026-05-07).
**Owner:** mvm + mvmd platform team.
**Scope:** the open-source `mvm` library + the hosted mvmd cloud (when launched).

This document maps each SOC 2 Trust Services Criterion to the implementing artifact in the mvm codebase: a code path, a test, an ADR, or a CI gate. Auditors get a living traceability matrix; developers get a single source of truth for "what control does this PR affect."

## Trust Services Criteria mapping (to be filled in Phase 9)

### CC1 — Control Environment
- [ ] (TBD) Documented governance model
- [ ] (TBD) Code-quality controls (ADR-033)
- [ ] (TBD) Two-person review for security paths (CODEOWNERS)

### CC2 — Communication and Information
- [ ] (TBD) Audit log structure + chain-signed envelope
- [ ] (TBD) Customer-facing posture statement

### CC3 — Risk Assessment
- [ ] (TBD) Threat models per ADR (STRIDE tables)
- [ ] (TBD) AI-agent threat model (ADR-036)

### CC4 — Monitoring Activities
- [ ] (TBD) Metrics catalog coverage (plan 60 §"Comprehensive metrics catalog")
- [ ] (TBD) Audit total-coverage test (`tests/audit_total_coverage.rs`)

### CC5 — Control Activities
- [ ] (TBD) Encryption layers (ADR-027)
- [ ] (TBD) Access controls (mvm-policy)
- [ ] (TBD) Default-deny network egress (ADR-017)

### CC6 — Logical and Physical Access Controls
- [ ] (TBD) mTLS at hostd hop
- [ ] (TBD) Attestation chain (ADR-018)
- [ ] (TBD) Tenant isolation (cgroup, bridge, signing key per tenant)

### CC7 — System Operations
- [ ] (TBD) SLO commitments (plan 60 §"Reliability and SLOs")
- [ ] (TBD) Incident response runbooks (`specs/runbooks/`)

### CC8 — Change Management
- [ ] (TBD) ADR coverage gate (`xtask check-adr-coverage`)
- [ ] (TBD) Reproducibility double-build (Phase 9)
- [ ] (TBD) Cosign-signed releases (Phase 9)

### CC9 — Risk Mitigation
- [ ] (TBD) PII redaction (ADR-020)
- [ ] (TBD) Continuous fuzzing (Phase 9)
- [ ] (TBD) Vulnerability disclosure (`SECURITY.md`)

### Availability (A)
- [ ] (TBD) Per-VM crash rate target < 0.1%
- [ ] (TBD) Builder warm-pool 99.9%
- [ ] (TBD) Pause/resume correctness test

### Processing Integrity (PI)
- [ ] (TBD) Reproducibility check
- [ ] (TBD) Signed Plan protocol (ADR-018, mvm-plan crate)
- [ ] (TBD) Audit chain integrity

### Confidentiality (C)
- [ ] (TBD) Encryption at rest (LUKS, AEAD snapshots)
- [ ] (TBD) Encryption in transit (ADR-027)
- [ ] (TBD) Tenant destruction certificates (ADR-028)

### Privacy (P)
- [ ] (TBD) PII redaction (ADR-020)
- [ ] (TBD) Opt-in telemetry only
- [ ] (TBD) GDPR right-to-erasure (`mvmctl tenant destroy`)
