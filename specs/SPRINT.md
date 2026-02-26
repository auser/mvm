# mvm Sprint 13: Securing OpenClaw

Previous sprints:
- [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete)
- [SPRINT-2-production-readiness.md](sprints/SPRINT-2-production-readiness.md) (complete)
- [SPRINT-3-real-world-validation.md](sprints/SPRINT-3-real-world-validation.md) (complete)
- Sprint 4: Security Baseline 90% (complete)
- Sprint 5: Final Security Hardening (complete)
- [SPRINT-6-minimum-runtime.md](sprints/SPRINT-6-minimum-runtime.md) (complete)
- [SPRINT-7-role-profiles.md](sprints/SPRINT-7-role-profiles.md) (complete)
- [SPRINT-8-integration-lifecycle.md](sprints/SPRINT-8-integration-lifecycle.md) (complete)
- [SPRINT-9-openclaw-support.md](sprints/SPRINT-9-openclaw-support.md) (complete)
- [SPRINT-10-coordinator.md](sprints/SPRINT-10-coordinator.md) (complete)
- [SPRINT-11-dev-environment.md](sprints/SPRINT-11-dev-environment.md) (complete)
- [SPRINT-12-install-release.md](sprints/SPRINT-12-install-release.md) (complete)

---

## Motivation

The vsock protocol between host and guest has no authentication, no command validation, no threat detection, and no health monitoring. OpenClaw agents run inside Firecracker microVMs as adversarial-by-default workloads — they will drift, forget rules, and silently fail. This sprint closes the security gaps identified in the [OpenClaw security research](research/openclaw-security.md) and implements the [securing plan](plans/14-securing-openclaw.md).

All security improvements are native Rust. SafeClaw and the OpenClaw Field Manual are reference material only — no integration.

## Baseline

| Metric            | Value           |
| ----------------- | --------------- |
| Workspace crates  | 5 + root facade |
| Lib tests         | 366             |
| Integration tests | 10              |
| Total tests       | 376             |
| Clippy warnings   | 0               |

---

## Phase 1: Authenticated Vsock Protocol
**Status: PENDING**

**Goal:** Ed25519-signed vsock frames with per-session keys provisioned via the secrets drive.

- [ ] Add `SecurityPolicy`, `AuthenticatedFrame`, `SessionHello`/`SessionHelloAck`, `AccessPolicy`, `RateLimitPolicy` types in new `mvm-core/src/security.rs`
- [ ] Add `pub mod security` to `mvm-core/src/lib.rs`
- [ ] Implement authenticated frame wrappers (`write_authenticated_frame`, `read_authenticated_frame`) in `mvm-guest/src/vsock.rs`
- [ ] Add `ed25519-dalek` dependency to `mvm-guest/Cargo.toml`
- [ ] Add session key generation to `mvm-runtime/src/security/signing.rs`
- [ ] Implement challenge-response handshake (`SessionHello` → `SessionHelloAck`) after existing `CONNECT/OK`
- [ ] Key provisioning: host writes per-session keypair to secrets drive (`/mnt/secrets/vsock/`) before VM boot
- [ ] Version negotiation: fall back to unauthenticated mode if guest responds `version: 1`
- [ ] Default `require_auth: false`, opt-in via `--require-vsock-auth`
- [ ] Tests: frame signing roundtrip, serde roundtrip, challenge-response handshake (mock UnixStream), tampered frame rejection, replay detection via sequence numbers

## Phase 2: Command Gating
**Status: PENDING**

**Goal:** Host-side blocklist for vsock commands. Matching commands are blocked or held for approval.

- [ ] Add `GateDecision`, `ApprovalVerdict`, `BlocklistEntry` types to `mvm-core/src/security.rs`
- [ ] Create `mvm-runtime/src/security/command_gate.rs` — Aho-Corasick literal matching + glob wildcards
- [ ] Gate logic: non-match → allow, Block → reject, RequireApproval → hold (dev mode: auto-approve with warning)
- [ ] Log every gate decision to audit trail
- [ ] Harden builder agent (`mvm-guest/src/bin/mvm-builder-agent.rs`): validate `flake_ref` against allow-list, validate `attr` starts with `packages.`, reject when `access.build == false`
- [ ] Export `command_gate` from `mvm-runtime/src/security/mod.rs`
- [ ] Tests: blocklist matching, gate decision logic, builder flake_ref validation, blocked command returns error via vsock

## Phase 3: Threat Classification + Audit Extension
**Status: PENDING**

**Goal:** Classify every vsock message against 10 threat categories using idiomatic Rust (not a wall of regex). Extend audit trail.

