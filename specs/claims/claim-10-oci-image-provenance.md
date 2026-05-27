---
claim: 10-oci-image-provenance
status: Shipped
gated_phrases:
  - "any OCI image"
  - "any container image"
  - "OCI image provenance"
  - "mvmctl image pull"
  - "mvmctl image export"
  - "mvmctl up --image"
  - "mvmctl run --image"
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
  - "crates/mvm-cli/Cargo.toml"
  - "crates/mvm-cli/src/commands/image.rs"
  - "crates/mvm-cli/src/commands/vm/audit_chain.rs"
  - "crates/mvm-cli/src/commands/vm/exec.rs"
  - "crates/mvm-build/src/oci_to_rootfs/**"
  - "crates/mvm-build/tests/oci_unpack_attacks.rs"
  - "crates/mvm-build/tests/oci_unpack_common/**"
  - "crates/mvm-build/tests/oci_ext4_materialization.rs"
  - "crates/mvm-build/tests/oci_verity_sealing.rs"
  - "public/src/content/docs/reference/cli-commands.md"
  - "crates/mvm-libkrun/fuzz/rust-toolchain.toml"
---

# Claim 10 — OCI image provenance is recorded in the admission audit chain

## Assertion

Every `mvmctl run --image <ref>` admission emits an audit-chain entry
recording:

- The registry host that served the image
- The repo path
- The reference as supplied (tag or digest)
- The resolved manifest digest (sha256)
- The list of layer digests
- The current verification status
- The trust policy in effect

`mvmctl audit verify` continues to detect drift on the audit chain.
Tampering with any field of an OCI provenance entry breaks the
chain signature.

## CI gate that ratifies the claim

Plan 85 Phase E/F ships focused CLI unit coverage:

- Cached image resolution returns a provenance record with registry,
  repo, supplied ref, resolved digest, layer digest list, trust policy,
  and verification status.
- `AuditEmitter::emit_oci_provenance` writes `plan.oci_provenance`
  with those labels and `verify_audit_chain` verifies the resulting
  signed chain.
- `mvmctl run --image` admits an execution plan before launch and
  emits `plan.admitted` followed by `plan.oci_provenance`.
- `--prod` policy still refuses mutable references before pull or
  boot.
- Production OCI policy parses registry allow-lists and trusted
  cosign identities, rejects signature opt-outs, rejects denied
  registries before verification, and rejects missing or invalid
  signatures.

## Status transitions

- **2026-05-14**: claim filed at status `Planned` (this PR).
- **2026-05-20 / Plan 85 Phase E**: status flips to `Preview` because
  `mvmctl image pull` persists provenance metadata and
  `mvmctl run --image` emits a chain-signed `plan.oci_provenance`
  admission event. Cosign / registry policy remains tracked in #407.
- **2026-05-20 / Plan 85 Phase F**: status flips to `Shipped` because
  production OCI pulls and `run --image --prod` require a
  digest-pinned reference, an OCI registry policy, and cosign
  verification of the resolved digest before cache admission or boot.

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
- CLAUDE.md §"Security model" — claim 10 line landed when Plan 85
  finalization flipped the plan status to `Shipped` (2026-05-26).
