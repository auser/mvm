# Plan 104 — Host Services Broker over vsock

Companion ADR: `specs/adrs/059-host-services-broker.md`
Follow-up plan (sketched at end): host-logging + workload-audit — plan number TBD (Plan 103 is contested by an egress-secret-detection proposal; re-verify before claiming a number)
Cross-repo mvmd dependency: `../mvmd/specs/plans/51-host-services-cross-vm-endpoints.md` + `../mvmd/specs/adrs/0022-mvmd-host-services-delegation.md`
Tracking sprint: **Sprint 57** in `specs/SPRINT.md` (Sprint 56 = symmetric trust boundary + claim 10).

> **Plan-numbering note (2026-05-26).** This plan was first drafted as Plan 97 / ADR-056, then 98 / 057, then Plan 98 / ADR-059, and finally **Plan 104 / ADR-059** because `98-vz-builder-vm.md` and `103-w6a-implementation-tracker.md` landed on `origin/main` mid-conversation (taking Plan 98 and Plan 103 respectively). Per the saved "spec numbering chaos" guidance, re-verify ALL numbers with `gh pr list` + `git fetch && git log --diff-filter=A -- specs/plans/104-*.md` *immediately* before opening the implementation PR. **Do not propose explicit Claim 10/11 numbers in ADR-059** — Sprint 56 holds Claim 10 already. Let ADR-059 assign claim numbers against ADR-002's live list at write time.

## Context

Today, anything a microVM needs from the host arrives one of two ways:

1. **Boot-time only** — read-only ext4 drive mounted at `/mnt/secrets` or `/mnt/config` (`mvmctl up --volume host_dir:/mnt/secrets`). ADR-048 explicitly tags this `unsafe_guest_secret_materialization` and declines to make a non-leakage claim about it.
2. **A small fixed-verb reverse channel** — `HostBoundRequest` on vsock port 53 carries `WakeInstance`, `QueryInstanceStatus`, `QueryHostTime` (`crates/mvm-guest/src/vsock.rs`). Each new verb is a code change to an enum.

There's also a **half-built secrets path**: `ExecutionPlan.secrets: Vec<SecretBinding>` exists in `crates/mvm-plan/src/plan.rs:96`; `KeystoreReleaser` trait stubs in `crates/mvm-supervisor/src/keystore.rs` return `NotWired` / `NotImplemented`; the `secrets:` field is hardcoded empty in synthesis (`plan_builder.rs:216`). **ADR-049** has committed to a vsock side-channel for secret substitution as the v1 mechanism — described in prose, stubbed in code.

What's needed is broader than secrets: a **host-side services layer** microVMs call at runtime — secrets today, then cost / time / logging / audit / monitoring as the catalog grows — with one auth model, one capability model, one audit chain, and **one extension point that supports built-in *and* addon-provided services** without protocol churn.

This plan generalizes ADR-049's "vsock substitution service" into a **services broker** hosting `host.secrets.v1` as its first (and most security-critical) tenant. The broker pattern is the right substrate; secrets is the right forcing function.

Intended outcome: after this plan lands, ADR-049's substitution service exists as `host.secrets.v1`, `HostBoundRequest::QueryHostTime` is gone (replaced by `host.time.v1`), the supervisor exposes a registry that new services plug into without protocol churn, and the workload's permission to call a given service is declared in the signed `ExecutionPlan` and enforced by the supervisor before the handler ever runs.

## Architecture

### Wire shape

- **Two vsock ports per VM:**
  - **Port 5300 — general broker channel.** Hosts `host.time.v1`, `host.cost.v1`, `broker.v1` (observational / low-criticality services). Dispatched by an in-process task in the per-VM supervisor.
  - **Port 5301 — secrets channel.** Hosts `host.secrets.v1` only. Dispatched by a dedicated **secrets subprocess** that runs alongside the supervisor (uid 902, seccomp `standard`, no FS/net beyond the per-VM Unix-domain socket back to the supervisor for audit + UDS to listen for forwarded calls). The supervisor's broker is a transparent proxy: gates 1–4 run in the supervisor; gate 5 + handler dispatch happen inside the secrets subprocess.
  - Both ports use 4-byte big-endian length prefix; payload is **JSON** (`serde_json`). Frame is wrapped in `AuthenticatedFrame` (Ed25519 + session id + sequence — already implemented in `crates/mvm-guest/src/vsock.rs`). Authenticated from day one on both ports.
- **Envelope (host-bound):**
  ```rust
  #[serde(deny_unknown_fields)]
  pub struct ServiceCall {
      pub service: ServiceId,
      pub verb: String,
      pub correlation_id: Ulid,
      pub payload: serde_json::Value,
  }
  ```
- **Envelope (guest-bound):**
  ```rust
  #[serde(deny_unknown_fields)]
  pub enum ServiceResponse {
      Ok { correlation_id: Ulid, payload: serde_json::Value },
      Err { correlation_id: Ulid, code: ServiceErrorCode, message: String },
  }
  ```
- `ServiceId` is reverse-DNS-like with a version segment: `host.secrets.v1`, `host.time.v1`, `host.cost.v1`. Versions explicit so v2 can ship alongside v1 without breaking callers.
- **Why JSON not CBOR.** v1 has no genuinely binary payload (signed credentials base64 cleanly into a `credential_b64` field; time/cost are ints; list_services is strings). JSON wins on debuggability (`jq` works), cross-language story (every SDK ecosystem has a robust JSON parser; the W7 ADR-049 hook matrix avoids CBOR-library-quality friction), and consistency with the existing `GuestRequest` / `HostBoundRequest` JSON discipline in `crates/mvm-guest/src/vsock.rs`. If a future service ships actual binary, the field can be base64-in-JSON — 33% overhead on that *one field* at ~500-byte typical payloads is negligible.
- **Why two ports + secrets-in-subprocess (T4 production-ready isolation).** The supervisor's general-broker dispatcher and the secrets dispatcher share zero code paths and zero address space. A logic bug in the schema/dispatcher/quota path of the general broker cannot reach the secrets subprocess's memory or registry. This is the production-ready isolation pattern (industry analogues: AWS STS, HashiCorp Vault, Kubernetes ServiceAccount token controllers — all out-of-process). Same envelope + handler trait + audit chain, different runtime processes.

### Host-side: two-process architecture

The supervisor hosts the **general broker** (in-process); a separate **secrets subprocess** hosts `host.secrets.v1` only. Both run for the lifetime of the guest.

**General broker (in-process, in `mvm-supervisor`):**

- New module `crates/mvm-supervisor/src/services/` with:
  - `broker.rs` — `ServiceBroker { listener: VsockListener, registry: ServiceRegistry, secrets_uds: Option<UnixStream> }`. Listens on port 5300. For in-process handlers, dispatches directly; for `host.secrets.v1` calls (which arrive on port 5301, not 5300 — but the secrets subprocess also goes through the binding-gate check via the supervisor before forwarding), the supervisor proxies.
  - `registry.rs` — `ServiceRegistry { handlers: HashMap<ServiceId, HandlerRef> }` where `HandlerRef::InProcess(Arc<dyn ServiceHandler>)` or `HandlerRef::OutOfProcess(UdsProxy)`. Built at plan-admission time.
  - `handler.rs` — the `ServiceHandler` trait:
    ```rust
    #[async_trait]
    pub trait ServiceHandler: Send + Sync {
        fn id(&self) -> ServiceId;
        fn profiles(&self) -> &[AgentProfile];
        fn audit_durability(&self) -> AuditDurability;  // PerCall | Batched(Duration)
        fn response_size_cap(&self) -> usize;           // default 64 KiB
        fn idempotency(&self) -> Idempotency;           // see §C3
        fn call_timeout(&self) -> Duration;             // see §C4
        async fn dispatch(&self, ctx: &ServiceCallCtx, verb: &str, payload: serde_json::Value)
            -> Result<serde_json::Value, ServiceError>;
    }
    ```
  - `host_time.rs`, `host_cost.rs` — in-process handlers.
  - `mvmd_client.rs` — `MvmdClient` trait for cross-VM data.
  - `circuit_breaker.rs` — per-handler health tracking (see §Security S13).
  - `quota.rs` — token-bucket + lifetime caps (see §Security).
  - `secrets_proxy.rs` — UDS client to the secrets subprocess. Wire format: same `ServiceCall`/`ServiceResponse` envelope. The supervisor performs gates 1–4 *before* forwarding; the secrets subprocess performs gate 5 + dispatch.

**Secrets subprocess (`mvm-secrets-dispatcher` binary, NEW):**

