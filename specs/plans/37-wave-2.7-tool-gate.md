# Plan 37 Wave 2.7 — ToolGate: vsock-RPC tool-call mediator

> Status: Phase 1 in-flight (this branch)
> Owner: Ari
> Parent: `specs/plans/37-whitepaper-alignment.md` §2.2 / §15
> Predecessor: Wave 2.6 (PR #48 — L7EgressProxy)
> Phase split: 2.7a (policy decision) + 2.7b (vsock RPC)

## Context

Plan 37 §2.2 is the second half of the differentiator. Where the L7
egress proxy mediates *outbound HTTP* calls, the ToolGate mediates
*tool calls* — the inside-the-VM RPC every agentic workload makes
("read this file", "execute this query", "send this Slack message").
Each tool call is a structured request the workload sends over vsock
to the supervisor; the supervisor decides Allow / Deny based on the
plan's bound `ToolPolicy`, then either invokes the tool or refuses.

Wave 1 shipped the trait surface (`ToolGate::check(&str)`) and a
fail-closed `NoopToolGate`. Wave 2.7 fills in the real impl. Like
Wave 2.6, it splits into two phases so the policy decision and the
I/O surface ship separately:

| Phase | Scope | Status |
| ----- | ----- | ------ |
| **2.7a (this PR)** | `PolicyToolGate` impl backed by `ToolPolicy.allowed` allowlist; `Supervisor::with_tool_gate` builder; `ToolAuditSink` + capturing/noop sinks; loud-by-default deny reasons | ✅ Done |
| **2.7b (next PR)** | Vsock listener loop on the supervisor; JSON-RPC framing; per-call audit emission via `ToolAuditSink`; workload-side guest-agent client lib | Deferred |

## Why this shape

- **Policy decision is the load-bearing piece.** The vsock RPC is
  plumbing; the actual security claim ("workload `X` cannot call
  `rm_rf` even if its code wants to") is fully captured by the
  Phase 1 `PolicyToolGate` + audit sink. Phase 2 just delivers
  the requests to the gate.
- **Phase 1 is fully testable without I/O.** `ToolGate::check`
  is a pure async function. Phase 1 ships 12 unit tests covering
  the allowlist behaviour, deny-reason verbosity, the deny-all
  sentinel, dedup of duplicate `Vec` entries, and audit-sink
  capture.
- **Phase 2 layers on cleanly.** The vsock listener loop is
  analogous to Wave 2.6's `L7EgressProxy::serve_connection` —
  a small framing parser + dispatch into the gate. Splitting
  it out keeps the PR reviewable.

## Design (Phase 1 — what's in this PR)

### `PolicyToolGate`

```rust
pub struct PolicyToolGate {
    allowed: BTreeSet<String>,
    quiet: bool,
}

impl PolicyToolGate {
    pub fn from_policy(policy: &ToolPolicy) -> Self;
    pub fn new<I, S>(names: I) -> Self where I: IntoIterator<Item = S>, S: Into<String>;
    pub fn deny_all() -> Self;
    pub fn quiet_deny_reasons(self, quiet: bool) -> Self;
    pub fn is_allowed(&self, name: &str) -> bool;
}
```

Stored as `BTreeSet<String>` for O(log n) lookup + stable
ordering of the allowlist when rendered into deny reasons.

### Audit fan-out

Mirrors Wave 2.6's `EgressAuditSink` shape so the supervisor's
audit fan-out stays uniform:

```rust
pub struct ToolAuditFields {
    pub outcome: ToolOutcome,            // Allow | Deny
    pub tool_name: String,
    pub reason: Option<String>,
}

#[async_trait]
pub trait ToolAuditSink: Send + Sync {
    async fn record(&self, fields: &ToolAuditFields)
        -> Result<(), ToolAuditError>;
}
```

Plus `CapturingToolAuditSink` (in-memory for tests/dev) and
`NoopToolAuditSink` (silently succeeds).

### Supervisor builder

```rust
impl Supervisor {
    pub fn with_tool_gate(mut self, policy: &ToolPolicy) -> Self {
        self.tool_gate = Arc::new(PolicyToolGate::from_policy(policy));
        self
    }
}
```

Note: empty `ToolPolicy.allowed` is **deny-all**, not "anything
goes". Workloads that genuinely need no tool restrictions must
wire a different `ToolGate` impl explicitly — fail-closed by
construction.

### Deny-reason verbosity

Loud by default (the deny reason includes the full allowlist) so
the operator dashboard shows in one read both:

- which tool was rejected
- which tools would have been permitted

Workloads with very long allowlists can opt into
`PolicyToolGate::quiet_deny_reasons(true)` to omit the listing.

### Audit safety

The deny reason **does** echo the rejected tool name and the
allowlist, but never any tool *arguments* — those don't reach
this layer. Wave 2.7b's vsock RPC handler will pass arguments
to the tool implementation only after the gate Allows; the gate
itself sees the name only.

## Tests in Phase 1

12 new tests in `mvm-supervisor`:

- allowed name → Allow
- unknown name → Deny with rejected name + full allowlist visible
- quiet mode → Deny with rejected name only
- deny_all sentinel → blocks every call
- empty allowlist → explicit `<empty allowlist — deny-all>` reason
- dedup of duplicate `Vec<String>` entries via `BTreeSet`
- stable BTreeSet ordering (audit determinism)
- `CapturingToolAuditSink` records both Allow and Deny entries
- `NoopToolAuditSink` silently succeeds
- 3 `Supervisor::with_tool_gate` integration tests covering
  Allow / Deny / fail-closed-empty paths

## Phase 2 scope (deferred — Wave 2.7b)

- Vsock listener loop on the supervisor side
- JSON-RPC envelope: `{ "method": "tool.check", "params": { "name": "..." } }`
  → `{ "result": "allow" | { "deny": "<reason>" } }`
- Per-call audit emission via `ToolAuditSink` from the dispatch path
- Guest-agent client lib (workload-side helper that wraps the
  vsock socket + framing)
- End-to-end test: workload binary issues a `tool.check` RPC,
  supervisor responds, gate decision matches `ToolPolicy.allowed`
- Backpressure / queue-depth limits on the vsock listener

## Acceptance criteria for Phase 1 (this PR)

1. ✅ `PolicyToolGate::from_policy(&ToolPolicy)` returns a gate that
   Allows allowlisted names and Denies everything else.
2. ✅ `Supervisor::with_tool_gate(&ToolPolicy)` swaps the
   `NoopToolGate` slot for a `PolicyToolGate`.
3. ✅ Empty `ToolPolicy.allowed` is fail-closed deny-all.
4. ✅ Deny reasons name the rejected tool and (by default) list the
   permitted tools.
5. ✅ `ToolAuditSink` trait + capturing/noop sinks shipped for
   Phase 2 to consume.
6. ✅ `cargo test -p mvm-supervisor --lib` clean (152 tests passing
   after Wave 2.6 + 2.7a).
7. ✅ `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Reversal cost

Trivial. `Supervisor::with_tool_gate` is opt-in; leaving the slot
at `NoopToolGate` reverts to pre-2.7 behaviour. The public
`ToolPolicy` struct is unchanged from Wave 1.
