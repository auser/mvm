# ADR-059: Host services broker over vsock

- Status: Proposed
- Date: 2026-05-26
- Owner: MVM Project
- Related: ADR-002 (microVM security posture), ADR-041 (signed audited execution plans), ADR-047 (app-deps audit pipeline), ADR-048 (workload secrets), ADR-049 (TLS substitution mechanism), ADR-053 (guest protocol versioning + readiness), ADR-058 (claim 10 — bytes leaving trust boundary), mvmd ADR 0008 (tenant-scoped authz), mvmd ADR 0023 (mvmd as cross-VM delegate, proposed)
- Sequenced by: [Plan 104 — Host Services Broker over vsock](../plans/104-host-services-broker.md)

## Context

Today, anything a microVM needs from the host arrives one of two ways:

1. **Boot-time only.** A read-only ext4 drive mounted at `/mnt/secrets` or `/mnt/config` (`mvmctl up --volume host_dir:/mnt/secrets`). ADR-048 explicitly tags this `unsafe_guest_secret_materialization` and declines to make a non-leakage claim about it.
2. **A small fixed-verb reverse channel.** `HostBoundRequest` on vsock port 53 carries `WakeInstance`, `QueryInstanceStatus`, `QueryHostTime` (`crates/mvm-guest/src/vsock.rs`). Each new verb is a code change to an enum.

There is also a **half-built secrets path**: `ExecutionPlan.secrets: Vec<SecretBinding>` exists in `crates/mvm-plan/src/plan.rs`; `KeystoreReleaser` trait stubs in `crates/mvm-supervisor/src/keystore.rs` return `NotWired` / `NotImplemented`; the `secrets:` field is hardcoded empty in synthesis. ADR-049 has committed to a vsock side-channel for secret substitution as the v1 mechanism — described in prose, stubbed in code.

What is needed is broader than secrets: a **host-side services layer** microVMs call at runtime — secrets today, then cost / time / logging / audit / monitoring as the catalog grows — with one auth model, one capability model, one audit chain, and one extension point that supports built-in *and* addon-provided services without protocol churn.

## Decision

The per-VM supervisor exposes a **host services broker** that microVMs reach over vsock. v1 ships three services:

- **`host.secrets.v1`** runs in a dedicated subprocess (`mvm-secrets-dispatcher` binary, uid 902, seccomp `standard`, setpriv `--bounding-set=-all --no-new-privs`). This is production-ready process-level isolation. Industry analogues: AWS STS, HashiCorp Vault, Kubernetes ServiceAccount token controllers — all out-of-process credential issuers.
- **`host.time.v1`** and **`host.cost.v1`** run in the in-process general broker inside the supervisor.
- **`broker.v1/list_services`** lets workloads enumerate bound services + verbs + deprecation flags at runtime.

The wire format is JSON via `serde_json`. Signed payloads use JCS (RFC 8785) for canonical bytes. Two vsock ports per VM: port 5300 for the general broker, port 5301 for the secrets dispatcher. Both use the existing `AuthenticatedFrame` (Ed25519 + session id + monotonic sequence) from day one.

Cross-VM data delegates to mvmd over its existing iroh ALPN transport (new `AgentRequest` enum variants in `crates/mvmd-agent/src/transport.rs`). The supervisor never assembles cross-tenant data itself; mvmd's tenant-scoped-authz is the authority.

The out-of-process handler substrate ships in v1 with `host.secrets.v1` as its first consumer. v2 third-party addons reuse the same substrate without protocol change.

## Architecture

### Wire shape

Two vsock listeners per VM. Both ports use 4-byte big-endian length prefix wrapped in `AuthenticatedFrame`.

```rust
#[serde(deny_unknown_fields)]
pub struct ServiceCall {
    pub service: ServiceId,
    pub verb: String,
    pub correlation_id: Ulid,
    pub payload: serde_json::Value,
}

#[serde(deny_unknown_fields)]
pub enum ServiceResponse {
    Ok { correlation_id: Ulid, payload: serde_json::Value },
    Err { correlation_id: Ulid, code: ServiceErrorCode, message: String },
}
```

`ServiceId` is reverse-DNS with explicit version segment: `host.secrets.v1`, `host.time.v1`, `host.cost.v1`. v2 services ship alongside v1 on different IDs — no silent upgrades.

### Two-process host-side architecture

**General broker** (in-process inside `mvm-supervisor`, listens on vsock 5300):