- New crate `crates/mvm-secrets-dispatcher/` producing a binary spawned by the supervisor per VM.
- Binary listens on **vsock port 5301** directly (the supervisor sets up the listener via the backend-specific mechanism — see W1) AND on a per-VM Unix-domain socket at `~/.mvm/vms/<vm>/services/secrets.sock` for supervisor-proxied calls (mode 0600, supervisor-owned).
- Runs under uid 902, seccomp `standard` (W2.4 profile, no `do_exec`), `setpriv --bounding-set=-all --no-new-privs`. Configured via supervisor-set environment + read-only mount of `~/.mvm/keys/host-signer.pub` (verification only — the signing key stays in the supervisor).
- Hosts ONLY `host.secrets.v1`. No other services. Cannot register additional handlers at runtime.
- Audit entries from the secrets subprocess are sent to the supervisor over the UDS for chain-signing — the supervisor is the only writer to `~/.mvm/audit/<tenant>.jsonl`. The subprocess holds no audit key material.
- Dies when the supervisor dies (subprocess inherits supervisor's lifetime via `prctl(PR_SET_PDEATHSIG)` on Linux / equivalent on macOS).

**Why two processes is mandatory, not optional:** the secrets subprocess's address space is fully isolated from the general broker's. A use-after-free, integer overflow, or logic bug in the general broker's schema/auth/binding/quota code cannot reach the credential-minting code, the keystore policy state, or the in-flight grant table. This is the answer to "control plane compromise = security concern."

### ExecutionPlan schema change

```rust
#[serde(default, deny_unknown_fields)]
pub services: Vec<ServiceBinding>,

pub struct ServiceBinding {
    pub service: ServiceId,
    #[serde(default)]
    pub policy: ServicePolicy,      // service-specific opaque JSON
    #[serde(default)]
    pub quotas: ServiceQuotas,      // per-workload-lifetime budgets (see §Security)
}
```

Existing `secrets: Vec<SecretBinding>` stays — it's the **policy blob** for the `host.secrets.v1` binding. The supervisor's broker assembly looks up `host.secrets.v1` and constructs a `HostSecretsV1Handler` parameterized by `plan.secrets`. No duplication.

### Capability gating — five sequential rules

Before any handler dispatch, a call traverses five rules in order. **They are sequential, not isolated within a single process** — for the general broker, all five run in the same supervisor task, in the same address space, parsing with the same `serde_json` parser, against state co-located in one process. **Process-level isolation only exists for `host.secrets.v1` calls**, which cross the UDS boundary to the secrets subprocess (gate 5 runs there in a separate address space; gates 1–4 still run in the supervisor — see §"Host-side: two-process architecture").

1. **Schema gate** — `serde_json` parse of the envelope with `deny_unknown_fields`; 64 KiB max frame size enforced *before* parse; recursion cap 8; 50ms parse timeout. **Note:** `deny_unknown_fields` covers only the outer envelope (`service`, `verb`, `correlation_id`, `payload`) — the `payload` itself is `serde_json::Value`, which is dynamically typed and **does not get `deny_unknown_fields` at envelope-parse time**. The typed second-stage parse via `ServiceHandler::parse_payload` (gate 5 prerequisite) is the *real* payload schema gate. See §S19.
2. **Authentication gate** — `AuthenticatedFrame` Ed25519 verify against workload session key (minted at plan admission, discarded at workload stop). Monotonic-sequence replay rejection.
3. **Binding gate** — workload's `ExecutionPlan.services` must bind this `ServiceId`. Bindings can't be added at runtime.
4. **Profile + rate-limit + quota gate** — `AgentProfile` check; token-bucket per `(workload_id, service_id)`; in-flight cap; lifetime quota (see §Security S12).
5. **Handler-specific policy** — per-handler `parse_payload` with typed `deny_unknown_fields` (the real schema gate); destination-URL match for `host.secrets.v1`; mvmd tenant-scoped-authz (ADR-0008) for cross-VM verbs.

### Audit chain

Extend `EventCategory` in `crates/mvm-supervisor/src/audit_recorder.rs` with one new variant `ServiceCall`. Every dispatch — allowed or denied — emits one entry: `(service, verb, outcome, correlation_id)`. **Payload content is never logged** (ADR-053 §4 redaction invariant); per-handler audit subentries take typed `AuditFields` (no `String` payload param).

### Cross-VM data via mvmd (delegation pattern)

Cross-VM concerns (tenant-aggregated cost, peer discovery, tenant config) belong in **mvmd**, not the per-VM supervisor (CLAUDE.md: "mvmd owns tenant isolation; mvmctl never reaches across workloads").

- `MvmdClient` trait in `crates/mvm-supervisor/src/services/mvmd_client.rs`. Real impl uses **mvmd-agent's iroh ALPN transport** (`crates/mvmd-agent/src/transport.rs` — iroh's QUIC stack multiplexed via ALPN with typed `AgentRequest`/`AgentResponse` enums; **NOT raw QUIC+mTLS** as earlier drafts described). New broker verbs land as additional `AgentRequest` variants, not as HTTP routes the agent proxies. Test-mode in-process impl backs out for unit tests.
- **Cross-tenant authorization happens on mvmd's side** via tenant-scoped-authz (ADR-0008). Broker never assembles cross-tenant data itself.
- Built-in handler split:
  - **No mvmd dep**: `host.time.v1`, `host.secrets.v1`, `host.cost.v1::workload` verb.
  - **Mvmd-delegated**: `host.cost.v1::tenant` verb, `host.peers.v1` (future), `host.config.v1` (future), `host.logging.v1` forwarding (the host-logging follow-up plan).

### Guest side

- New module `crates/mvm-guest/src/services.rs` (broker client). `AuthenticatedFrame` + JSON.
- `GuestCapability::ServicesBroker` advertised in `ProtocolHello`, gated by `AgentProfile`.
- Broker exposes no code-execution verb; `host.secrets.v1` returns signed destination-bound credentials, not raw bytes. ADR-002 claim 4 and ADR-048 secret non-leakage preserved.

### SDK side

- `crates/mvm-sdk/src/services/`: `client.rs`, `host_secrets.rs` (ADR-049 hook-point dispatcher), `host_time.rs`, `host_cost.rs`.
- Python surface: `mvm.services.host_time()`, `mvm.services.host_cost()`. Secrets is invisible (auto-substitution at HTTP middleware level per ADR-049).

## Extensibility design

Extensibility is designed in along seven orthogonal axes:

### A1 — Versioned ServiceIds + parallel versions

- `ServiceId` carries `.v1` suffix. v2 ships alongside v1 on different IDs (`host.secrets.v1` and `host.secrets.v2` coexist in the registry).
- Workloads bind explicitly to a version. No silent upgrades.
- **Deprecation lifecycle:** a service is marked `deprecated_in("2027-Q1")` in its handler metadata. `broker.v1/list_services` surfaces the deprecation flag. At the deprecation date, the handler is hard-removed in a single PR (per saved rule "no backwards compatibility — this is the first version"). The migration window is explicit but not extended via shims.

### A2 — Cargo feature flags per built-in service

- `crates/mvm-supervisor/Cargo.toml` gains features: `service-host-secrets`, `service-host-time`, `service-host-cost`, `service-host-cost-mvmd` (the cross-VM verb), `service-broker-meta` (always on — `broker.v1/list_services`). All on by default.
- Slim builds (e.g., the builder VM init binary) can compile out unused services: `mvm-builder-init` ships with `--no-default-features --features service-host-time` only.
- Each handler module is `#[cfg(feature = "service-…")]`-gated; the static catalog assembles only enabled handlers.

### A3 — `ServicePolicy` as service-defined typed schema

- `ServicePolicy` is `serde_json::Value` in the wire schema, but each handler declares a **typed Rust schema** for its own policy:
  ```rust
  impl ServiceHandler for HostSecretsV1Handler {
      type Policy = SecretsPolicy;
      fn parse_policy(&self, raw: JsonValue) -> Result<Self::Policy, _> { … }
      …
  }
  ```
- Schema lives next to the handler. Adding a new policy field is a single-file change.
- `xtask check-handler-policy-schema` lint enforces every handler ships a typed `Policy` (no opaque-JSON escape hatch in built-ins).
- For v2 out-of-process addons (see A4), the addon manifest declares its policy schema as JSON Schema; the broker validates inbound policy against it at admission.

### A4 — Out-of-process handler substrate (v1 ships it; the secrets subprocess is its first consumer)

The out-of-process handler substrate has a **mandatory v1 consumer**: the secrets dispatcher (see §"Host-side: two-process architecture"). This kills the "speculative substrate, no v1 consumer" criticism — every line of the proxy substrate is exercised by the security-critical secrets path on every workload start.

**Substrate (v1):**
- The supervisor's broker dispatches `HandlerRef::OutOfProcess(UdsProxy)` for any handler registered with an out-of-process reference. The proxy uses the same `ServiceCall`/`ServiceResponse` envelope over a per-VM Unix-domain socket at `~/.mvm/vms/<vm>/services/<service_id>.sock` (mode 0600, supervisor-owned).
- The supervisor performs gates 1–4 *before* forwarding (schema, auth, binding, profile/rate-limit/quota). Forwards only validated calls. The out-of-process handler performs gate 5 + dispatch.
- The handler subprocess runs under seccomp `standard` + setpriv (`--bounding-set=-all --no-new-privs`) + uid 902 (separate from agent uid 901 and supervisor uid 0). It has no FS or net access beyond the supervisor-owned UDS, and a read-only view of the host signer's *public* key for verification.
- Audit entries flow back to the supervisor over the UDS; the supervisor is the sole writer to the chain-signed log. The subprocess holds no audit key material.

**v1 consumer:** `mvm-secrets-dispatcher` binary hosting `host.secrets.v1` only.

**v2 consumer (separate plan):** third-party addon services declare themselves via a manifest at `~/.mvm/services/<service_id>/manifest.toml` carrying `id`, `version`, `binary`, `policy_schema_url`, `audit_subentry_schema_url`, `signature` (signed under existing claim-9 deps-audit pipeline). The supervisor scans `~/.mvm/services/` at admission, verifies signatures, and registers any addons bound by the workload's `ExecutionPlan.services` as `HandlerRef::OutOfProcess`. The wire protocol, audit flow, and lifecycle are *identical* to the v1 secrets dispatcher — when v2 ships, no protocol or substrate change is needed. **The v1 secrets dispatcher serves as the design's existence proof.**

**TCB:** broker stays minimal. Subprocess faults can't crash the supervisor. Subprocess code can't reach the supervisor's memory. Workload trusts the supervisor's signature on the envelope (which the supervisor adds *after* the subprocess returns), not the subprocess's — so a compromised subprocess can return wrong data within its service scope but cannot impersonate other services or forge audit entries.

### A5 — Service composition (services calling services)

- A handler may invoke other handlers via an in-process `ServiceCallContext::invoke(service_id, verb, payload)` — useful for, e.g., `host.config.v1` fetching a credential through `host.secrets.v1`.
- **Recursion cap:** invocation depth limited to 3. Beyond that → `ServiceErrorCode::CompositionDepth`. Audit chain records the call tree.
- **Capability requirement:** the calling handler must declare statically which services it composes (`fn composes_with() -> &[ServiceId]`). The CI lint `xtask check-handler-composition` verifies declared composition matches actual `invoke()` calls in the source. The composing-handler's plan binding doesn't automatically grant the composed service — the workload's plan must bind both.

### A6 — Version negotiation at plan admission

- During `admit_for_run`, the supervisor inspects the workload's `ExecutionPlan.services` binding set and verifies every requested `ServiceId` is in the static catalog (or, in v2, has a discovered addon manifest). Missing services → admission refused with a structured error listing the unsupported IDs.
- The SDK's compile-time decorator (`@mvm.bind("host.cost.v1")` or equivalent) inserts the binding into the workload's IR; if the host catalog doesn't support that ID, `mvmctl up` fails fast at admission rather than at first call.

