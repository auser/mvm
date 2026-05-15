---
claim: 10-oci-image-provenance
status: Planned
gated_phrases:
  - "any OCI image"
  - "any container image"
  - "OCI image provenance"
  - "mvmctl image pull"
  - "mvmctl image export"
  - "mvmctl up --image"
  - "mvm-oci"
  - "OCI ingest"
  - "bidirectional OCI"
  - "OCI registry"
exempt_paths:
  - "specs/**"
  - "CHANGELOG.md"
  - ".github/**"
  - "memory/**"
  - "public/src/content/docs/contributing/adr/**"
  - "public/src/content/docs/security/sandbox-parity-status.md"
  - "xtask/src/check_doc_claims.rs"
  - "crates/mvm-oci/**"
  - "Cargo.toml"
  - "Cargo.lock"
  - "crates/mvm-build/src/oci_to_rootfs/**"
  - "crates/mvm-build/tests/oci_unpack_attacks.rs"
  - "crates/mvm-build/tests/oci_unpack_common/**"
---

# Claim 10 — OCI image provenance is recorded in the admission audit chain

## Assertion

Every `mvmctl up --image <ref>` admission emits an audit-chain entry
recording:

- The registry host that served the image
- The repo path
- The reference as supplied (tag or digest)
- The resolved manifest digest (sha256)
- The list of layer digests
- The cosign attestation summary (verified, unsigned, or refused)
- The trust policy in effect (`anonymous`, `keyring`, `cosign-required`)

`mvmctl audit verify` continues to detect drift on the audit chain.
Tampering with any field of an OCI provenance entry breaks the
chain HMAC.

## CI gate that ratifies the claim

Plan 75 W5 ships `tests/oci_provenance_audit.rs`:

- Pull `docker.io/library/alpine:3.20@sha256:<fixture-pin>` against
  a local-fixture registry; assert audit entry recorded with
  correct manifest digest, layer digest list, and trust policy.
- Byte-flip the audit entry's `resolved_digest` field; assert
  `mvmctl audit verify` exits nonzero with `E_AUDIT_CHAIN_BROKEN`.
- Pull a cosigned image with a project-pinned key; assert audit
  entry's `cosign_attestation` is `verified`.
- Pull an unsigned image under `--prod` policy with default
  posture; assert refusal and that no audit entry was emitted
  (rejection happens before admission).

## Status transitions

- **2026-05-14**: claim filed at status `Planned` (this PR).
- **Plan 75 W5 lands**: status flips to `Preview` once
  `mvmctl image pull` is in the CLI and the audit emit path is
  wired; the gate test is green but cosign is not yet shipped.
- **Plan 75 W6 lands**: status flips to `Shipped` once cosign
  verification is wired and the `--prod` policy is enforced.

## Why this claim needs a gate

Until the CI gate above passes, the docs/website/README must not
claim mvm verifies OCI image provenance. The claim is on the
roadmap — `Planned` — and the gated phrases list above blocks
premature use.

The phrase list is conservative on purpose: anything that mentions
the OCI pull, export, or ingest surface gets caught. Once a phrase
is admitted (status `Shipped`), the gate disengages and the docs
team can use it freely.

## Cross-refs

- ADR-002 §"Security model" — claims 1–8 (pre-plan-75) and 9 (deps).
- OCI provenance planning — defines this claim in its security
  implications section.
- CLAUDE.md §"Security model" — claim 10 line lands in CLAUDE.md
  when plan 75 W6 flips status to `Shipped`.