- New module `crates/mvm-supervisor/src/services/` with `broker.rs`, `registry.rs`, `handler.rs`, `host_time.rs`, `host_cost.rs`, `mvmd_client.rs`, `circuit_breaker.rs`, `quota.rs`, `secrets_proxy.rs`.
- Dispatches `HandlerRef::InProcess(Arc<dyn ServiceHandler>)` directly; forwards `HandlerRef::OutOfProcess(UdsProxy)` to the secrets subprocess.

**Secrets subprocess** (`mvm-secrets-dispatcher` binary, NEW crate, listens on vsock 5301):

- New crate `crates/mvm-secrets-dispatcher/`. Binary spawned per VM by the supervisor at admission time.
- Runs under uid 902 + seccomp `standard` + setpriv. Cannot register handlers at runtime; hosts only `host.secrets.v1`.
- Reads its config from the supervisor's stdin once at startup (host signer *public* key path, audit back-channel UDS path, agent profile, allowed bindings from `ExecutionPlan.services`), then closes stdin.
- Audit subentries flow back to the supervisor over a separate UDS for chain-signing — the subprocess never holds the signing key.
- Dies when the supervisor dies (`PR_SET_PDEATHSIG(SIGTERM)` on Linux; kqueue-monitored parent-pid watch on macOS).
- Restart-on-crash with exponential backoff (100ms, 500ms, 2s). After 3 restarts within a workload lifetime, the supervisor stops restarting, audits `secrets.subprocess.crashed_repeatedly`, and triggers a workload pause via the Plan 82 harness. The workload sees `Err(Unavailable)` for `host.secrets.v1` calls until operator review.

**Why two processes is mandatory, not optional.** The secrets subprocess's address space is fully isolated from the general broker's. A use-after-free, integer overflow, or logic bug in the general broker's schema/auth/binding/quota code cannot reach the credential-minting code, the keystore policy state, or the in-flight grant table.

### ExecutionPlan schema change

```rust
#[serde(default, deny_unknown_fields)]
pub services: Vec<ServiceBinding>,

pub struct ServiceBinding {
    pub service: ServiceId,
    #[serde(default)]
    pub policy: ServicePolicy,
    #[serde(default)]
    pub quotas: ServiceQuotas,
}
```