### A7 — Per-tenant service catalogs (mvmd)

- mvmd Plan 51 exposes `GET /v1/host-services/tenant/{tenant_id}/catalog` returning the set of services this tenant is allowed to use. Different tenants can have different catalogs (a Free tenant might not see `host.cost.v1::tenant`; an Enterprise tenant might see `host.config.v1`).
- At workload admission, the per-VM supervisor pulls the tenant catalog from mvmd-agent, intersects it with the workload's `ExecutionPlan.services` bindings, and refuses any binding not in the tenant's catalog. The intersection is signed and recorded in the audit chain at admission.

## Build sequence

Each wave is independently mergeable and leaves `cargo test --workspace && cargo clippy -- -D warnings` green. **Check off as completed.**

- [ ] **W1 — Broker substrate + secrets-subprocess scaffolding** *(no mvmd dep)*
  - [ ] `ServiceCall` / `ServiceResponse` envelope types in `crates/mvm-supervisor/src/services/broker.rs` (JSON via `serde_json`)
  - [ ] `ServiceId`, `ServiceErrorCode`, `ServiceCallCtx` newtypes
  - [ ] `ServiceHandler` trait + `ServiceRegistry` with `HandlerRef::{InProcess, OutOfProcess}` discriminator
  - [ ] **Two vsock listeners per VM (per backend; NOT uniform "wire it in"):**
    - [ ] **libkrun** — bind ports 5300 + 5301 via `add_vsock_port2(port, host_path, listen=true)` in `crates/mvm-libkrun/src/sys.rs`; general-broker task reads 5300, secrets subprocess reads 5301
    - [ ] **Firecracker** — bind via the in-process `Supervisor` struct in `crates/mvm-supervisor/src/supervisor.rs` (no separate per-VM binary)
    - [ ] **Apple Container** — host-as-listener path parallel to libkrun's, both ports
    - [ ] **vz (Apple Silicon)** — *new Swift work:* `VZVirtioSocketListener` class added to `crates/mvm-vz-supervisor/Sources/mvm-vz-supervisor/` with `shouldAcceptNewConnection` plumbing, relays accepted sockets for BOTH ports to their respective listeners via host-side UDS. **The Swift supervisor has no listener path today** (`VsockProxy.swift` is host-as-client only). Substantial sub-task.
  - [ ] **`crates/mvm-secrets-dispatcher/` NEW crate** — binary spawned by supervisor per VM; listens on per-VM UDS `~/.mvm/vms/<vm>/services/secrets.sock` (mode 0600) for supervisor-proxied calls, and on vsock 5301 via the backend-set listener for direct guest calls. Uid 902, seccomp `standard`, `setpriv --bounding-set=-all --no-new-privs`. Read-only access to host signer *public* key only.
  - [ ] Supervisor subprocess lifecycle: spawn at VM admission, attach via `PR_SET_PDEATHSIG` (Linux) / equivalent on macOS so subprocess dies with supervisor. Restart-on-crash with backoff (max 3 restarts per workload lifetime; beyond → audit `secrets.subprocess.crashed_repeatedly` and workload pause).
  - [ ] `crates/mvm-supervisor/src/services/secrets_proxy.rs` — UDS client that forwards `ServiceCall` to the subprocess after gates 1–4
  - [ ] Both brokers reject every call with `NotBound` (no handlers registered yet; the secrets subprocess ships its stub-handler scaffolding in W1)
  - [ ] Service composition `ServiceCallContext::invoke` API with depth cap (composition crosses the process boundary if the composed service is out-of-process; tested via stub handler)
  - [ ] Cargo feature flags: `service-host-secrets`, `service-host-time`, `service-host-cost`, `service-host-cost-mvmd`, `service-broker-meta`
  - [ ] Tests: envelope serde roundtrip (JSON), `deny_unknown_fields` on envelope rejection, `AuthenticatedFrame` happy path, replay rejection, length-prefix tampering rejection, frame > 64 KiB rejection, recursion cap rejection, parse timeout, composition depth cap (including cross-process), **subprocess crash isolation (kill subprocess mid-call; supervisor survives; workload sees `Err(Unavailable)`)**, **supervisor crash teardown (kill supervisor; subprocess exits cleanly via pdeathsig)**, cross-backend listener attaches and accepts on all four backends, **both ports**
- [ ] **W2 — ExecutionPlan + admission wiring** *(no mvmd dep)*
  - [ ] Add `services: Vec<ServiceBinding>` + `ServiceBinding` + `ServicePolicy` + `ServiceQuotas` to `crates/mvm-plan/src/plan.rs`
  - [ ] Update synthesis (`plan_builder.rs:216`)
  - [ ] Extend `admit_for_run` to assemble per-VM `ServiceRegistry`; refuse admission for unsupported service IDs (clear error listing them)
  - [ ] Per-handler typed `Policy` parsing via the `parse_policy` trait method
  - [ ] Add `EventCategory::ServiceCall` to `audit_recorder.rs`
  - [ ] Token-bucket rate-limit + lifetime-quota + in-flight cap implementation in `quota.rs`
  - [ ] Circuit breaker in `circuit_breaker.rs`
  - [ ] Tests: plan synthesis + verification with bindings, audit-chain entry shape, unknown-binding rejection at admission, lifetime quota exhaustion, circuit breaker opens after N failures, in-flight cap enforced, bootstrap-order (workload calls broker before broker ready) returns `NotReady`
- [ ] **W3 — `host.time.v1`** *(no mvmd dep)*
  - [ ] Implement `HostTimeV1Handler`
  - [ ] Add `broker.v1/list_services` introspection verb (returns bound services + verbs + deprecation flags)
  - [ ] **Delete** `HostBoundRequest::QueryHostTime` and its host-side dispatch (no shim)
  - [ ] Update the only internal caller of `QueryHostTime` to use the broker
  - [ ] Tests: handler returns sane wall+monotonic, profile/binding gates, rate-limit triggers, list_services returns bound set + deprecation flags
- [ ] **W4a — `host.cost.v1` (workload-scope only)** *(no mvmd dep; ⚠️ hidden dependency on building cost accumulators)*
  - [ ] **Prerequisite: build per-workload cost accumulators in `crates/mvm-supervisor/src/cost.rs`** — they do NOT exist today (verified 2026-05-26: no `cost.rs`, no accumulator type in `crates/mvm-supervisor/src/`). Schema: `WorkloadCostAccumulator { cpu_seconds: f64, ram_mb_seconds: f64, vsock_bytes_in: u64, vsock_bytes_out: u64, ts_start: SystemTime }`. Updated by the supervisor's existing per-VM hooks (cgroup poll for CPU/RAM; the broker itself for vsock).
  - [ ] Implement `HostCostV1Handler` reading from those accumulators
  - [ ] Verb `workload`; verb `tenant` returns `NotImplemented` until W4b
  - [ ] Tests: returns right shape, quota enforcement, denied in `BuilderOnly` profile, accumulator updates correctly under simulated CPU/RAM load
  - [ ] **Alternative if W4a's accumulator scope is too big:** defer the workload verb to W4b alongside cross-tenant and ship W4a as `NotImplemented` for *both* verbs. Decision: leave to implementer based on scope estimate at W4a start.
- [ ] **W4b — `host.cost.v1` cross-tenant via mvmd** *(depends on mvmd Plan 51 W1+W2+W3)*
  - [ ] Add `MvmdClient` trait + real impl over mvmd-agent's iroh ALPN transport (new `AgentRequest` variants — NOT a new HTTP route, NOT raw QUIC+mTLS)
  - [ ] Test-mode in-process `MvmdClient`
  - [ ] mvmd response schema validation (refuse if mvmd returns unexpected fields/types — see §Security S15)
  - [ ] Implement `host.cost.v1::tenant` verb
  - [ ] Per-tenant catalog intersection at admission (§A7)
  - [ ] Tests: positive aggregation against mock client, cross-tenant authz denial, mvmd-unavailable → `ServiceErrorCode::Unavailable` (no stale data), forged workload-id rejected, malformed mvmd response rejected, tenant catalog intersection refuses out-of-catalog binding
- [ ] **W5 — `host.secrets.v1`** *(no mvmd dep; ADR-049 implementation; runs inside the secrets subprocess)*
  - [ ] Implement `HostSecretsV1Handler` per ADR-049 §"Substitution flow" — **inside `crates/mvm-secrets-dispatcher/`, NOT in mvm-supervisor**
  - [ ] Wire the supervisor's `secrets_proxy.rs` to forward gates-1-4-passed calls to the subprocess; subprocess does gate 5 + dispatch
  - [ ] Audit subentries flow from subprocess back to supervisor over UDS; supervisor chain-signs and appends
  - [ ] Destination-URL match against `allowed_destinations`
  - [ ] Signed-credential generation
  - [ ] `audit_durability() = PerCall`
  - [ ] **Delete** `KeystoreReleaser`, `NoopKeystoreReleaser`, `LiveKeystoreReleaser` stubs (no shim)
  - [ ] Plumb existing `ExecutionPlan.secrets` field as handler's policy blob
  - [ ] `zeroize::Zeroize` impl on secret-bearing payload types
  - [ ] Inter-call memory-state hygiene (no leak from call N to call N+1)
  - [ ] Tests: positive substitution, destination-deny, expired grant, unknown grant, replay, audit-subentry shape, ADR-049 hostile-guest matrix (raw socket bypass, substitution replay, library bypass), inter-call state hygiene
- [ ] **W6 — Fuzz + CI** *(no mvmd dep)*
  - [ ] Add `crates/mvm-guest/fuzz/fuzz_service_call.rs` per ADR-002 §W4.2
  - [ ] Wire into existing `cargo-fuzz` lane (≥5min/PR)
  - [ ] Add `xtask check-handler-adr-coverage` lint
  - [ ] Add `xtask check-handler-policy-schema` lint
  - [ ] Add `xtask check-handler-composition` lint
  - [ ] Add `xtask check-no-mutable-handler-state` lint (see §Security S14)
  - [ ] Confirm `prod-agent-no-exec` still passes
  - [ ] Cross-backend test matrix: broker handshake + at least one handler call on each of libkrun / Firecracker / Apple Container / vz
