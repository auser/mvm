# HIPAA Security Rule — Mapping

**Status:** STUB. Filled out in Phase 9 of `specs/plans/60-mvm-microsandbox-migration.md`.
**Last verified:** N/A (stub created 2026-05-07).
**Owner:** mvm + mvmd platform team.
**Scope:** the open-source `mvm` library + the hosted mvmd cloud (when launched and only after a Business Associate Agreement is signed).

## Default posture: BAA-required

The hosted mvmd cloud will require a Business Associate Agreement before customers can store Protected Health Information. The mvm library itself is the substrate; HIPAA compliance is an operational property of a deployment, not the library.

This document maps each technical safeguard from 45 CFR §164.312 (the HIPAA Security Rule's Technical Safeguards) to the implementing artifact in the mvm codebase.

## §164.312(a) — Access Control

### (a)(1) — Unique user identification (Required)
- [ ] (TBD) Per-VM Ed25519 identity key (ADR-018)
- [ ] (TBD) Per-tenant signing key (mvm-plan)

### (a)(2)(i) — Emergency access procedure (Required)
- [ ] (TBD) `mvmctl tenant destroy` (ADR-028) — emergency deprovisioning
- [ ] (TBD) Recovery key escrow (opt-in) — documented in plan 60

### (a)(2)(ii) — Automatic logoff (Addressable)
- [ ] (TBD) Session idle timeout (`mvmctl session timeout`) — Phase 7

### (a)(2)(iii) — Encryption and decryption (Addressable)
- [ ] (TBD) Volume LUKS encryption (Phase 2)
- [ ] (TBD) Snapshot AEAD encryption (Phase 2)

## §164.312(b) — Audit Controls

- [ ] (TBD) Chain-signed HMAC audit log (ADR-019)
- [ ] (TBD) Audit total-coverage test (`tests/audit_total_coverage.rs`)
- [ ] (TBD) Audit categories: cmd, lifecycle, secret, flow, plan, policy, key, host, audit
- [ ] (TBD) Audit shipping to remote sink (`audit-remote-sink` feature)

## §164.312(c) — Integrity

### (c)(1) — Mechanism to authenticate ePHI
- [ ] (TBD) dm-verity rootfs integrity (Firecracker tier)
- [ ] (TBD) AEAD authentication on snapshots
- [ ] (TBD) HMAC chain on audit log

## §164.312(d) — Person or Entity Authentication

- [ ] (TBD) Attestation chain (ADR-018)
- [ ] (TBD) mTLS at mvmd-agent ↔ mvm-hostd hop (ADR-027)
- [ ] (TBD) Ed25519 identity keys per VM

## §164.312(e) — Transmission Security

### (e)(1) — Integrity controls
- [ ] (TBD) AuthenticatedFrame on vsock (ADR-026)
- [ ] (TBD) Replay protection (nonce + monotonic timestamp)

### (e)(2)(i) — Encryption (Addressable)
- [ ] (TBD) iroh QUIC TLS 1.3 (ADR-027)
- [ ] (TBD) mTLS at hostd hop
- [ ] (TBD) X25519 ephemeral session keys for vsock (forward secrecy)

## Operational requirements (out of mvm's scope, in mvmd's)

The HIPAA Security Rule has Administrative and Physical safeguards (§164.308 and §164.310) that are operational by nature: workforce training, contingency plans, facility access controls, etc. These belong to the deployer (mvmd hosted cloud or self-hoster), not the mvm library.

The Privacy Rule (45 CFR §164.500-534) is similarly operational and out of scope here.

## Breach notification

§164.404 requires breach notification within 60 days. Implementation is mvmd's: the hosted cloud will integrate the audit log + flow events into its incident-response system. mvm's contribution is making sure the events are *recordable* (not making them, that's the operator's job).