`SCHEMA_VERSION` bumps 4→5. Existing v4 plans hard-fail at verification — no shim, no backcompat (consistent with the project's no-backcompat-first-version rule). Existing `secrets: Vec<SecretBinding>` stays as the policy blob for `host.secrets.v1`.

### Capability gating — five sequential rules

Before any handler dispatch, a call traverses five rules **in order**. They are sequential, not isolated within a single process: for the general broker, all five run in the same supervisor task, in the same address space, sharing the same `serde_json` parser. **Process-level isolation only exists for `host.secrets.v1` calls**, which cross the UDS boundary to the secrets subprocess (gate 5 runs there in a separate address space; gates 1–4 still run in the supervisor before forwarding).

1. **Schema gate.** `serde_json` parse of the envelope with `deny_unknown_fields`; 64 KiB max frame size enforced before parse; recursion cap 8; 50ms parse timeout. Note: `deny_unknown_fields` on the envelope does not cover the dynamically-typed `payload: serde_json::Value`; the typed second-stage parse via `ServiceHandler::parse_payload` (gate 5 prerequisite) is the real payload schema gate.
2. **Authentication gate.** `AuthenticatedFrame` Ed25519 verify against the workload session key (minted at plan admission, discarded at workload stop). Monotonic-sequence replay rejection.
3. **Binding gate.** Workload's `ExecutionPlan.services` must bind this `ServiceId`. Bindings cannot be added at runtime.
4. **Profile + rate-limit + quota gate.** `AgentProfile` check; token-bucket per `(workload_id, service_id)`; in-flight cap; lifetime quota.
5. **Handler-specific policy.** Per-handler `parse_payload` with typed `deny_unknown_fields` (the real schema gate); destination-URL match for `host.secrets.v1`; mvmd tenant-scoped-authz (ADR 0008) for cross-VM verbs.

### Audit chain

Extend `EventCategory` in `crates/mvm-supervisor/src/audit_recorder.rs` with one new variant `ServiceCall`. Every dispatch — allowed or denied — emits one entry: `(service, verb, outcome, correlation_id)`. **Payload content is never logged** (ADR-053 §4 redaction invariant); per-handler audit subentries take typed `AuditFields` (no `String` payload param).

Three contracts on the chain entry format (load-bearing for the future host-logging follow-up plan's mvmd-agent sync mechanism):

1. **Append-only with stable canonical byte serialization** — entries are length-prefixed JSON canonicalized via JCS (RFC 8785) so a sync agent can hash entry bytes + `prev_hash` without re-serializing.
2. **Self-contained per entry** — each entry carries `(prev_hash, ts, category, fields, sig)` with no out-of-band state needed to verify.
3. **`chain_head` exposed** — the supervisor exposes the latest entry's hash via `AuditRecorder::current_head() -> Hash` so a future sync agent can poll/push.

### Cross-VM delegation via mvmd

Cross-VM concerns (tenant-aggregated cost, peer discovery, tenant config) belong in mvmd (per CLAUDE.md: "mvmd owns tenant isolation; mvmctl never reaches across workloads"). `MvmdClient` trait in `crates/mvm-supervisor/src/services/mvmd_client.rs`; real impl uses **mvmd-agent's iroh ALPN transport** with new typed `AgentRequest` variants — NOT raw QUIC+mTLS, NOT new HTTP routes the agent proxies. mvmd Plan 52 and mvmd ADR 0023 sequence the mvmd side.

This is an **architectural boundary, not a trust boundary** — see mvmd ADR 0023 for the full elaboration. A compromised supervisor forges arbitrary workload-ids; mvmd accepts them. The "blast radius stays single-tenant" property only holds under the uncompromised-supervisor assumption, which is itself in scope under ADR-002.

### Built-in handler split

- **No mvmd dep:** `host.time.v1`, `host.secrets.v1`, `host.cost.v1::workload` verb.
- **Mvmd-delegated:** `host.cost.v1::tenant` verb, `host.peers.v1` (future), `host.config.v1` (future).

### Extensibility surface (seven axes — full detail in Plan 104 §Extensibility design)

A1 versioned ServiceIds with parallel versions + deprecation; A2 Cargo feature flags per built-in service; A3 typed `ServicePolicy` per handler; A4 out-of-process handler substrate (v1 ships it; secrets dispatcher is the first consumer); A5 service composition with depth cap 3; A6 version negotiation at plan admission; A7 per-tenant catalogs via mvmd Plan 52.

## Security model

### New claims

ADR-002's live list runs through Claim 11. This ADR adds two new claims:

| # | Claim | Primary layer | Workstream | CI gate |
|---|---|---|---|---|
| 12 | Every host-side service the broker exposes is bound to a signed `ExecutionPlan.services` binding, enforced before handler dispatch, and audited via the chain-signed log | cross-cutting (policy + audit) | Plan 104 W2 | `service_call_denied_when_unbound` + `audit_chain_contains_service_call_entries` tests; `xtask check-handler-adr-coverage` lint |
| 13 | No raw secret value crosses the broker channel; `host.secrets.v1` returns destination-bound, time-bound signed credentials only. Raw secret bytes never leave the supervisor's address space | cross-cutting (data containment) | Plan 104 W5 | `host_secrets_v1_denied_outside_allowed_destinations` + `zeroize_drop_zeros_secret_bytes` + `host_secrets_v1_signed_payload_jcs_roundtrip` + ADR-049 hostile-guest matrix |

Claim 12 is the binding-gated dispatch invariant. A tampered binding fails plan verification under Claim 8; an unbound call is refused with an audited deny.

Claim 13 is the secret-value-never-leaves invariant. `host.secrets.v1` returns destination-bound signed credentials (per ADR-049); raw secrets stay in the supervisor's keystore. The S25 placeholder-egress backstop in gvproxy/passt (Plan 104 W6 / W7) is a defense-in-depth net against SDK-bypass attacks at the L4/L7 boundary.

### Threat model

The broker is a new attack surface. Threats and mitigations (numbered per Plan 104 §Security S1–S28; only load-bearing ones repeated here):

- **S1 — JSON parser as TCB code.** `serde_json` (already in-tree, well-fuzzed). Knobs: `BROKER_MAX_FRAME_BYTES=65536`, `BROKER_MAX_DEPTH=8`, `BROKER_PARSE_TIMEOUT_MS=50`. The secrets subprocess uses its own `serde_json` instance — a parser bug exploited in the general broker does not pivot to the secrets subprocess's memory.
- **S5 — Supervisor blast radius, secrets isolated by process boundary.** The general broker (port 5300) runs in the supervisor; `host.secrets.v1` runs in the secrets subprocess with no shared address space. Subprocess crashes don't kill the supervisor; the supervisor returns `Err(Unavailable)` and restarts the subprocess (max 3 times per workload, then workload pause).
- **S10 — Out-of-process handler TCB scope.** v1 ships the substrate with `mvm-secrets-dispatcher` as its first consumer. The substrate code lives in the supervisor TCB; the dispatcher binary is a new line of TCB code — minimal, single-responsibility, dedicated security review per the no-`do_exec` discipline. v2 third-party addons reuse the same subprocess pattern.
- **S14 — Inter-call memory hygiene.** Handlers must not leak material from call N to call N+1. `zeroize::Zeroizing<…>` wrappers on any handler-internal cache; CI lint `xtask check-no-mutable-handler-state` scans handler modules for `Mutex<T> where T: !Zeroize`.
- **S22 — Audit batch durability (BLOCKING).** Batched fsync is fine; batched *enqueue* is not. The `Recorder` API takes the entry synchronously before `dispatch` returns; only the fsync is batched. Test: `audit_entry_enqueued_before_response_returned`.
- **S23 — Tenant catalog must be mvmd-signed (BLOCKING).** A compromised mvmd-agent or MITM in the iroh transport could inject a wider catalog than the tenant is entitled to. Mitigation: catalog response carries an mvmd-fleet-credential-signed envelope; the supervisor verifies against a pinned mvmd public key before trusting the payload.
- **S24 — Privileged composition can leak secrets (BLOCKING).** A handler composing `host.secrets.v1` via `ServiceCallContext::invoke` could inadvertently include the composed credential in its own outbound response. Mitigation: `xtask check-handler-composition` lint fails the build on any handler that calls `ctx.invoke("host.secrets.v1", …)` and embeds the result in its response payload. Allowlist via `#[allow(secret_passthrough)]` with mandatory review.
- **S25 — SDK integrity / placeholder egress backstop (BLOCKING).** The host-side egress proxy (gvproxy/passt) detects `mvm-secret://` token patterns in outbound HTTP bytes and drops the frame, emitting `secret.substitute.bypass_detected`. Belt-and-suspenders against a malicious deps-volume substitute SDK; Claim 11 (signed deps volume) is the primary defense.
- **S26 — First-call cold-cache timing oracle on `host.secrets.v1`.** Response latency padded to a fixed floor (default 5ms) regardless of cache state. Knob: `BROKER_SECRETS_LATENCY_FLOOR_MS=5`.
- **S28 — JSON canonical encoding for signed credential payloads.** Signed credentials use JCS (RFC 8785) for bytes-to-sign — sorted keys, no whitespace, defined number serialization, NFC Unicode.

### Surfaces that do not expand

- No new host process or persistent socket on disk in v1 beyond the per-VM secrets dispatcher subprocess and its two UDS endpoints (mode 0600, supervisor-owned). v2 third-party addons add per-addon UDS in a separate plan.
- Trust boundary unchanged from ADR-002 — supervisor was already trusted.
- Egress policy unchanged — broker is host↔guest only.
- `prod-agent-no-exec` (ADR-002 Claim 4) unchanged — no broker verb is code-execution-shaped.

## Alternatives considered

**(A) Stay with the fixed-verb `HostBoundRequest` enum.** Rejected: every new service is a code change to a guest-side enum; no auth model beyond the proxy socket; no audit; no per-workload binding. Doesn't scale to secrets + cost + logging + future telemetry.

**(B) ADR-049's TLS-terminating proxy with injected CA.** Considered as the secret-substitution alternative. The cost is significant: it **expands the host's trust boundary into the guest's trust store** — a CA the host controls is now trusted by the guest's TLS stack for *all* outbound connections, not just secret-bearing ones. (B) ships separately as the `unsafe_guest_tls_inspection` opt-in for workloads that can't be modified (vendored binaries, third-party agents).

**(B′) Vsock substitution via SDK hook — the default chosen here.** SDK hooks the HTTP client *before* TLS, asks the host for a destination-bound signed credential, injects it into the outbound request, and the guest does its own TLS to upstream. The guest's trust store is untouched. Protocol-agnostic (HTTP/1.1, HTTP/2, HTTP/3, gRPC, mTLS). Cost: per-language hook matrix (Plan 104 W7).

Plan 104 takes the strongest property of each: (B′) is the primary path; S25 adds the network-layer enforcement property from (B) as a fallback.

**(C) CBOR wire format with COSE signing.** Considered for v1. Switched to JSON via `serde_json` (Plan 104 T3 decision): no genuinely binary payload in v1; SDK matrix friction in Python/TS/Rust CBOR libraries is real; `jq` debuggability over project lifetime matters; consistency with existing `GuestRequest` / `HostBoundRequest` JSON channels. Future binary payloads use base64-in-JSON on the specific field. ADR-049's signing scheme is Ed25519-on-bytes (not COSE), so JSON-with-JCS is appropriate.

**(D) Single-process broker with all services (including secrets) in-process inside the supervisor.** Considered, rejected (Plan 104 T4 decision). Process-level isolation is the production-ready pattern for credential issuers (AWS STS, Vault, K8s SA token controllers — all out-of-process). The split-task→split-process migration later would be more painful under the no-backcompat rule. Cost: ~50% W1+W5 scope growth — a new crate, subprocess lifecycle, UDS proxy code path. Justified by (a) user-stated concern that control-plane compromise is a security risk and (b) the substrate then has a concrete v1 consumer (kills the "speculative substrate" criticism — see T5).

**(E) Defer the out-of-process handler substrate to v2 when third-party addons need it.** Considered, rejected (Plan 104 T5 decision). Coupling the substrate's first consumer (secrets dispatcher) to v1 means every line of the UDS proxy code is exercised by the security-critical secrets path on every workload start. The substrate's design is informed by real requirements, not hypothetical addon needs. v2 third-party addons reuse the substrate when they land.

## Consequences

### Positive

- **One auth model, one capability model, one audit chain** for every host-side service the workload calls — replacing the ad-hoc `HostBoundRequest` enum + the half-built `KeystoreReleaser` stubs.
- **Production-ready isolation for credential issuance** via the subprocess pattern. A logic bug in the general broker's TCB code cannot pivot into the secrets subprocess's memory.
- **Substrate proven by use, not speculation.** The out-of-process handler path is exercised by the secrets dispatcher from day one; v2 addons reuse it without protocol change.
- **Extensibility without protocol churn.** Adding a new built-in service is a single handler file + one registry line + a `ServiceBinding` entry (Plan 104 §"Manual falsifiability check"); the envelope, registry, and auth path do not change.

### Negative

- **One new line of TCB code** (the `mvm-secrets-dispatcher` binary). Mitigated by minimal single-responsibility design and dedicated security review per the no-`do_exec` discipline.
- **`SCHEMA_VERSION` bump 4→5.** Existing v4 plans hard-fail at verification; per the no-backcompat rule, no shim. Migration: re-synthesize + re-sign under v5.
- **Cross-VM calls have higher latency than in-supervisor calls** (sub-100ms target with pre-warmed iroh + agent-local TTL cache). Acceptable for the cost / catalog / future config use cases; not a fit for hot-path queries.
- **Per-backend listener work is non-trivial on vz.** The existing Swift `VsockProxy` is host-as-client only; vz needs a new `VZVirtioSocketListener` class. Substantial sub-task in Plan 104 W1.

## Migration

No backwards-compatibility path is shipped. v4 `ExecutionPlan` instances hard-fail at verification under v5. `KeystoreReleaser` / `NoopKeystoreReleaser` / `LiveKeystoreReleaser` stubs are deleted in Plan 104 W5. `HostBoundRequest::QueryHostTime` is deleted in Plan 104 W3; the only internal caller is migrated to the broker in the same commit. ADR-049's prose is updated in W5 with a one-line "Implementation: lands as `host.secrets.v1` in the broker (ADR-059, Plan 104)" — no semantic change to ADR-049.

## Out of scope

- Streaming responses (monitoring, log tail). Envelope is request/response only in v1.
- Addon-provided handlers shipping in v1. v1 ships only the substrate (the addon-proxy path is implemented and exercised by the secrets dispatcher; no third-party addons are consumed).
- `unsafe_guest_tls_inspection` proxy-with-CA path from ADR-049 — separate plan.
- Non-HTTP secret substitution — out of scope per ADR-049 §"Non-HTTP egress."
- Cross-VM cost aggregation across tenants — `host.cost.v1::tenant` is single-tenant.
- Hardware enclave integration for `host.secrets.v1` signing key (Apple Secure Enclave, TPM) — future hardening ADR.
- Runtime-mutable bindings (supplemental signatures) — future plan if demand emerges. Per Plan 104 C5, plans are immutable post-admission; a binding change requires workload restart.
- Audit chain rotation policy — deferred to the host-logging follow-up plan (number TBD) when `host.audit.v1` lands and workloads can write to the chain.