- [ ] **W7 — ADR-049 §W3 SDK matrix** *(no mvmd dep; can split per language)*
  - [ ] Python: `requests.Session.send`, `httpx.Client.send`/`AsyncClient.send`, `aiohttp.ClientSession._request`, `urllib3`
  - [ ] TypeScript: `fetch` polyfill, `axios.interceptors.request`, `node:http(s).request` patch
  - [ ] Rust: `reqwest_middleware::Middleware`, `tower::Layer` for hyper, `tonic::Interceptor`
  - [ ] `register_substitution_handler` extension point in all three
  - [ ] Built-in `aws` credential adapter (SigV4) in all three
  - [ ] Deterministic S3 `ListBuckets` SigV4 tests proving placeholders resolve before signing
  - [ ] ADR-049's hostile-guest tests in all three SDKs

## Critical files to create or modify

**New:**
- `crates/mvm-supervisor/src/services/{mod.rs, broker.rs, registry.rs, handler.rs, host_time.rs, host_cost.rs, mvmd_client.rs, circuit_breaker.rs, quota.rs, secrets_proxy.rs}` (note: `host_secrets.rs` moves to the dispatcher crate; the supervisor only holds the UDS proxy)
- `crates/mvm-secrets-dispatcher/` — **NEW crate**, binary `mvm-secrets-dispatcher`, hosts `HostSecretsV1Handler`, listens on vsock 5301 + per-VM UDS, runs under uid 902 + seccomp + setpriv
- `crates/mvm-guest/src/services.rs`
- `crates/mvm-sdk/src/services/{mod.rs, client.rs, host_secrets.rs, host_time.rs, host_cost.rs}`
- `crates/mvm-guest/fuzz/fuzz_service_call.rs`
- `crates/mvm-cli/src/commands/services.rs` (see §C2)
- `specs/adrs/059-host-services-broker.md`

**Modify:**
- `crates/mvm-plan/src/plan.rs` — `services`, `ServiceBinding`, `ServicePolicy`, `ServiceQuotas` types
- `crates/mvm-plan/src/plan_builder.rs:216` — synthesize binding set instead of `Vec::new()`
- `crates/mvm-supervisor/src/audit_recorder.rs` — add `EventCategory::ServiceCall`
- `crates/mvm-guest/src/vsock.rs` — `GuestCapability::ServicesBroker`, port 5300, `ProtocolHello` capability; **remove** `HostBoundRequest::QueryHostTime` (W3)
- `crates/mvm-supervisor/src/keystore.rs` — **delete** in W5
- `crates/mvm-supervisor/src/lib.rs` — `pub mod services;`
- `crates/mvm-libkrun/src/bin/mvm-libkrun-supervisor.rs` — start broker task (libkrun backend)
- `crates/mvm-supervisor/src/supervisor.rs` and the Firecracker dispatch path in `crates/mvm/src/vm/` — Firecracker uses the in-process `Supervisor` struct (no separate per-VM binary); the broker is a task spawned by that struct's lifecycle
- `crates/mvm-vz-supervisor/Sources/mvm-vz-supervisor/` — **new Swift listener class** (`VZVirtioSocketListener` + `shouldAcceptNewConnection`); host-side UDS relay to the Rust broker task. The existing `VsockProxy.swift` is host-as-client only and doesn't cover this
- Apple Container backend entry — host-as-listener path parallel to libkrun's
- `crates/mvm-supervisor/src/cost.rs` (NEW) — per-workload cost accumulators built in W4a (does not exist today)
- `.github/workflows/ci.yml` — fuzz lane + cross-backend test matrix lane
- `crates/mvm-supervisor/Cargo.toml` — `service-*` feature flags

**Update or supersede:**
- `specs/adrs/049-secret-substitution-mechanism.md` — one-line "Implementation: lands as `host.secrets.v1` in the host services broker (ADR-059, Plan 104)." No semantic change.
- `specs/plans/74-claim-safe-sandbox-parity.md` §W3 — redirect to Plan 104 W5+W7.

### Subprocess lifecycle details (T4 design)

