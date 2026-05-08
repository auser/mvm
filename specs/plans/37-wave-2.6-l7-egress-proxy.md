# Plan 37 Wave 2.6 — L7 Egress Proxy: wiring the inspector chain

> Status: ready for review
> Owner: Ari
> Parent: `specs/plans/37-whitepaper-alignment.md` §15
> Predecessors: Waves 2.1–2.5 (PRs #43, #44, #45, #46, #47)
> Estimated effort: 2 PRs (#48 Phase 1 = this plan; #49 Wave 2.7 ToolGate)

## Context

Waves 2.1–2.5 built five inspectors:

| Wave | Inspector            | Filters by                  |
| ---- | -------------------- | --------------------------- |
| 2.1  | `DestinationPolicy`  | (host, port) string         |
| 2.2  | `SecretsScanner`     | body content (regex set)    |
| 2.3  | `SsrfGuard`          | resolved IP                 |
| 2.4  | `InjectionGuard`     | body content (prompt-inj)   |
| 2.5  | `PiiRedactor`        | body content (PII shapes)   |

They're dead weight without something feeding them real outbound HTTP
requests. Wave 2.6 is that wiring step: replace `NoopEgressProxy` with
a real `L7EgressProxy` that listens on TCP, runs the inspector chain
on each request, and forwards or denies based on the verdict.

## Two-phase architecture

| Phase | Scope                                          | Inspectors that run |
| ----- | ---------------------------------------------- | ------------------- |
| **2.6 (this plan)** | HTTP CONNECT proxy. Opaque TLS tunnel for HTTPS; full request/response visibility for plain HTTP (gated behind a dev-only flag). | DestinationPolicy + SsrfGuard always; body inspectors only on plain-HTTP path |
| **2.6.5 (later)** | TLS MITM with per-workload name-constrained CA (ADR-006). | All five on every request |

Splitting like this keeps PR #48 reviewable and doesn't block Phase 1
on ADR-006's full implementation. Phase 2 plugs MITM into the same
shell; the proxy core, chain dispatch, DNS-pin, audit, and deny-path
UX are all already in place.

## Design decisions (confirmed during review)

1. **Listener: TCP on `127.0.0.1` inside the workload's network
   namespace.** Standard `HTTPS_PROXY=` shape; no env-var
   conventions to define for Unix sockets.
2. **HTTPS-only by default.** Plain HTTP gated behind
   `EgressPolicy::allow_plain_http`. The supervisor hard-errors at
   policy load if a `variant=prod` workload sets it true — this makes
   the secure default louder than a comment.
3. **Body cap: configurable per workload, default 16 MiB
   (16 * 1024 * 1024).** AI-provider requests with long contexts and
   image uploads routinely exceed 1 MiB; 16 MiB is the realistic
   ceiling. Over-cap requests deny with `body_too_large`.
4. **ToolGate (Wave 2.7) ships in the same PR-set.** Independent
   code path; stacks on #48 only because both edit
   `Supervisor::with_*` builders.

## Architecture

```rust
// crates/mvm-supervisor/src/l7_proxy.rs
pub struct L7EgressProxy {
    chain: Arc<InspectorChain>,
    audit: Arc<dyn AuditSigner>,
    resolver: Arc<dyn DnsResolver>,    // mockable for tests
    body_cap_bytes: usize,
    allow_plain_http: bool,
    bind_addr: SocketAddr,
}

impl L7EgressProxy {
    pub async fn serve(self) -> Result<(), EgressError> { ... }
}

#[async_trait]
impl EgressProxy for L7EgressProxy {
    async fn evaluate(&self, host: &str, port: u16, body: &[u8])
        -> Result<EgressDecision, EgressError>
    { /* runs the chain twice (pre/post DNS) */ }
}
```

### Per-connection lifecycle (CONNECT path)

1. Parse `CONNECT host:port HTTP/1.1` request line.
2. Build `RequestCtx { host, port, path: "", body: vec![],
   resolved_ip: None }`.
3. **Run chain (1st pass).** `DestinationPolicy` checks the (host,
   port) tuple. Body inspectors are no-ops (body is empty).
4. If `Deny`: respond `HTTP/1.1 403 Forbidden\r\nX-Mvm-Egress-Reason:
   <rule_name>: <reason>\r\n\r\n`; emit audit entry; close.
5. Resolve `host` → first IP via the `DnsResolver`. Set
   `ctx.resolved_ip`.
6. **Run chain (2nd pass).** `SsrfGuard` now sees the pinned IP and
   either denies or passes.
7. If `Deny`: 403 + audit + close (same path as step 4).
8. If `Allow`: connect to **the pinned IP** (not the hostname — DNS
   rebinding defence). Send `HTTP/1.1 200 OK\r\n\r\n` to the
   workload, then `tokio::io::copy_bidirectional`.
9. On any `Transform { note }` along the way: collect into the audit
   entry's `transforms` field. Wave 2.5 is detect-only so we never
   mutate the body.

### Per-connection lifecycle (plain-HTTP path)

Only available when `allow_plain_http = true` (forbidden in production
variants).

1. Parse request line + headers, extract `Host:` header.
2. Read body up to `body_cap_bytes`. Over-cap → 413 `Payload Too
   Large` + audit entry with `outcome=deny, reason=body_too_large`.
3. Build full `RequestCtx`. Run chain (pre-DNS).
4. Resolve, pin IP, re-run chain.
5. On Allow: forward upstream via reqwest; stream response back.
6. On Deny: 403 with reason header.

### Plumbing into Supervisor

```rust
impl Supervisor {
    pub fn with_l7_egress(
        mut self,
        policy: &EgressPolicy,
        audit: Arc<dyn AuditSigner>,
        variant: Variant,         // dev | prod, from mvm-plan
    ) -> Result<Self, SupervisorError> {
        if variant == Variant::Prod && policy.allow_plain_http {
            return Err(SupervisorError::PolicyViolation(
                "plain HTTP not permitted for prod variants".into()
            ));
        }
        let chain = build_chain_from_policy(policy);
        self.egress_proxy = Arc::new(L7EgressProxy::new(
            chain, audit, policy, variant,
        )?);
        Ok(self)
    }
}

fn build_chain_from_policy(policy: &EgressPolicy) -> InspectorChain {
    InspectorChain::new()
        .with(Box::new(DestinationPolicy::from_allow_list(&policy.allow_list)))
        .with(Box::new(SsrfGuard::new()))
        .with(Box::new(SecretsScanner::with_default_rules()))
        .with(Box::new(InjectionGuard::with_default_rules()))
        .with(Box::new(PiiRedactor::with_default_rules()))
}
```

Order matches Plan 37 §15: cheapest/most-precise first.

## EgressPolicy extensions (mvm-policy)

```rust
pub struct EgressPolicy {
    pub allow_list: Vec<(String, u16)>,           // existing
    pub allow_plain_http: bool,                   // new; default false
    pub body_cap_bytes: u64,                      // new; default 16 MiB
    pub enabled_inspectors: BTreeSet<String>,     // new; default all
}
```

`enabled_inspectors` lets operators disable a specific inspector by
name (e.g., turn off `pii_redactor` for an analytics workload that
provably scrubs upstream). Default: all on.

## Audit emission

One `AuditEntry` per chain run:

```rust
struct EgressAuditFields {
    outcome: Outcome,                    // allow | deny | transform
    inspector: &'static str,             // denying inspector, or last-running
    host: String,
    port: u16,
    path: String,
    transforms: Vec<String>,             // every Transform { note }
    reason: Option<String>,              // for Deny
    resolved_ip: Option<IpAddr>,         // post-DNS pin
    duration_ms: u32,
}
```

Wired through the existing `AuditSigner` trait (Wave 1 already binds
`bundle_id` / `image_sha256` to every entry).

## Testing strategy

| Layer | What | Where |
| ----- | ---- | ----- |
| Unit | `L7EgressProxy::evaluate(host, port, body)` with mocked DNS resolver, asserts each chain-verdict branch | `mvm-supervisor::l7_proxy::tests` |
| In-process integration | Spawn proxy on ephemeral port, drive via real `TcpStream` writing CONNECT, assert 200/403 + headers + audit entries | same module |
| DNS-rebinding regression | Mock resolver returns `127.0.0.1` for `evil.com`; assert Deny via SsrfGuard with the host in the audit entry | same |
| Body-cap regression | Plain-HTTP request with body > cap → 413 + audit `body_too_large` | same |
| Variant gate regression | `Variant::Prod` + `allow_plain_http=true` → `Supervisor::with_l7_egress` returns `PolicyViolation` | `mvm-supervisor::supervisor::tests` |

End-to-end with real workload + Apple Container backend lands in
Wave 2.6.5 alongside MITM.

## Phase 1 scope (this PR / #48)

- `crates/mvm-supervisor/src/l7_proxy.rs` — the proxy.
- `crates/mvm-policy/src/policies.rs` — `EgressPolicy` extension fields.
- `crates/mvm-supervisor/src/supervisor.rs` — `with_l7_egress` builder.
- This file (`specs/plans/37-wave-2.6-l7-egress-proxy.md`).
- Test layers above.

**Explicit non-goals for Phase 1:**
- TLS MITM (Wave 2.6.5).
- Per-connection bandwidth limits.
- DNS result caching (single-resolve-per-request is security-critical).
- HTTP/2 / HTTP/3.
- Connection pooling to upstream (one-shot connect per request).

## Wave 2.7 (PR #49) — ToolGate vsock RPC

Stacks on #48 only because both edit `Supervisor::with_*`. Independent
code path; separate review.

- Replace `NoopToolGate` with `VsockToolGate`.
- Workload-side request shape (vsock RPC).
- Wire `Supervisor::with_tool_gate` builder.
- Tests in `mvm-supervisor::tool_gate::tests`.

## Acceptance criteria

PR #48 closes when:

1. ✅ `L7EgressProxy::serve` accepts CONNECT requests on TCP, runs the
   chain, returns 200/403 with the right headers.
2. ✅ DNS rebinding (mock resolver to private IP) hard-fails via
   SsrfGuard with the audit entry showing the resolved IP.
3. ✅ Plain-HTTP path forbidden for `Variant::Prod` policies; allowed
   for `Variant::Dev`.
4. ✅ Over-cap body denies with `body_too_large` and a 413.
5. ✅ Every chain run emits exactly one `AuditEntry`.
6. ✅ `cargo test --workspace` clean; `cargo clippy
   --workspace --all-targets -- -D warnings` clean.
7. ✅ Plan doc + runbook section published.

## Reversal cost

Low. The proxy is opt-in via `Supervisor::with_l7_egress` — leaving
the slot at `NoopEgressProxy` reverts to pre-2.6 behaviour. The
`EgressPolicy` field additions are additive (`#[serde(default)]` for
the new fields means older policy bundles still parse).