- [ ] Add `ThreatCategory` (10 variants), `ThreatFinding`, `Severity` types to `mvm-core/src/security.rs`
- [ ] Create `mvm-runtime/src/security/threat_classifier.rs` with three-tier detection:
  - [ ] Tier 1: Aho-Corasick multi-pattern matching (~200 literals, single O(n) scan) — credential prefixes, destructive commands, exfil domains, privilege escalation, system paths, Firecracker-specific
  - [ ] Tier 2: Typed Rust pattern matching (str methods + match arms) — path analysis, command structure, credential format, network patterns, permission parsing, Nix-specific
  - [ ] Tier 3: Regex only for complex patterns (~20-30 via `RegexSet`) — AWS key format, JWT tokens, base64 payloads, obfuscation, shell injection
- [ ] MicroVM-specific patterns: Firecracker escape, Nix sandbox breakout, cgroup escape, seccomp bypass, vsock abuse
- [ ] Add `aho-corasick` dependency to `mvm-runtime/Cargo.toml`
- [ ] Extend `AuditEntry` in `mvm-core/src/audit.rs`: add `threats`, `gate_decision`, `frame_sequence` fields (`#[serde(default)]`)
- [ ] Add `AuditAction` variants: `VsockSessionStarted`, `VsockSessionEnded`, `VsockFrameReceived`, `CommandBlocked`, `CommandApproved`, `CommandDenied`, `ThreatDetected`, `RateLimitExceeded`, `SessionRecycled`
- [ ] Wire classifier into audit event emission in `mvm-runtime/src/security/audit.rs`
- [ ] Tests: per-category classification, benign message produces no findings, `AuditEntry` backward compat, performance (<10ms for 1000 frames)

## Phase 4: Health Monitoring + Session Lifecycle + Rate Limiting
**Status: PENDING**

**Goal:** Host-side vsock health checks with kill/restart. VM session recycling. Frame rate limiting.

- [ ] Create `mvm-runtime/src/security/health_monitor.rs` — periodic vsock Ping, consecutive failure tracking, kill + restart after N failures, audit logging
- [ ] Create `mvm-runtime/src/security/session_manager.rs` — per-VM session state (started_at, tasks_completed, frames_sent/received), recycle on `max_lifetime_secs` or `max_tasks`, graceful drain via SleepPrep
- [ ] Create `mvm-runtime/src/security/rate_limiter.rs` — sliding window token bucket, configurable frames_per_second/frames_per_minute, exceeded frames dropped + audit event
- [ ] Add session + rate limit config types to `mvm-core/src/security.rs`
- [ ] Export new modules from `mvm-runtime/src/security/mod.rs`
- [ ] Tests: rate limiter allows/blocks correctly, session expiry triggers recycle, 3 consecutive ping failures triggers kill

## Phase 5: Security Posture Scoring + Immutable Config
**Status: PENDING**

**Goal:** Multi-layer health scoring for VM security config. Config drive integrity verification. CLI surface.

- [ ] Create `mvm-runtime/src/security/posture.rs` — 12 security layers (JailerIsolation, CgroupLimits, SeccompFilter, NetworkIsolation, VsockAuth, EncryptionAtRest, EncryptionInTransit, AuditLogging, SecretManagement, ConfigImmutability, GuestHardening, SupplyChainIntegrity)
- [ ] Implement `PostureCheck { name, score, status, detail }` and overall score calculation
- [ ] Config drive integrity: SHA-256 hash at boot, periodic re-check for tampering
- [ ] `SecurityPolicy` lives on config drive (read-only post-boot)
- [ ] Add `mvm security status` CLI command (or `mvm doctor --security`)
- [ ] Add posture types to `mvm-core/src/security.rs`
- [ ] Tests: posture check scoring, overall calculation, config hash computation

---

## Carried from Sprint 12

- [ ] Structured logs for sync/build (timestamps, phases) with `--json` flag
- [ ] Capture and surface builder VM logs when nix build fails
- [ ] CLI help/examples refreshed for new flags
- [ ] Add one end-to-end test covering: sync → build --flake → run --config

---

## Non-goals (this sprint)

- SafeClaw integration (reference material only)
- mvmd-specific security (coordinator approval flow, fleet-wide session management) — follow-up after mvm-side work lands
- Hardware attestation (TPM2/SEV-SNP/TDX)

## Success criteria

- Vsock protocol supports authenticated frames with Ed25519 signatures
- Command gate blocks known-dangerous patterns, auto-approves in dev mode
- Threat classifier detects credential leaks, destructive commands, Firecracker escape attempts
- Health monitor detects and kills unresponsive VMs
- `mvm security status` outputs a posture score for a running VM
- All existing tests pass, zero clippy warnings
- New test count: 376 + ~40-60 security tests

## Verification

After each phase:
1. `cargo build` — no compile errors
2. `cargo clippy -- -D warnings` — zero warnings
3. `cargo test` — all existing + new tests pass