- **Spawn:** at VM admission, the supervisor's `admit_for_run` ceremony spawns `mvm-secrets-dispatcher` via `std::process::Command` with stdin/stdout piped for initial config exchange, then drops the pipes once the subprocess is listening on its UDS.
- **Configuration:** supervisor passes via stdin a small JSON config (host signer public key path, audit-back-channel UDS path, vsock port 5301, agent profile, allowed bindings from the workload's `ExecutionPlan.services`). After consume, stdin closes.
- **Audit back-channel:** subprocess writes audit subentries to a supervisor-owned UDS (separate from the call-forwarding UDS). Supervisor chain-signs and appends to `~/.mvm/audit/<tenant>.jsonl`. Subprocess never holds the signing key.
- **Inheritance:** subprocess's parent is the supervisor; `PR_SET_PDEATHSIG(SIGTERM)` on Linux ensures the subprocess dies if the supervisor dies. macOS equivalent: a kqueue-monitored parent-pid watcher (or libdispatch's `dispatch_source_create(DISPATCH_SOURCE_TYPE_PROC, …)` watching the supervisor's PID).
- **Restart policy:** if the subprocess crashes, supervisor restarts it with exponential backoff (100ms, 500ms, 2s). After 3 restarts within the workload's lifetime, supervisor stops restarting, audits `secrets.subprocess.crashed_repeatedly`, and triggers a workload pause via Plan 82's harness. The workload sees `Err(Unavailable)` for `host.secrets.v1` calls until resumed (which only happens after operator review).
- **In-flight call handling on crash:** outstanding correlation IDs return `Err(Unavailable)`. New session keys minted post-restart; old sessions invalidated (consistent with S8 pause/resume semantics).

## Reuse — existing pieces this plan leans on

- `AuthenticatedFrame` framing in `crates/mvm-guest/src/vsock.rs` — reused unchanged.
- `Recorder` trait + chain-signed JSONL audit — reused; one new `EventCategory` variant.
- `AgentProfile` enum — reused as the first capability gate.
- `ProtocolHello` capability negotiation (ADR-053) — reused.
- Per-VM supervisor process model — reused; broker is a task inside it.
- `ExecutionPlan.secrets: Vec<SecretBinding>` and `SecretReleasePolicy` — reused as policy blob for `host.secrets.v1`.
- Supervisor's per-workload cost accumulators — reused by `host.cost.v1`.

## Verification

**Per-wave smoke:**
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace -- -D warnings`

**End-to-end smoke (after W5):**
- [ ] `cargo run -- up --plan examples/secrets-broker/plan.json` — sealed-prod workload calls `host.secrets.v1`; supervisor audits.
- [ ] `cargo run -- audit verify` — chain integrity green including new `ServiceCall` entries.

**Security regression set (must pass on every PR after the respective wave):**
- [ ] `service_call_denied_when_unbound` (W2)
- [ ] `service_call_denied_outside_profile` (W2)
- [ ] `service_call_replay_rejected` (W1)
- [ ] `service_call_session_after_stop_rejected` (W2)
- [ ] `service_call_rate_limit_enforced` (W2)
- [ ] `service_call_lifetime_quota_exhausted` (W2)
- [ ] `service_call_response_size_cap_enforced` (W2)  ← S11
- [ ] `service_call_amplification_attack_refused` (W2)  ← S11
- [ ] `broker_cpu_budget_enforced` (W2)
- [ ] `broker_memory_cap_enforced` (W2)
- [ ] `broker_queue_full_returns_error` (W1)  ← S21
- [ ] `circuit_breaker_opens_after_failures` (W2)  ← S13
- [ ] `circuit_breaker_half_open_recovery` (W2)  ← S13
- [ ] `handler_panic_does_not_kill_supervisor` (W2)
- [ ] `handler_inter_call_memory_hygiene` (W5)  ← S14
- [ ] `handler_call_timeout_enforced` (W2)  ← C4
- [ ] `handler_idempotency_contract_per_handler` (W3/W4a/W5)  ← C3
- [ ] `broker_pause_resume_invalidates_old_session` (W2, Plan 82 harness)
- [ ] `audit_chain_contains_service_call_entries` (W2)
- [ ] `audit_chain_carries_no_payload_bytes` (W2)
- [ ] `host_secrets_v1_denied_outside_allowed_destinations` (W5)
- [ ] `zeroize_drop_zeros_secret_bytes` (W5)
- [ ] `cross_tenant_query_refused` (W4b)
- [ ] `forged_workload_id_refused` (W4b)
- [ ] `malformed_mvmd_response_rejected` (W4b)  ← S15
- [ ] `confused_deputy_via_mvmd_field_injection_refused` (W4b)  ← S15
- [ ] `bootstrap_order_returns_not_ready` (W1)  ← S16
- [ ] `cross_backend_broker_matrix_libkrun_fc_apple_vz` (W6)  ← S17
- [ ] `json_type_confusion_returns_bad_request_not_panic` (W2)  ← S19
- [ ] `composition_depth_cap_enforced` (W1)
- [ ] `addon_proxy_stub_round_trip` (W1; v2-ready substrate)
- [ ] `hostile_guest_raw_socket_bypass` (W7)
- [ ] `audit_entry_enqueued_before_response_returned` (W2)  ← S22
- [ ] `audit_batch_survives_supervisor_crash_mid_window` (W2)  ← S22
- [ ] `mvmd_unsigned_catalog_rejected` (W4b)  ← S23
- [ ] `mvmd_wrong_key_signed_catalog_rejected` (W4b)  ← S23
- [ ] `composed_secret_does_not_leak_via_composer_response` (W6 lint + W5 runtime)  ← S24
- [ ] `placeholder_in_outbound_request_dropped_and_audited` (W7 + gvproxy/passt)  ← S25
- [ ] `host_secrets_v1_latency_floor_holds_warm_and_cold` (W5)  ← S26
- [ ] `service_session_revoked_refuses_further_calls` (W2 + C2 CLI)  ← S27
- [ ] `host_secrets_v1_signed_payload_jcs_roundtrip` (W5)  ← S28
- [ ] `secrets_subprocess_crash_isolation` (W1)  ← T4 (subprocess crash → supervisor survives, workload sees Unavailable)
- [ ] `secrets_subprocess_cannot_reach_supervisor_memory` (W1)  ← T4 (process isolation invariant)
- [ ] `supervisor_crash_subprocess_exits_via_pdeathsig` (W1)  ← T4 (lifecycle)
- [ ] `secrets_subprocess_uid_902_seccomp_setpriv_enforced` (W5)  ← T4 (privilege confinement)
- [ ] Fuzz lane: `fuzz_service_call.rs` ≥5min/PR (W6)

**Manual falsifiability check (post-W7):**
- [ ] Add a fourth service `host.dev.echo.v1` in a throwaway branch. Single new handler file + one registry line + `ServiceBinding` entry. If it requires touching the envelope, registry, or auth path, the design failed and v1 needs redesign before the host-logging follow-up plan starts.
- [ ] (Bonus) Wire a *stub* out-of-process addon at `~/.mvm/services/dev.echo.v1/` with a static manifest, verify the broker's addon-proxy path round-trips a call. Confirms A4 substrate is real.

## Decisions (resolving the earlier open questions)

1. **Cross-VM in scope; mvm broker delegates to mvmd over iroh ALPN.** Per-VM data stays in the supervisor handler. Cross-VM data via mvmd-agent's existing iroh transport with new `AgentRequest` variants. mvmd-side work: Plan 51 + ADR-0022.
2. **Encoding: JSON via `serde_json`** (DECIDED 2026-05-26, T3). Switched from CBOR. v1 has no genuinely binary payload; SDK-matrix friction + `jq` debuggability + consistency with existing JSON channels (`GuestRequest`, `HostBoundRequest`) win. Signed payloads use JCS (RFC 8785) for canonical bytes.
3. **`broker.v1/list_services` exposed.** Workloads enumerate bound services + verbs + deprecation flags at runtime.
4. **`host.secrets.v1` runs in a dedicated subprocess** (DECIDED 2026-05-26, T4 — production-ready isolation). Separate binary `mvm-secrets-dispatcher` at uid 902, seccomp `standard`, setpriv; UDS-only access to the supervisor. Industry pattern (AWS STS, HashiCorp Vault, Kubernetes ServiceAccount token controllers).
5. **The out-of-process handler substrate ships in v1 with secrets as its first consumer** (DECIDED 2026-05-26, T5). v2 third-party addons reuse the same substrate; no protocol change needed when they land.

## Comparison: SDK-hook vsock vs TLS-terminating proxy (ADR-049 alternatives)

ADR-049 considered two architectural shapes for secret substitution; Plan 104 implements the chosen default and adds a backstop from the alternative.

**(a) Host-side TLS-terminating proxy with injected CA** — a host-issued CA is installed in the guest's trust store; the proxy terminates TLS, injects credentials in plaintext, re-encrypts upstream. Simpler integration (no SDK matrix, library bypass is impossible since the proxy intercepts at L4/L7 regardless of what the guest does). The trade is significant: it **expands the host's trust boundary into the guest's trust store** — a CA the host controls is now trusted by the guest's TLS stack for *all* outbound connections, not just secret-bearing ones.

**(b) Vsock substitution via SDK hook** *(the default mvm chose)* — the SDK hooks the HTTP client *before* TLS, asks the host for a destination-bound signed credential, injects it into the outbound request, and the guest does its own TLS to upstream. **The guest's trust store is untouched.** Protocol-agnostic (HTTP/1.1, HTTP/2, HTTP/3, gRPC, mTLS). Cost: per-language hook matrix (ADR-049 §W3, Plan 104 W7) — Python `requests`/`httpx`/`aiohttp`, TypeScript `fetch`/`axios`, Rust `reqwest`/`hyper`.

ADR-049 chose (b) as default because preserving the guest's trust boundary outweighed the SDK-matrix cost. (a) ships separately as the `unsafe_guest_tls_inspection` opt-in for workloads that can't be modified (vendored binaries, third-party agents).

**Plan 104 takes the strongest property of each:** (b) is the primary path; **S25 adds the network-layer enforcement property from (a) as a fallback** — gvproxy/passt detects `mvm-secret://` token pattern in outbound HTTP bytes and drops the frame, emitting `secret.substitute.bypass_detected`. If an attacker substitutes the SDK with a malicious version that strips substitution hooks, S25 catches the unsubstituted placeholder at the L4/L7 boundary before it leaves the host. Combined: ADR-049 (b)'s untouched trust store *and* a library-bypass-resistant backstop.

## Security considerations and new surfaces

The broker is a new attack surface. Five independent gates protect every call (§"Capability gating"). Per-surface mitigations follow.

**S1 — JSON parser as TCB code.** `serde_json` (already in-tree, used by existing `GuestRequest` / `HostBoundRequest` channels — well-fuzzed). Knobs: `BROKER_MAX_FRAME_BYTES=65536`, `BROKER_MAX_DEPTH=8`, `BROKER_PARSE_TIMEOUT_MS=50`. Failure mode: supervisor dies → workload torn down → audit shows broker died mid-call → no exfil. The secrets subprocess uses its own `serde_json` instance — a parser bug exploited in the general broker doesn't pivot to the secrets subprocess's memory.

**S2 — Capability creep.** CI lint `xtask check-handler-adr-coverage`; static catalog at admission (no runtime registration in v1); every new verb requires fuzz-corpus seed.

**S3 — Timing / call-rate covert channels.** Rate limit applies to read-only services too; per-workload total-call/minute budget escalates to `ServiceCallAbuse` audit + (optional) workload pause via Plan 82.

**S4 — Cross-tenant information leakage.** mvmd's tenant-scoped-authz refuses any `tenant_id ≠ workload.tenant_id`. Workload-id signed by mvmd, unforgeable. Audit on both sides correlatable by `correlation_id`.

**S5 — Supervisor blast radius (revised: secrets isolated by process boundary).** The general broker (port 5300) runs in the supervisor; `host.secrets.v1` runs in the **secrets subprocess** with no shared address space. A use-after-free / integer overflow / logic bug in the general broker's schema/auth/binding/quota code cannot reach the credential-minting code, the in-flight grant table, or the keystore policy state. `zeroize::Zeroize` on secret-bearing types in the dispatcher. Subprocess crashes don't kill the supervisor; the supervisor returns `Err(Unavailable)` and restarts the subprocess (max 3 times per workload, then workload pause). v2 third-party addons reuse the same subprocess pattern.

**S6 — DoS / resource exhaustion.** Rate limit + size cap + per-workload broker-CPU budget (`BROKER_CPU_BUDGET_MS_PER_MIN=50`) + memory cap (`BROKER_INFLIGHT_MEM_CAP_BYTES=1048576`).

**S7 — Replay across workloads.** Session id minted at plan admission (128-bit ULID), monotonic sequence enforced; session id recorded in audit as `service.session.ended` at workload stop.

**S8 — Pause/resume + snapshot.** On pause, broker flushes in-flight (`Throttled` to outstanding), refuses new until resume. Resume mints fresh session — old correlation IDs invalidated.

**S9 — Logging vs redaction discipline.** `Recorder::record_service_call` API takes typed fields only — no payload-string param. Secret types must impl `RedactedDebug`, not `Debug`. CI sentinel-grep fixture.

**S10 — Out-of-process handler TCB scope.** v1 ships the out-of-process substrate with the **secrets dispatcher as its first consumer** (uid 902, seccomp `standard`, setpriv, UDS-only access). The substrate code lives in the supervisor TCB; the dispatcher binary (`crates/mvm-secrets-dispatcher/`) is a new line of TCB code — minimal, single-responsibility, dedicated security review per the no-`do_exec` discipline. v2 third-party addons reuse the same subprocess pattern; addon binaries are signed under claim-9 deps-audit pipeline at admission.

**S11 — Response-size amplification.** A small `ServiceCall` could trigger a large `ServiceResponse` (e.g., `broker.v1/list_services` for a workload with 50 bindings). Mitigation: per-handler `response_size_cap()` (default 64 KiB); if exceeded → `Err(ResponseTooLarge)` + audited. Knob: `BROKER_RESPONSE_CAP_DEFAULT_BYTES=65536`.

**S12 — Cumulative lifetime quota per workload.** Token-bucket covers per-window load; lifetime quota covers total budget. `ServiceQuotas { lifetime_calls: u64, lifetime_bytes_out: u64 }` per `ServiceBinding`. Exceeded → `Err(LifetimeQuotaExhausted)` + audited; workload may pause (Plan 82) or be hard-refused per its plan policy. Lifetime quotas survive pause/resume (carried in workload state).

**S13 — Circuit breaker for unhealthy handlers.** A handler that fails N consecutive calls (e.g., mvmd is down for `host.cost.v1::tenant`) opens a circuit breaker for that handler; subsequent calls return `Err(Unavailable)` immediately without dispatch. Half-open recovery after a backoff. Prevents both (a) wasting supervisor CPU on repeated-fail handlers and (b) timing-amplification attacks (slow-fail handlers leaking timing info). Knobs: `BROKER_CB_FAIL_THRESHOLD=5`, `BROKER_CB_HALF_OPEN_AFTER_MS=10000`.

**S14 — Inter-call memory hygiene.** A handler must not leak material from call N to call N+1 (e.g., a left-over key in `Arc<Mutex<_>>`). Mitigation: `ServiceHandler::dispatch` constructs fresh per-call state; any handler-internal cache uses `zeroize::Zeroizing<…>` wrappers. CI lint `xtask check-no-mutable-handler-state` scans handler modules for `Mutex<T> where T: !Zeroize`, fails build on findings (with allowlist for known-safe caches).

**S15 — Confused-deputy via mvmd response injection.** Cross-VM handlers call mvmd and use the response to make decisions. A compromised mvmd could inject fields the broker uses as authority signals. Mitigation: mvmd responses parsed with `deny_unknown_fields`; any field used in a decision path is typed strictly; the broker never reads "trust signals" from mvmd response payloads — only the decision (allow/deny) is consumed, and mvmd is the authz authority by design.

**S16 — Bootstrap-order failure mode.** A workload boots faster than the broker listener. Mitigation: broker listens *before* the guest process is spawned; if a workload reaches the broker port before fully initialized, broker returns `Err(NotReady)` (not crash, not silent drop).

**S17 — Cross-backend behavioral parity.** Broker must behave identically on libkrun (macOS), Firecracker (Linux), Apple Container (macOS), and vz (Apple Silicon, Sprint 55). Vsock semantics differ subtly. Mitigation: cross-backend test matrix in CI exercising the broker handshake + at least one handler call on each backend. Failure: backend-specific shims rather than divergent behavior.

**S18 — Audit log size and rotation (carried, decided in the host-logging follow-up plan).** Once `host.audit.v1` ships (the host-logging follow-up plan), workloads can write to the chain — the chain can grow unboundedly. Rotation strategy is the host-logging follow-up plan's responsibility, but Plan 104's `ServiceCall` entries are short and bounded (~200 B each), so Plan 104 doesn't worsen the problem materially.

**S19 — JSON `Value` type confusion.** The envelope's `payload: serde_json::Value` is dynamically typed; a handler expecting an int could receive a string. JSON is less prone than CBOR to numeric ambiguity (no int-vs-float distinction issue) but still requires typed parsing. Mitigation: per-handler `parse_policy` and `parse_payload` methods use typed `serde` deserialization with `deny_unknown_fields`; type mismatch → `Err(BadRequest)` not panic.

**S20 — Session key handling on host signer rotation (minor).** If the host signer key rotates mid-workload, existing workload session keys (minted from admission ceremony) keep working — they're independent. Documented; no code change required.

**S21 — Vsock back-pressure as covert channel.** If the guest queues `ServiceCall`s faster than the broker drains, queue depth itself encodes a signal. Mitigation: bounded vsock receive queue per workload (default 16) with explicit drop policy — over → `Err(QueueFull)` immediate, no blocking. Knob: `BROKER_QUEUE_DEPTH=16`.

**S22 — Audit batch durability window (BLOCKING).** `audit_durability = Batched(100ms)` (§Efficiency posture) means a supervisor crash within the batch window loses the audit entry for any call admitted in that window. A crafted payload (or a panicking handler) inside the window leaves no forensic trace.
- Mitigation: the audit *enqueue* must precede the caller response — batching applies only to `fsync`, not to in-memory queue insertion. The `Recorder` API takes the entry synchronously before `dispatch` returns; the batch flusher fsyncs to disk on a 100ms timer.
- Test: `audit_entry_enqueued_before_response_returned` — synthesize a call, observe enqueue order vs response; kill the supervisor mid-batch, restart, assert the chain contains the entry.

**S23 — Tenant catalog must be mvmd-signed (BLOCKING).** Per §A7, the per-VM supervisor pulls the tenant catalog from mvmd-agent and intersects it with the workload's bindings. A compromised mvmd-agent (or a MITM in the iroh transport, even with mTLS) could return a wider catalog than the tenant is entitled to, and the supervisor would chain-sign the bogus intersection.
- Mitigation: the catalog response carries an mvmd-fleet-credential-signed envelope (the same ADR-0022 §Trust model signature mvmd uses on tenant authorization). The supervisor verifies the signature against a pinned mvmd public key before trusting the catalog payload. Without this, ADR-0022's "tenant-scoped authz lives in mvmd" claim is weaker than advertised.
- Test: `mvmd_unsigned_catalog_rejected` — fixture mvmd returns an unsigned (or wrong-key-signed) catalog; supervisor refuses admission with audit `service.catalog.signature_invalid`.

**S24 — Privileged composition can leak secrets (BLOCKING).** §A5 allows a handler to invoke `host.secrets.v1` via `ServiceCallContext::invoke`. Claim Y covers raw-secret leakage *through* `host.secrets.v1`'s response, but a composing handler (e.g., `host.config.v1` fetching a credential to call mvmd) could inadvertently include the composed credential in its *own* outbound `ServiceResponse`. Bug-in-composer = secret-leak-via-composer.
- Mitigation: handlers declaring `composes_with([host.secrets.v1])` are statically constrained — the composing handler's return type may not embed `serde_json::Value` borrowed from the composed result. The new `xtask check-handler-composition` (already in W6) extends to lint this: any handler that calls `ctx.invoke("host.secrets.v1", …)` and whose response payload contains a field assigned from the invoke result fails the build. A whitelist (`#[allow(secret_passthrough)]` on the specific field) is available with mandatory review.
- Test: `composed_secret_does_not_leak_via_composer_response` — fixture composer handler attempts to embed the secret; lint fails the build (positive); whitelisted path tested separately.

**S25 — SDK integrity / placeholder egress backstop (BLOCKING).** ADR-049 §"Opt-out by raw socket" notes that a guest ignoring `mvm-sdk-runtime` can call `socket(2)` directly. The deeper risk: if an attacker substitutes the deps volume with a malicious SDK that *appears* to be `mvm-sdk-runtime` but silently strips substitution hooks, `mvm-secret://01H…` placeholders egress *as-is* to the destination. The destination sees a useless string, but the workload's tracking that a credential "left the workload" is broken — and any logging at the destination now contains the placeholder string.
- Mitigation: the host-side egress proxy (gvproxy on macOS, passt on Linux) detects the `mvm-secret://` token pattern in outbound HTTP request bytes (URI, headers, body — at the point flow-audit already inspects per Plan 101) and **drops the frame**, emitting `secret.substitute.bypass_detected` to the audit chain. This is a backstop at the L7 boundary; the SDK is best-effort. Claim 9 (signed deps volume + CVE scan) is the primary defense; this is the belt for the suspenders.
- Test: `placeholder_in_outbound_request_dropped_and_audited` — guest sends a raw HTTP request containing the placeholder; egress proxy drops; audit chain shows the bypass detection.

**S26 — First-call cold-cache timing oracle on `host.secrets.v1`.** Per §C3, `host.secrets.v1` is `Idempotency::MintFresh` — but the first call to a given grant must perform lookup + verify + mint, while subsequent calls hit a warmer code path. A workload can use first-call latency relative to subsequent-call latency as an oracle on grant presence or system state.
- Mitigation: `host.secrets.v1` pads response latency to a fixed floor (default 5ms) regardless of cache state. The audit-write latency for the `PerCall` durability is included in the floor budget.
- Knob: `BROKER_SECRETS_LATENCY_FLOOR_MS=5`. Test: `host_secrets_v1_latency_floor_holds_warm_and_cold`.

**S27 — Signed-plan revocation: no in-session invalidation when host signer key is rotated for cause.** S20 covers the cryptographic-independence case (rotation doesn't break in-flight sessions). S27 covers the *intentional* revocation case: operator rotates the key specifically because a plan was compromised; the workload is still running and the broker continues serving it.
- Mitigation: new `mvmctl services revoke <workload>` operation (extends §C2 CLI) flushes the workload's broker session, emits `service.session.revoked { reason }` to the audit chain, and refuses further calls. Distinct from workload stop (which tears down the whole VM). Available via mvmd-agent for fleet operators (extends mvmd Plan 51 catalog endpoint with a revocation action).
- Test: `service_session_revoked_refuses_further_calls`.

**S28 — JSON canonical encoding for signed credential payloads.** Signed credentials returned by `host.secrets.v1` are JSON. JSON is not canonically encoded by default — key ordering, whitespace, and Unicode normalization can all vary. If the signed payload is re-serialized between sign and verify, the signature could fail (or, worse, a different encoding could pass).
- Mitigation: signed payloads use **JCS (JSON Canonicalization Scheme, RFC 8785)** for the bytes-to-sign. Sorted keys, no whitespace, defined number serialization, NFC Unicode. Specified in ADR-059 at write time; the dispatcher's signing path uses the `serde_jcs` crate (or equivalent) to produce canonical bytes before Ed25519 signing.
- Test: `host_secrets_v1_signed_payload_jcs_roundtrip` — re-encode the signed payload through JCS, assert byte-identical to the original.

### Efficiency posture

- Parse + auth + binding-check (general broker, in-process): ~150µs (JSON is faster to parse than CBOR for typical envelopes).
- In-supervisor handler latency: sub-millisecond target.
- **Secrets subprocess round-trip:** supervisor → UDS → subprocess → handler → UDS → supervisor. Adds ~100–300µs over an in-process call (one extra memcpy + context switch). Acceptable: secrets calls are rare relative to time/cost, and ADR-049's budget (one vsock round-trip + Ed25519 sign per egress) already comfortably accommodates it.
- mvmd-delegated handler: sub-100ms (pre-warmed iroh connection + agent-local TTL cache).
- Audit append: per-handler `audit_durability()` — `Batched(100ms)` for non-secret services (S22: enqueue precedes response; batch only on fsync), `PerCall` for `host.secrets.v1` + future `host.audit.v1`.
- Circuit breaker prevents CPU waste on failing handlers (incl. crashed-subprocess detection on the secrets path).

### Proposed security claim additions

Two new claims for the broker (numbers to be assigned in ADR-059 against ADR-002's live list — **Sprint 56's "claim 10" rules out claim 10 from this plan**):

> **Claim X (numbering TBD).** Every host-side service the broker exposes is bound to a signed `ExecutionPlan.services` binding, enforced before handler dispatch, and audited via the chain-signed log. A tampered binding fails plan verification; an unbound call is refused with an audited deny.

> **Claim Y (numbering TBD).** No raw secret value crosses the broker channel. `host.secrets.v1` returns destination-bound, time-bound signed credentials only. Raw secret bytes never leave the supervisor's address space.

### Surfaces that don't expand

- No new host process / daemon / uid / persistent socket on disk in v1. (v2 addons add per-addon uid 902 + per-VM UDS, in a separate plan.)
- Trust boundary unchanged — supervisor was already trusted.
- Egress policy unchanged — broker is host↔guest only.
- `prod-agent-no-exec` unchanged — no broker verb is code-execution-shaped.

## mvmd-side extension — Plan 51 + ADR-0022 (drafts, to be written to mvmd repo on approval)

### `../mvmd/specs/plans/51-host-services-cross-vm-endpoints.md` (draft)

> **Plan 51 — Host services cross-VM endpoints**
>
> ### Context
>
> mvm's Plan 104 lands a host-services broker. Cross-VM services need data only mvmd has. This plan adds the mvmd-side endpoints and proxy plumbing.
>
> ### Architecture
>
> - **New gateway endpoints** in `crates/mvmd-gateway/src/routes/host_services.rs`:
>   - `GET /v1/host-services/tenant/{tenant_id}/cost?window=…`
>   - `GET /v1/host-services/tenant/{tenant_id}/peers?label_selector=…`
>   - `GET /v1/host-services/tenant/{tenant_id}/config/{key}`
>   - `GET /v1/host-services/tenant/{tenant_id}/rate-budget/{service}`
>   - `GET /v1/host-services/tenant/{tenant_id}/catalog` (the tenant's allowed service set; see mvm §A7)
> - Endpoints gated by mvmd's HMAC-SHA256 API key auth + tenant-scoped-authz. Per-node supervisor authenticates with its fleet credential AND carries a signed `X-MVM-Workload-Id` header verified against mvmd's instance registry. mvmd rejects any query whose `tenant_id` doesn't match the workload's tenant.
> - **Agent-side proxy** in `crates/mvmd-agent/src/host_services_proxy.rs`. Same shape as `sandbox_proxy.rs`. **Transport detail:** new verbs land as `AgentRequest` enum variants over iroh ALPN (existing `crates/mvmd-agent/src/transport.rs` pattern), NOT as a new HTTP route dispatched by the agent. The gateway HTTP routes above are for fleet operators and external integrators; the broker → agent path is iroh-native.
> - **Audit:** every cross-VM call logged in both mvmd's audit stream AND mvm-supervisor's chain — correlatable by `correlation_id`.
>
> ### Build sequence
>
> - [ ] **W1 — Endpoints + auth.** Five routes, workload-id verification, tenant-scoped-authz refusal tests.
> - [ ] **W2 — Agent proxy.** `host_services_proxy.rs`, QUIC dispatcher wiring.
> - [ ] **W3 — Cost + catalog handlers.** Aggregator + tenant catalog endpoint.
> - [ ] **W4 — Hostile-tenant tests.** Cross-tenant attempts, forged workload-id, response replay, malformed-response-side fuzz.
>
> ### Risks
>
> - **R1 — Latency.** Mitigation: agent-local TTL cache for hot keys.
> - **R2 — mvmd-down.** Mitigation: 500ms timeout, `ServiceErrorCode::Unavailable` propagated to guest; never stale data.

### `../mvmd/specs/adrs/0022-mvmd-host-services-delegation.md` (draft)

> **ADR-0022 — mvmd as the cross-VM delegate for the host services broker**
>
> - Status: Proposed
> - Date: 2026-05-26
> - Owner: MVM Project
> - Related: ADR-002, ADR-049, ADR-008, mvm-side ADR-059 (host services broker, proposed)
>
> ### Context
>
> mvm's Plan 104 / ADR-059 introduce a host services broker. Cross-VM services need mvmd-only data. Two shapes:
>
> - **(a) mvm supervisor reaches across workloads directly.** Rejected: violates CLAUDE.md's tenant isolation rule; forces redundant tenant-scoped-authz in mvm; makes per-VM blast radius cover other tenants.
> - **(b) mvm supervisor delegates to mvmd.** Cross-VM data via mvmd's tenant-scoped-authz + audit.
>
> ### Decision
>
> **(b).** mvmd is the single point of tenant-aggregated data and tenant-scoped authorization.
>
> ### Trust model
>
> Two identities per call:
> - **Node identity** (existing): fleet credential. Transport trust.
> - **Workload identity** (new): signed `X-MVM-Workload-Id` header. mvmd verifies against instance registry and refuses tenant_id mismatches.
>
> Both required: node identity is transport-level; workload identity binds the request to one tenant.
>
> ### Consequences
>
> - **Positive:** single tenant-scoped-authz; correlatable audit; mvm per-VM blast radius stays single-tenant.
> - **Negative:** cross-VM calls have higher latency. Mitigated by agent-local cache.
>
> ### Non-goals
>
> - Cross-tenant aggregation. No endpoint exposes data across tenants.
> - Mutation. v1 cross-VM endpoints are read-only.

**Both files written to mvmd repo verbatim from this draft on Plan 104 approval.**

## Follow-up — Host logging + workload audit (sketched, separate plan; number TBD)

(Numbering note: this follow-up was originally drafted as Plan 99, then renumbered through 103 as vz-phase-c-completion, symmetric-builder-vm, in-guest-volume-encryption, and gateway-audit-substrate plans took 99–102. Plan 103 is currently contested by a separate "egress secret detection + obfuscation" proposal flagged 2026-05-26; re-check `gh pr list` + `ls specs/plans/` and pick the next free number when this follow-up actually lands.)

**`host.logging.v1` — workload-emitted structured logs.** Verbs `emit`, `emit_batch` (≤100 records), `tail`. Per-record cap 8 KiB. Rate limit `BROKER_LOGGING_TOKENS_PER_SEC=200`. Audit chain records only `(service, verb, record_count, outcome)`. Cross-VM via mvmd Plan 51 W3 → tenant log sink. Workload-trusted content; opt-in regex redaction via `policy.redact`.

**`host.audit.v1` — workload-emitted audit chain entries.** Verb `record(category, fields)`. Per-record cap 4 KiB, `BROKER_AUDIT_TOKENS_PER_SEC=20`. New `EventCategory::WorkloadAudit` (distinct from `ServiceCall`). `audit_durability() = PerCall`. Verifier distinguishes workload-asserted from host-asserted entries.

**Companion ADR-060** — workload-audit semantics (workload-asserted vs host-asserted entries; verifier behavior; chain rotation policy — addresses S18).

**Depends on:** Plan 104 W1+W2 + mvmd Plan 51 W3.

## Additional considerations

These are decisions pinned for the implementer:

**C1 — Observability: Prometheus metrics from the broker.**
- Per-service latency histograms (`mvm_broker_dispatch_seconds{service=…}`)
- Error-rate counters (`mvm_broker_errors_total{service,code}`)
- Cap-breach counters (`mvm_broker_cap_breach_total{cap=…}` — frame-size, response-size, rate-limit, lifetime-quota, cpu-budget, memory-cap)
- Circuit-breaker state (`mvm_broker_cb_state{service,state}`)
- Lifetime-quota utilization (`mvm_broker_quota_remaining{service,kind}`)
- Wire into `tracing-prometheus` or the existing `mvm-metrics` crate.

**C2 — Dev experience: `mvmctl services` CLI subcommands.**
- `mvmctl services list <workload>` — shows bound services, remaining quotas, circuit-breaker state.
- `mvmctl services call <workload> <service> <verb> --payload <json>` — manual call for debugging (subject to plan binding gate; doesn't bypass auth).
- `mvmctl services catalog` — host's static catalog (which built-in handlers this build has compiled in, post-Cargo-features).
- Wire into `mvm-cli/src/commands/services.rs`.

**C3 — Per-handler idempotency declaration.**
- Different services need different retry semantics. Encode on the trait:
  ```rust
  pub enum Idempotency {
      MintFresh,                   // each call → new result, e.g. host.secrets.v1
      CacheRecent(Duration),       // cache N ms, e.g. host.cost.v1 → 1s
      DedupByCorrelation,          // same correlation_id → reject duplicate, e.g. host.audit.v1
  }
  fn idempotency(&self) -> Idempotency;
  ```

**C4 — Per-handler call timeout, not global.**
- The 50ms global cap is *parse*. Handlers can legitimately take longer.
- Encode `fn call_timeout(&self) -> Duration` on the trait. Defaults: `host.secrets.v1=20ms`, `host.time.v1=2ms`, `host.cost.v1::workload=5ms`, `host.cost.v1::tenant=150ms`, `host.audit.v1=20ms`, `host.logging.v1=10ms`.
- Beyond timeout → `Err(Timeout)` + audit; never blocks the supervisor's reconcile loop.

**C5 — Plans are immutable post-admission.**
- Adding a service binding to a running workload requires workload restart. Fits the signed-plan model.
- If demand emerges for runtime-mutable bindings, that's a future plan (likely a "supplemental binding signature" mechanism).

**C6 — TOCTOU between admission and dispatch.**
- The binding gate confirms "workload may call `host.secrets.v1`." The *grant* inside `host.secrets.v1` has its own expiry (per ADR-049).
- Be explicit in ADR-059: binding is "permission to call," handler enforces "freshness of underlying authority." Two layers, deliberately separate.

**C7 — Audit chain origin durability (DECIDED 2026-05-26: confirmed approach; concrete constraint pinned).**
- Each entry's `prev_hash` chains; tampering inside is detectable. An attacker deleting the chain file and starting fresh defeats this.
- **Mitigation (confirmed):** mvmd-agent syncs the chain head (the latest entry's hash) to durable cluster storage. Off-host anchor lets us detect a wholesale chain reset.
- **Concrete forward-looking constraint on Plan 104 W2** (so this isn't a wish): the `EventCategory::ServiceCall` audit entries must be:
  1. **Append-only with a stable canonical byte serialization** — entries are length-prefixed JSON canonicalized via JCS (RFC 8785, per S28) so a sync-shim can hash the entry bytes and `prev_hash` without re-serializing.
  2. **Self-contained per entry** — each entry carries `(prev_hash, ts, category, fields, sig)` with no out-of-band state needed to verify. A future sync mechanism can stream entries to mvmd-agent without needing to replay the whole chain.
  3. **`chain_head` exposed** — the supervisor exposes the latest entry's hash via an internal API (`AuditRecorder::current_head() -> Hash`) so a future sync agent can poll/push without rooting around in the JSONL file.
- Concrete sync impl lands in the host-logging follow-up plan; the three bullets above are the *contract* Plan 104 W2 must satisfy. If the entry serialization changes later, the sync mechanism breaks — so getting the canonical form right in W2 is load-bearing.

**C8 — Vsock back-pressure as covert channel.**
- See S21. Bounded receive queue (`BROKER_QUEUE_DEPTH=16`) with `Err(QueueFull)` drop policy.

**C9 — Naming consistency: ServiceId is `host.secrets.v1` (DECIDED 2026-05-26).**
- Original draft used `secrets.v1` (unprefixed) — inconsistent with `host.time.v1` / `host.cost.v1`.
- **Rename applied throughout this plan:** wire ServiceId is `host.secrets.v1`; handler type is `HostSecretsV1Handler`; supervisor handler file is `crates/mvm-supervisor/src/services/host_secrets.rs`; SDK dispatcher file is `crates/mvm-sdk/src/services/host_secrets.rs`; Cargo feature is `service-host-secrets`; test name prefix is `host_secrets_v1_*`.
- ADR-049's prose must be updated to mention `host.secrets.v1` as the broker-namespaced identifier when ADR-049 is touched in W5 (no semantic change to ADR-049; just an identifier substitution in the "Implementation: lands as …" line).

## Tensions and unresolved premise questions

Surfaced by adversarial review 2026-05-26. These are framing/design tensions the plan needs an explicit position on. Not bugs — choices that look load-bearing at second glance.

**T1 — "Five sequential rules in one substrate" — be honest about the failure model.**
The capability-gating section now says explicitly: all five gates run in the same supervisor task, in one address space, sharing the same parser and registry state. They're defense-in-depth against bugs in a *single rule*, not against substrate compromise. If the supervisor task is compromised (e.g., use-after-free in the schema parser corrupts the registry), gates 3, 4, 5 are also compromised. Updated the §"Capability gating" section to lead with this. No code change required; the framing was overselling.

**T2 — In-process broker forecloses operational moves.**
The "no new daemon" choice is presented as upside, but it also rules out: (a) hot-swap of a buggy handler without VM restart, (b) true crash isolation (`catch_unwind` does NOT catch aborts or stack overflows — a handler can still kill the supervisor), (c) broker restart mid-workload, (d) running the broker under stricter seccomp than the supervisor. Combined with C5 (plans immutable post-admission) + A7 (per-tenant catalogs from mvmd), the operational picture is: **a tenant catalog change in mvmd forces every running workload of that tenant to restart to pick it up.** This is not currently surfaced as a contract. Two paths:
- **Acceptable:** workloads under this broker are expected to be short-lived (sandbox runs, ephemeral compute) so catalog churn = next-workload pickup is fine. Document this explicitly.
- **Not acceptable:** the broker needs a "service-binding refresh" path that re-pulls the catalog without VM restart. That's a substrate change.
Current plan defaults to the first interpretation. Pin it or change it.

**T3 — Encoding: JSON (DECIDED 2026-05-26).**
Original draft used CBOR. Switched to JSON because v1 has no genuinely binary payload, the W7 ADR-049 SDK matrix (Python `cbor2` / TS `cbor-x` / Rust `ciborium`) introduces real cross-language friction, `jq` debuggability is real over project lifetime, and the existing `GuestRequest` / `HostBoundRequest` channels already use JSON in `crates/mvm-guest/src/vsock.rs` — consistency. Future binary payloads use base64-in-JSON on the specific field; 33% overhead on one field at ~500-byte typical payloads is negligible. Trade-off acknowledged: COSE-based signing (RFC 8152) would mandate CBOR; ADR-049's signing scheme is Ed25519-on-bytes (not COSE), so JSON is fine. JCS (RFC 8785 — JSON Canonicalization Scheme) is used for the bytes-to-sign when signing JSON payloads.

**T4 — `host.secrets.v1` runs in a dedicated subprocess (DECIDED 2026-05-26).**
Resolved by §"Host-side: two-process architecture" above. The general broker (port 5300, in-process) and the secrets dispatcher (port 5301, separate subprocess at uid 902 with seccomp + setpriv) share zero code paths and zero address space. A use-after-free in the general broker's dispatcher cannot reach the credential-minting code, the grant table, or the keystore policy state in the secrets subprocess. This is the production-ready isolation pattern (industry analogues: AWS STS, HashiCorp Vault, Kubernetes ServiceAccount token controllers — all out-of-process credential issuers). Cost: ~50% W1+W5 scope growth — a new crate (`mvm-secrets-dispatcher`), subprocess lifecycle in the supervisor, UDS proxy code path. Justified because (a) the user stated control-plane compromise as a security concern, (b) this gives A4 a concrete v1 consumer (see T5), (c) split-task→split-process migration later is more painful under the no-backcompat rule.

**T5 — A4 substrate ships in v1 with secrets as its first consumer (DECIDED 2026-05-26).**
Resolved by §A4 rewrite above. The "speculative substrate with no v1 consumer" criticism dissolves — the v1 consumer of the out-of-process handler substrate IS the secrets dispatcher (T4). Every line of the UDS proxy code is exercised by the security-critical secrets path on every workload start. v2 third-party addons reuse the *same* substrate when they land: same wire envelope, same handler trait, same audit flow, same subprocess pattern. The substrate's design is informed by the secrets dispatcher's real requirements, not a hypothetical addon's. This pattern of "ship the abstraction along with its first real consumer, defer hypothetical consumers to v2" is the right discipline.

**T6 — mvmd delegation is an architectural boundary, not a trust boundary.**
ADR-0022's trust model has the supervisor signing the `X-MVM-Workload-Id` header. A compromised supervisor forges arbitrary workload-ids; mvmd accepts them. The "blast radius stays single-tenant" guarantee only holds *if* the supervisor is uncompromised, in which case there's no remaining isolation to preserve (the supervisor would also release its own tenant's secrets). The delegation buys clean separation of *correctness* concerns (mvmd owns aggregation), not a new trust boundary. **Updated:** the §Cross-VM delegation prose and mvmd ADR-0022 should be explicit that this is an architectural boundary, NOT a trust boundary. The trust boundary remains "supervisor and below are trusted."

**T7 — C7 (audit sync) was a wish; now pinned to three concrete constraints.**
Resolved above in §C7 — the chain entry format must be (1) append-only canonical JSON (JCS, RFC 8785), (2) self-contained per entry, (3) expose `chain_head` via internal API. Plan 104 W2 must satisfy these contracts so the follow-up sync mechanism doesn't require rewriting the audit format.

### Out of scope here (deferred to future plans)

- `host.metrics.v1` (counter/gauge) and `host.trace.v1` (OpenTelemetry) — the host-logging follow-up plan+ in the workload-telemetry group.
- Hardware enclave integration for `host.secrets.v1` signing key (Apple Secure Enclave on macOS, TPM on Linux) — future hardening ADR.
- v2 addon detailed design — separate plan once v1 substrate is proven.
- GDPR / data-residency for cross-VM forwarded logs — the host-logging follow-up plan / ADR-060.
- Runtime-mutable bindings (supplemental signatures) — future plan if demand emerges.

## Risks

- **R1 — ADR-049 SDK matrix is a lot of code.** ~10 hook points across 3 ecosystems. Split W7 per language.
- **R2 — Per-VM supervisor adds a vsock listener.** Fuzz surface (W6) + a few MB memory (negligible).
- **R3 — `host.secrets.v1` is the most security-critical code shipped on the runtime path in months.** Dedicated security review of W5 before merge; W5 includes ADR-049's hostile-guest matrix; W6 fuzz target lands same PR window.
- **R4 — Schema change to `ExecutionPlan` requires `SCHEMA_VERSION` bump 4→5.** `crates/mvm-plan/src/plan.rs:45` defines `SCHEMA_VERSION: u32 = 4` with explicit "older verifiers must fail closed on unknown schema versions." Adding `services: Vec<ServiceBinding>` is a v5 schema; old (v4) plans hard-fail at verification, not silently accept the new field as empty. This is consistent with the saved no-backcompat rule but the earlier R4 wording ("old plans verify") was misleading. Migration: any in-flight v4 plans must be re-synthesized + re-signed under v5 to keep running; per the no-backcompat rule, no shim.
- **R5 — Plan numbering race (REVISED 2x).** Plan 98 was claimed by `98-vz-builder-vm.md` and Plan 103 by `103-w6a-implementation-tracker.md` mid-conversation; this plan moved to Plan 104. As of the final rename, **Plan 104 / ADR-059 / Sprint 57 / mvmd Plan 51 / mvmd ADR-0022 are free**. Sprint 55-derived work claims 97 and 98; Sprint 56 claims 99–102, 057, 058 in mvm and 50 in mvmd; Sprint 56 W6 follow-up claims 103. Re-verify all numbers immediately before opening the implementation PR; per saved guidance, do not renumber other sessions' work.
- **R6 — Lockstep delivery between mvm W4b and mvmd Plan 51.** Land mvmd Plan 51 W1+W2+W3 *before* opening mvm W4b. W4b PR pins required mvmd commit. mvm W1–W4a + W5 + W6 + W7 have no mvmd dep.
- **R7 — JSON encoding crates.** `serde_json` for the wire envelope (already in-tree), `serde_jcs` (or equivalent) for canonical signing bytes (RFC 8785). Existing `cargo-deny` lane catches regressions. **No CBOR libraries needed** — ciborium dropped from the dependency closure when T3 settled on JSON.
- **R8 — Cross-backend behavioral divergence.** Vsock semantics differ across libkrun, Firecracker, Apple Container, vz. Mitigation: W6 cross-backend test matrix; backend-specific shims rather than divergent behavior.
- **R9 — Sprint 56 claim 10 collision.** Sprint 56 owns "claim 10" (bytes leaving trust boundary). Plan 104's broker claims must be assigned different numbers in ADR-059 against the live ADR-002 claim list at write time.

## Non-goals (explicit)

- Streaming responses (monitoring, log tail). Envelope is request/response only in v1.
- Addon-provided handlers shipping in v1. v1 ships only the substrate (addon-proxy path implemented, no addons consumed).
- `unsafe_guest_tls_inspection` proxy-with-CA path from ADR-049. Ships separately.
- Non-HTTP secret substitution. Out of scope per ADR-049 §"Non-HTTP egress."
- Cross-VM cost aggregation across tenants. `host.cost.v1::tenant` is single-tenant.
- Audit log rotation strategy. Deferred to the host-logging follow-up plan / ADR-060 (when `host.audit.v1` lands).
