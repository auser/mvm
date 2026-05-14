---
title: "ADR-032: Hosted-cloud invariants — no lock-in, metering precision, compliance-ready"
status: Proposed
date: 2026-05-07
related: ADR-002 (security posture), ADR-029 (compliance), plan 60-mvm-libkrun-migration
---

## Status

Proposed. Invariants are enforced from Phase 0 (no hosted-cloud-only paths in mvm) through Phase 9 (compliance docs).

## Context

A future hosted, monetized mvmd P2P cloud is on the roadmap. We don't want to wake up at month 12 of the migration and discover the open-source library and the hosted product have diverged — that's both a license-compliance hazard and a vendor-lock-in trap that conflicts with the project's stated values.

Conversely, we don't want to under-invest in primitives the hosted cloud will depend on (precise metering, attestation, compliance docs) and pay a "hosted retrofit" tax later.

The right move is a small set of invariants the hosted cloud can rely on, enforced in mvm from day one of the migration.

## Decision

Five invariants. Every PR is reviewed against them.

### I1. No hosted-cloud-only code paths in `mvm` or `mvmctl`
The open-source library remains fully self-hostable. Anything specific to the hosted offering (e.g., Stripe billing, customer-tier-specific limits, hosted-only auth flows) lives in `mvmd` or higher. CI lint: grep for `cfg(feature = "hosted-only")` in mvm; fail if present.

### I2. Metering primitives are precise and tamper-evident
The metrics catalog (plan 60 §"Comprehensive metrics catalog") covers:
- Per-tenant CPU seconds, memory bytes, disk bytes, build seconds, egress bytes
- Per-tenant tool RPC counts (web_search, web_fetch, code_eval, …)
- Per-tenant snapshot bytes, snapshot count
- Per-tenant audit event count

Each metric is tied to a tenant_hash label (truncated SHA-256 of tenant_id; never raw tenant_id). Audit-logged on every read so dispute-resolution has an evidence trail.

### I3. Attestation is the basis of trust in hosted mode
In a self-hosted setup, attestation is good hygiene; in the hosted cloud, it's the basis of trust between customer and operator. Phase 6 implements attestation **at the strictness level the hosted cloud will require**, not a weaker dev-only version. mvmd may refuse unattested requests when running in hosted mode (a config flag, not a code fork).

### I4. Compliance docs are first-class artifacts
Phase 9 ships `specs/compliance/{soc2-controls,pci-scope,hipaa-mapping,gdpr-mapping}.md`. Stubs are created in Phase 0. Each compliance control maps to a test or ADR ID. CI checks doc staleness via timestamp on the "last verified" field.

### I5. Customer-facing data destruction is provable
`mvmctl tenant destroy` (Phase 7a) emits a destruction certificate signed by the host identity key. Customers ending service get cryptographic proof their data is gone. This is a hosted-cloud requirement we choose to bake into the open-source primitive so self-hosters get it too.

## Consequences

**Positive**:
- Open-source library and hosted product never diverge silently.
- Compliance retrofitting later costs ~zero — the docs and primitives are already shipped.
- Customers (current and future) get a clean migration story off the hosted cloud if they want.

**Negative**:
- Some hosted-cloud-friendly shortcuts (e.g., baking in a default Stripe billing flow) are explicitly forbidden. mvmd takes the cost.
- Compliance docs are work overhead from day one; mitigated by template ADRs and table-of-contents-driven authoring.

## Alternatives considered

- **Build the hosted product first, retrofit the OSS later**: rejected. Retrofits never happen cleanly.
- **Build the OSS without hosted-cloud awareness**: rejected. Means month-12 retrofit of attestation, metering, destruction certificates — far more painful.

## Threat model impact

- I3 makes attestation a load-bearing security property in hosted mode. Threat model documented in ADR-018 (attestation chain).
- I5 is itself a security property: providing a proof-of-deletion has cryptographic guarantees attached. Documented in ADR-028 (tenant destruction).

## Compliance impact

- SOC 2: positive — I2, I4, I5 each map to specific Trust Services Criteria.
- PCI: neutral — these invariants don't add PCI scope; ADR-029 covers PCI specifically.
- HIPAA: positive — I3 (authentication via attestation), I5 (data deletion proof) align with §164.312.
- GDPR: positive — I5 satisfies right-to-erasure (Art. 17).
