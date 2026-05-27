# Plan 102 — Gateway audit substrate (Plan 101 W6 implementation)

This is the implementation sub-plan for Plan 101 leg 2 / W6 — the
gateway audit substrate that makes claim 10 ("bytes leaving the trust
boundary onto the host network gateway") testable end-to-end.

Plan 101 (`plans/101-in-guest-volume-encryption-and-gateway-audit.md`)
defines the wave map for the whole claim-10 substrate; W7 (the audit
event schema) shipped in PR #450; this plan covers W6.

## Context

The gateway is where guest egress crosses out of the microVM's trust
boundary. ADR-058
(`adrs/058-claim-10-bytes-leaving-trust-boundary.md`) commits us to a
tamper-evident record of every byte that crosses that boundary, signed
into the existing per-tenant chain at `~/.mvm/audit/<tenant>.jsonl`.

Both gateways we ship — gvproxy (macOS, Linux libkrun) and passt
(Linux passt mode) — have **no native flow-event API**. gvproxy
exposes three CLI flags and a debug verbosity knob; passt has
`--log-file` (warn-level only) and `--trace` (pcap-style packet dump,
not structured). Neither emits structured per-flow events. So we wrap
them.

## Scope decisions

These were resolved during planning (full rationale in
`plans/101-in-guest-volume-encryption-and-gateway-audit.md` and the
planning conversation):

1. **North-south only.** W6 captures microVM ↔ internet through the
   gateway. East-west (microVM ↔ microVM via the tenant bridge) is a
   separate audit plane, deferred to a future W11.
2. **In-process bridge** inside the existing libkrun/Vz supervisor.
   Avoids +1 process per microVM at scale; isolation guardrails are
   `catch_unwind` + bounded resources, not a separate binary.
3. **SOCK_STREAM listen + accept-many** at
   `~/.mvm/audit/gateway-<instance>.sock` (mode 0700). JSON-lines wire
   format. Multiple subscribers (supervisor + ad-hoc `nc -U`) can read
   concurrently.
4. **`etherparse` for L2/L3/L4 parsing.** Mature, `unsafe`-free, gated
   by `cargo deny` + `cargo audit`. Hand-rolling Ethernet/IPv6
   extension headers / vlan tagging / fragmentation is exactly where
   bugs hide.
5. **Staged in two PRs** — W6.A substrate plumbing first, W6.B real
   flow extraction second. Both tick the same Plan 101 W6 checkbox.

## W6.A — substrate plumbing

- [ ] Create `crates/mvm-supervisor/src/gateway_audit.rs` with
      `GatewayAuditSink` — owns the `UnixListener` at
      `~/.mvm/audit/gateway-<instance>.sock` (mode 0700), accepts
      subscriber connections, fans out JSON-line events.
- [ ] Create `crates/mvm-libkrun/src/gateway_bridge.rs` with the Tokio
      splice task. No L2/L3 parsing yet — just splice bytes both
      directions and emit placeholder `FlowOpened` on first byte and
      `FlowClosed` on EOF for each direction.
- [ ] Wire the bridge into `crates/mvm-libkrun/src/gvproxy.rs` —
      intercept the socketpair so the bridge sits between guest and
      gateway.
- [ ] Wire the bridge into `crates/mvm-libkrun/src/passt.rs` — same.
- [ ] Wire the bridge into `crates/mvm-backend/src/vz.rs:809-811`
      (the existing gvproxy lifecycle TODO).
- [ ] Single-writer audit chain: the bridge task sends `FlowEvent`
      messages over an mpsc channel to a per-tenant signer task; the
      signer task is the only caller of
      `FileAuditSigner::sign_and_emit`.
- [ ] ADR-058 amendment lands in the same PR: §"Scope" clarifies
      gateway-egress only; §"Out of scope" adds east-west lateral
      flows.
- [ ] Live smoke: `nc -U ~/.mvm/audit/gateway-<instance>.sock` shows
      at least one `FlowOpened` + `FlowClosed` pair while a workload
      makes outbound HTTP.

## W6.B — real flow extraction

- [ ] Add `etherparse` to `crates/mvm-libkrun/Cargo.toml`.
- [ ] Implement Ethernet/IPv4/IPv6/TCP/UDP parsing in
      `gateway_bridge.rs`. Parser wrapped in `std::panic::catch_unwind`
      — on panic emit `GatewayAuditFault` and degrade that flow to
      pass-through splice.
- [ ] Bounded 5-tuple flow table (4096 active flows / instance) with
      oldest-idle eviction and `FlowEvicted` emission on overflow.
- [ ] Rate-limit `FlowOpened` to 1000/sec per instance; excess
      aggregated into `FlowFlood` summary events.
- [ ] Per-subscriber outbound queue (1024 events) with drop-oldest on
      full; emit `SubscriberLag` to the chain.
- [ ] New fuzz target
      `crates/mvm-libkrun/fuzz/fuzz_gateway_bridge.rs` with corpus
      seeded from real Ethernet traffic (passive captures or stored
      fixtures).
- [ ] Short `cargo-fuzz` run green on the new target.
- [ ] Real `bytes_sent` / `bytes_recv` counts in `FlowClosed` from
      parsed frame sizes.
- [ ] Performance microbench landed; documented per-VM throughput
      cost. Target: <5% throughput loss on the default workload mix.

Note: `FlowFlood`, `GatewayAuditFault`, `SubscriberLag`, and
`FlowEvicted` are likely new `LocalAuditKind` variants. PR #450 reserved
the core four (FlowOpened / FlowClosed / FlowBytes /
FlowPolicyDecision); these four guardrail kinds will need a small
follow-up to that schema or a sibling slice in W6.A.

## Security

The bridge introduces several attack surfaces that the implementation
must handle:

1. **Audit chain integrity.** Single writer per tenant — bridge tasks
   send over mpsc to the signer task; no concurrent signing.
2. **DoS via flow flooding.** Rate cap + FlowFlood aggregation +
   bounded flow table.
3. **DoS via slow subscriber.** Per-subscriber bounded queue,
   drop-oldest, SubscriberLag event. Subscriber never blocks splice.
4. **TOCTOU on socket bind.** Parent dir 0700. No auto-unlink — fail
   loud on `EADDRINUSE`; supervisor restart handles stale sockets
   explicitly.
5. **Parser fault containment.** `catch_unwind` + pre-merge fuzzing
   (claim 5 / `cargo-fuzz`).
6. **Privilege.** No new grant — bridge runs in the existing
   supervisor process which already handles the socketpair.

Threat model accepts (documented in the ADR-058 amendment):

- **Information disclosure via flow metadata.** Mode 0700 mitigates;
  multi-user shared hosts with the same UID are not supported.
- **Side channels via timing.** Inherent to any flow audit. Accepted.
- **Non-IP frame gap.** ARP / multicast / BPDU emit `FrameOther`
  events; full L2 pcap is out of scope.
- **East-west bypass.** Out of W6 scope; W11 candidate.

## Out of scope (W6)

- East-west microVM ↔ microVM audit (W11).
- L7 content inspection, secret detection, egress obfuscation (Plan
  103 — proposed, needs its own brainstorm and ADR).
- Structured per-protocol L2 visibility beyond 5-tuple (raw L2 pcap
  is a different tool).
- The 30s flow-bytes aggregation timer + `NetworkAuditConfig`
  per-tenant config (W8 territory).
- The `mvmctl audit traffic` CLI (W9 territory).
- The CI tamper gate that byte-flips a flow log (W10 territory).

## Verification

```sh
# Workspace gates
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace

# Live smoke (W6.A acceptance)
mvmctl up examples/python/hello-app/ --dev &
nc -U ~/.mvm/audit/gateway-<instance>.sock &
# Trigger outbound HTTP from the guest, observe FlowOpened/FlowClosed.

# Audit chain unbroken with new event kinds
mvmctl audit verify   # exit 0

# Fuzz (W6.B)
cd crates/mvm-libkrun/fuzz
cargo +nightly fuzz run fuzz_gateway_bridge -- -max_total_time=60
```

## Deferred follow-ups

(Track items that drop out of W6.A or W6.B during implementation
here. Per project convention, deferred items live in the same plan
doc as the slice that introduced them — not in PR descriptions.)

W6.A implementation tracker: [Plan 103](103-w6a-implementation-tracker.md).

### W6.A.5 — Vz Swift bridge + fd interception wire-up

Deferred from W6.A because Swift requires macOS + Vz framework
to compile-verify, and the Rust `configure_with_gateway` split
(needed for libkrun fd interception) is invasive enough to want
its own focused PR.

- [ ] `crates/mvm-vz-supervisor/Sources/mvm-vz-supervisor/Network.swift`
      — `makeGvproxyDevice` rewrite (socketpair + ingest connect
      with `MVM_VZ_BRIDGE_V1\n` handshake + `Task.detached` splice
      loops + NDJSON emit on first packet / shutdown)
- [ ] Swift XCTest harness (5 tests: shuffles_datagrams,
      emits_flow_opened, emits_flow_closed, handshake_sent_first,
      reconnect)
- [ ] `crates/mvm-backend/src/vz.rs:809-812` — populate `network`
      with `NetworkConfig::Gvproxy { events_ingest_socket_path:
      Some(...) }` sourced from `mvm_data_dir() + audit/`
- [ ] `crates/mvm-backend/src/libkrun.rs` — populate `SupervisorConfig`
      audit fields from `ExecutionPlan` (tenant_id) + `mvm_data_dir()`
- [ ] Split `configure_with_gateway` into `configure_pre_net` +
      bridge-socketpair-insertion in `crates/mvm-libkrun/src/lib.rs`
- [ ] Add `pub fn run_supervisor_with_bridge<F>(cfg, factory)` in
      mvm-libkrun, swap `mvm-libkrun-supervisor::main` from
      `run_supervisor` to `run_supervisor_with_bridge`
- [ ] Live smoke on all three backends (Linux+passt,
      macOS+libkrun+gvproxy, macOS+Vz+gvproxy)

### W6.B follow-ups (real flow extraction, next PR in this work stream)

- [ ] Flow flooding DoS: per-instance rate cap (1000/sec) +
      `FlowFlood` aggregation events for excess
- [ ] Parser fault containment: `std::panic::catch_unwind` around
      etherparse; emit `GatewayAuditFault` on panic and degrade
      that flow to pass-through splice
- [ ] Bounded flow table (4096 active flows / instance) with
      oldest-idle eviction and `FlowEvicted` emission on overflow
- [ ] Real per-direction byte counters in `FlowClosed` (from parsed
      frame sizes, not stub counts)
- [ ] Fuzz target `crates/mvm-libkrun/fuzz/fuzz_gateway_bridge.rs`
      seeded from real Ethernet captures
- [ ] Rust→Swift drop command channel for Vz mediation actuation
      (per-flow drop directives from `FlowPolicy` evaluator to
      Swift bridge)
- [ ] Broadcast/ARP/DHCP noise distinguishing — emit `FrameOther`
      for non-IP frames; drop from main per-flow event stream
- [ ] Spurious vfkit-handshake `FlowOpened` on passt ingress —
      passt's first inbound bytes are vfkit framing, not guest
      traffic; parser-aware fix
- [ ] `signer_task_sole_writer_under_concurrent_events` direct
      regression test (currently covered end-to-end by passt
      bridge test)
- [ ] `bridge_panic_exits_process_nonzero` subprocess integration
      test (W6.A's `catch_unwind` → `exit(1)` plumbing is in place
      but not tested via subprocess)
- [ ] `gateway_child_does_not_inherit_audit_socket_fds` CLOEXEC
      regression test

### Adjacent new plans (separate work streams)

- [ ] Move gvproxy spawn helper out of mvm-libkrun (mvm-providers
      or new mvm-gateways crate)
- [ ] **SNI inspector in gateway bridge** — new plan. Hostname-
      level allowlist without TLS MITM. Inspector extracts SNI
      from TLS ClientHello cleartext, populates
      `FlowDecisionCtx::sni_hostname`, lets a `FlowPolicy` impl
      allow/deny by hostname.
- [ ] **Plan 34 Phase 2: finalize TLS MITM in L7EgressProxy** —
      reactivate existing plan. Workload-trusted host CA per
      ADR-006; supervisor terminates TLS, inspects URL, decides,
      re-encrypts. Enables URL-path allowlist
      (`github.com/auser/mvm` vs `github.com/some-evil-repo`).
- [ ] **Plan 74 follow-up: mandatory-deny well-known DoH
      endpoints** (1.1.1.1:443, dns.google, etc.). Closes the
      DoH-bypass gap in admission-time DNS pinning.
- [ ] **gvproxy supply-chain hardening** — pin version+SHA in
      `mvmctl doctor`; vendoring evaluation. cargo deny / cargo
      audit don't cover Homebrew bottles.
- [ ] **mvmd-network-manager** cross-repo plan filed in
      `tinylabscom/mvmd/specs/plans/50-network-manager.md`
      (per-tenant gateway pool, egress quotas, tenant-level audit
      rollup, cross-tenant isolation policy).
