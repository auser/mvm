---
title: "Plan 34 — L7 egress proxy + DNS-answer pinning (plan 32 / Proposal D follow-up)"
status: Proposed
date: 2026-04-30
related: ADR-002 (microVM security posture); ADR-004 (hypervisor egress policy); plan 32 (MCP + LLM-agent adoption); plan 25 (microVM hardening)
---

## Status

Proposed. Plan 32 / Proposal D shipped the L3 tier (`NetworkPreset::Agent`
+ existing `apply_network_policy` iptables machinery). ADR-004 named
the L7 HTTPS-proxy + DNS-pinning layers as the next stage and
documented why they're deferred. This plan turns those deferrals into
concrete implementation work.

## Why

ADR-004 §"The three-layer model" makes the case explicit: the L3
allowlist catches raw-IP exfil but not DNS rotation (CDN-fronted
hosts where the authorised answer changes between rule-install and
connect) or SNI/Host-header abuse over a permitted destination.
For LLM-agent workloads (`nix/images/examples/llm-agent/`'s
`claude-code-vm`) talking to high-stakes endpoints like
`api.anthropic.com`, those gaps matter — an exfil that uses
DNS-rotation tricks to reach a CDN-fronted host the L3 tier
authorised yesterday is exactly the kind of attack the LLM-agent
posture is supposed to defend against.

Plan 32 / `feat/egress-l7-proxy` shipped the foundation:

- `mvm_core::policy::network_policy::EgressMode` enum
  (`Open` / `L3Only` / `L3PlusL7`).
- `mvm_runtime::vm::egress_proxy` module with the `EgressProxy`
  trait, `ProxyHandle`, `EgressProxyError` (with a
  `NotImplemented` variant pointing here), and a `StubEgressProxy`.
- 4 unit tests cover the stub's "not implemented" surface.

This plan implements the runtime backing.

## Threat model (additive over ADR-004)

L7 closes two gaps in L3:

1. **DNS rotation.** The L3 ruleset resolves the allowlist's
   hostnames once at TAP attach. A guest that DNS-resolves
   `cdn.example.com` later may get a different IP than what was
   authorised — the L3 ruleset still says "drop" because that IP
   isn't in the table. The L7 proxy enforces by SNI/Host instead of
   IP, so the destination's IP can rotate freely.
2. **SNI/Host abuse.** A guest that opens TLS to a permitted SNI
   then sends an HTTP/1.1 `Host: evil.example.com` header is
   blocked at L7 (the proxy rejects the Host) but invisible to L3.

L7 does NOT close:

- **TLS-pinning bypass.** A client that ignores the proxy's CA
  cert (e.g. statically-pinned cert in a malicious binary) can't
  be intercepted; CONNECT to a permitted SNI lets the bytes
  through unchanged. L7 catches "agent uses default trust store"
  cases; not "malicious binary ships its own roots."
- **Tunnels over allowed protocols.** Anything HTTPS-shaped to a
  permitted destination is permitted. WebSocket/HTTP/2 to
  api.anthropic.com is fine; if the agent is malicious-by-design
  and uses Anthropic as a relay, that's outside this plan.
- **Non-HTTP egress.** Raw TCP / UDP to permitted ports falls
  back to the L3 layer.

## Implementation

### Tier 1 — `mitmdump` per-VM supervisor

**File:** `crates/mvm-runtime/src/vm/egress_proxy.rs` (extend the
stub).

`MitmdumpSupervisor` implements `EgressProxy`:

- `start_for_vm(vm_name, policy) -> ProxyHandle`:
  - Allocate a free TCP port from `MVM_EGRESS_PROXY_PORT_BASE`
    (default 18000) onwards. Port allocator lives in a
    `Mutex<BTreeMap<u16, String>>` shared across calls so
    concurrent VMs don't collide.
  - Generate a per-VM `mitmdump` filter script that allows
    SNI/Host matches against the policy's allowlist and rejects
    everything else with HTTP 403. Script lives in
    `~/.mvm/egress/<vm_name>/filter.py`.
  - Spawn `mitmdump --mode regular@<port> -s <filter.py>
    --set block_global=true` as a child process owned by the
    supervisor.
  - Wait for the proxy to start listening (TCP probe with a 3 s
    timeout). Fail with `EgressProxyError::Other` if not up.
  - Install iptables `nat OUTPUT/PREROUTING` rules that REDIRECT
    the guest's `:443` and `:80` traffic to the proxy port.
    Rules live in `MVMEGRESS-L7-<vm_name>` chain; cleanup
    flushes that chain.
  - Return `ProxyHandle { vm_name, listen_port }`.

- `stop_for_vm(handle)`:
  - Flush `MVMEGRESS-L7-<vm_name>` chain.
  - Send SIGTERM to the child process; wait up to 3 s; SIGKILL.
  - Remove `~/.mvm/egress/<vm_name>/`.
  - Free the port allocation.

**Cross-platform:** matches existing `apply_network_policy`
pattern. The supervisor dispatches through `shell::run_in_vm` on
macOS (Lima) and runs natively on Linux.

### Tier 2 — Per-host CA cert

**File:** `crates/mvm-cli/src/commands/ops/egress.rs` (new).

`mvmctl egress init-ca` generates an ed25519 CA at
`~/.mvm/egress/ca.pem` + `ca.key` (owner-readable only, mode 0400),
valid for 5 years. `mvmctl doctor` reports its presence/expiry.

`mitmdump` is configured to use this CA via `--set
ca_certs=~/.mvm/egress/ca.pem`. The same cert path is mounted
read-only into every guest at `/etc/ssl/certs/mvm-egress.crt` via
mvm-runtime's existing config-files plumbing (no new `secret_files`
needed — cert is non-sensitive once distributed).

Guest-side rootfs init (in `nix/lib/minimal-init/`) copies
`/etc/ssl/certs/mvm-egress.crt` into the system trust store at boot
when the file is present.

### Tier 3 — DNS-answer pinning (optional, per-policy)

**File:** `crates/mvm-runtime/src/vm/dns_pin.rs` (new).

`dnsmasq` stub resolver on the host (or inside Lima on macOS) bound
to a private CIDR the guest reaches. The stub:

- Accepts only allowlisted domains; SERVFAIL for everything else.
- Forwards permitted queries upstream.
- Hooks into the iptables `MVMEGRESS-L3-<vm>` chain so resolved IPs
  are pinned with their TTL — when the TTL expires, the pin
  re-resolves and updates the chain.

Optional because L7 catches most of DNS rotation's attack value
(SNI is checked at the proxy, not by IP). Operators who want the
extra defence enable it via
`network_policy.dns_pinning = true` (new field, defaults false).

### Tier 4 — Wire it up

**Files:**
- `crates/mvm-runtime/src/vm/network.rs` — `apply_network_policy`
  takes an `EgressMode` parameter and a `&dyn EgressProxy`. When
  `EgressMode::L3PlusL7`, it calls `proxy.start_for_vm` after the
  L3 rules. `cleanup_network_policy` calls `proxy.stop_for_vm`.
- `crates/mvm-cli/src/commands/vm/up.rs` — new flag
  `--egress-mode <open|l3-only|l3-plus-l7>` (default depends on
  policy: unrestricted → open, allowlist → l3-only, agent preset
  → l3-plus-l7 if CA cert is initialised, else l3-only with a
  warning).
- `crates/mvm-cli/src/commands/env/doctor.rs` — probe for
  mitmdump on PATH (or in nixpkgs closure), CA cert presence/
  expiry, dnsmasq if dns-pinning is on.

### Tier 5 — Tests

- Unit: port allocator (concurrency), filter-script generation,
  iptables-rule generation. All pure code — runs in CI.
- Integration (live KVM): boot `claude-code-vm` with
  `--egress-mode l3-plus-l7`, `curl https://api.anthropic.com/`
  → 200, `curl https://google.com/` → 403 from proxy with
  "egress: domain not on allowlist" body. Live-KVM only.
- Cleanup-on-crash: SIGKILL the mvmctl parent, verify the
  supervisor's child mitmdump dies (process group), verify
  iptables rules are flushed by `mvmctl cache prune`.

### Tier 6 — `mvmctl cache prune` orphan cleanup

**File:** `crates/mvm-cli/src/commands/ops/cache.rs`.

Walk `~/.mvm/egress/<vm_name>/` directories whose `vm_name` isn't
in the running VM list; flush the corresponding iptables chain;
remove the directory.

### Tier 7 — `nix/images/examples/llm-agent/` README

Recommend `--egress-mode l3-plus-l7` once the CA is set up. Show
the L3-only fallback as the alternative for users who don't want
to inject the CA.

## Cross-cutting considerations (must-fold into tiers before merge)

These twelve gaps surfaced on a re-read after PRs #20–#26 landed.
Grouped by tier; each item names the tier it amends. The sequence
in §"Sequence" assumes they're folded.

### Security/correctness (must do)

- **CA private-key handling (Tier 2).** `~/.mvm/egress/ca.key` mode
  `0400` is necessary but not sufficient — *every* process running as
  the host user can read it. Decision: bind the key to `mvmctl egress
  init-ca` only; mitmdump is handed *per-VM signed leaf certs* with
  X.509 `nameConstraints permitted` set to that VM's allowlist.
  Documented as ADR-005-pending. Keeps the local-user-trusts-the-host
  model (consistent with ADR-002 §"out of scope: malicious host")
  while eliminating the "any local malware exfils through the trusted
  CA" path.

- **CA cert is sensitive once it's in a guest (Tier 2).** Earlier
  draft said "non-sensitive once distributed." That's wrong — a guest
  with the CA can MITM any TLS endpoint. Per-VM Name-Constrained CAs
  (above) cap the blast radius: claude-code-vm's CA can only sign for
  `*.anthropic.com` / `*.openai.com` / `*.github.com`. Even a malicious
  agent that exfils its CA can't use it to MITM other destinations.

- **Connection-by-IP bypass (Tier 1, Tier 4).** mitmdump only sees
  TCP redirected by `nat` rules. A guest that opens raw TCP to
  `8.8.8.8:53` for tunnelling, or any port the REDIRECT doesn't
  cover, slips through. The L3 layer's existing `MVMEGRESS-L3-<vm>`
  chain DROPs everything not explicitly allowed; chain-ordering
  matters. The wire-up MUST install L3 chain *first*, then layer L7
  REDIRECT only on `:443` and `:80`. Add a regression test that
  asserts `connect(8.8.8.8, 53)` from inside the guest fails when
  mode is `L3PlusL7`.

- **HTTP/2 vs HTTP/3 (Tier 1, Tier 4).** mitmdump intercepts HTTP/1
  + HTTP/2 over TLS via CONNECT. HTTP/3 (QUIC over UDP/443)
  bypasses it entirely. Easy fix: DROP UDP/443 from guest CIDR by
  default in the `MVMEGRESS-L3-<vm>` chain — clients fall back to
  HTTP/2. Document; revisit if a guest needs HTTP/3 specifically.

- **Cleanup on crash, tier 1 specifics (Tier 1).** Plan said "process
  group" but `MitmdumpSupervisor` runs from an mvm-runtime call,
  which isn't a session leader. On Linux, set
  `prctl(PR_SET_PDEATHSIG, SIGKILL)` on the spawned mitmdump child so
  a parent crash doesn't orphan it. macOS has no PR_SET_PDEATHSIG —
  rely on `mvmctl cache prune`'s orphan-cleanup pass (Tier 6) plus a
  watchdog that polls `kill(parent_pid, 0)` once per minute. Both
  paths must be wired; cross-platform-discipline note from
  `apply_network_policy` already covers Lima dispatch.

- **Concurrent-port-allocator races (Tier 1).** `Mutex<BTreeMap<u16,
  String>>` is necessary but racy: between "allocate port N" and
  "mitmdump binds N", another process can grab N. Implement a
  bind-then-commit loop: try `bind(0)` (kernel picks), commit the
  bound port to the map, retry-on-EADDRINUSE only if the kernel
  declines a specific request. Cap retries at 8.

### UX / interop (should do)

- **CA distribution mechanism (Tier 2, Tier 4) — be specific.** Plan
  said "mounted via existing config-files plumbing." Concrete pick:
  use `--secret-file` (mode 0400 owned by service uid) for the
  per-VM leaf cert, `/run/mvm-egress/ca.crt` mode 0444 for the host
  CA bundle (non-sensitive once name-constrained). Wire both
  through `mvmctl up` and `mvmctl exec`. Plan 32 / Proposal B's
  llm-agent flake gains an `egress` block in the secrets config.

- **`mvmctl egress init-ca` idempotency + rotation (Tier 2).** Second
  run refuses unless `--force` is passed. New verb `mvmctl egress
  rotate-ca` does the right thing: regenerate root, invalidate all
  template caches that embedded the old root, log the rotation to
  `~/.mvm/log/audit.jsonl` with kind `EgressCaRotated`. Document the
  rotation as a "every 90 days, automated via routine" recommendation
  in the README.

- **`L3PlusL7` default selection (Tier 4) — make precedence explicit.**
  The mode that runs is resolved in this order:
  1. Explicit `--egress-mode` CLI flag (highest precedence).
  2. Template's baked policy (PR #25 `default_network_policy`).
  3. CA-present heuristic: if `mvmctl egress init-ca` has been run
     and the policy preset is `agent`, default to L3+L7.
  4. Fallback to `L3Only` (preserves L3 behaviour from PR #20).
  Step 3 emits a stderr warning the first time it auto-selects so
  operators see it. `--no-warn-no-ca` lint mode for CI.

- **Per-template `default_network_policy` interaction (Tier 4).**
  PR #25 shipped `Option<NetworkPolicy>` on TemplateSpec but no
  `egress_mode` field. Decision: extend `NetworkPolicy::Preset` /
  `AllowList` with an optional `egress_mode: Option<EgressMode>`
  *enrichment*, NOT a sibling spec field. This way `mvmctl template
  create --network-preset agent --egress-mode l3-plus-l7` bakes both
  in one place, and the precedence order above stays clean.

### Plumbing (nice to have)

- **DNS pinning skeleton without dnsmasq (Tier 3).** A pure-Rust DNS
  stub (rebind port 53 inside Lima, parse DNS packets, allowlist by
  question, forward upstream) is ~300 LoC and removes a runtime dep.
  Worth weighing once Tier 1 + Tier 2 land — the dnsmasq path is the
  safer first cut, but pure-Rust avoids a CVE-bearing C dependency.
  File as plan-34 §"Optional Tier 3.5" once Tier 1 ships.

- **Metrics integration (Tier 1).** mvm has Prometheus metrics. The
  L7 supervisor exports per-VM allowed/denied counts: counter
  `mvm_egress_proxy_requests_total{vm, host, action="allowed|denied"}`,
  histogram `mvm_egress_proxy_handshake_seconds`, gauge
  `mvm_egress_proxy_active_per_vm`. Operators see "claude-code-vm
  tried 47 connects to api.openai.com (allowed) and 3 to
  evil.example.com (blocked)" in their dashboards. Land in the
  existing `mvmctl metrics` registry — no new prom endpoint needed.

## Files (summary)

| File | Change |
|---|---|
| `crates/mvm-runtime/src/vm/egress_proxy.rs` | Replace `StubEgressProxy` with `MitmdumpSupervisor`; add `prctl(PR_SET_PDEATHSIG)` on spawn (Linux) + watchdog poll (macOS); bind-then-commit port allocator; Prometheus metrics (`mvm_egress_proxy_*`) |
| `crates/mvm-runtime/src/vm/dns_pin.rs` | new — dnsmasq supervisor |
| `crates/mvm-runtime/src/vm/network.rs` | wire `apply_network_policy` to call the proxy; assert L3-then-L7 chain ordering; DROP UDP/443 from guest CIDR |
| `crates/mvm-core/src/policy/network_policy.rs` | extend `NetworkPolicy::Preset` / `AllowList` with `egress_mode: Option<EgressMode>` enrichment (not a sibling field) |
| `crates/mvm-cli/src/commands/ops/egress.rs` | new — `mvmctl egress init-ca` (idempotent, refuses without `--force`); `mvmctl egress rotate-ca` |
| `crates/mvm-core/src/policy/audit.rs` | new audit kind `EgressCaRotated` |
| `crates/mvm-cli/src/commands/vm/up.rs` | new `--egress-mode` flag with documented 4-step precedence |
| `crates/mvm-cli/src/commands/env/doctor.rs` | report mitmdump availability + CA cert presence/expiry |
| `crates/mvm-cli/src/commands/ops/cache.rs` | orphan-cleanup pass + macOS watchdog fallback |
| `nix/lib/minimal-init/lib/04-etc-and-users.sh.in` | install per-VM leaf cert (mode 0400, service uid) at `/run/secrets/mvm-egress.crt` and host CA bundle at `/etc/ssl/certs/mvm-egress-ca.crt` |
| `nix/images/examples/llm-agent/README.md` | recommend `--egress-mode l3-plus-l7` + 90-day rotation routine |
| `specs/adrs/005-name-constrained-egress-ca.md` | new ADR — CA private-key handling + per-VM Name-Constrained leaf certs |

## Sequence

Roughly 1.5 sprints once the considerations are folded:

- Day 1: ADR-005 (Name-Constrained CA design) — locks the
  cryptographic story before code starts.
- Day 2-3: port allocator (bind-then-commit) + mitmdump supervisor +
  filter-script generation + PDEATHSIG/watchdog. Unit tests for the
  port-race path and L3-then-L7 chain ordering.
- Day 4: CA-cert tooling (`mvmctl egress init-ca` /
  `rotate-ca`, doctor probe, Name-Constrained leaf signing for
  per-VM dispatch).
- Day 5: `NetworkPolicy::Preset.egress_mode` enrichment + 4-step
  precedence in `up.rs`. Live KVM smoke against `claude-code-vm`
  asserting `connect(8.8.8.8:53)` blocked and the
  `mvm_egress_proxy_requests_total{action="denied"}` counter
  increments.
- Day 6: `mvmctl cache prune` orphan handling. macOS watchdog.
  README updates with rotation routine.
- Day 7-8 (stretch): DNS pinning via dnsmasq, then plan-34 §"Optional
  Tier 3.5" — pure-Rust DNS stub if dnsmasq's surface proves
  problematic.

## Reversal cost

- Drop `MitmdumpSupervisor` → fall back to `StubEgressProxy` (current
  state of plan 34's foundation branch). The schema change
  (`EgressMode::L3PlusL7`) stays — clients setting it just get the
  `NotImplemented` error.
- `mvmctl egress init-ca` is a separate subcommand; removing it is
  contained.
- DNS-pinning (tier 3) is independent of the L7 proxy; can land
  separately or roll back without affecting L3 / L7.

## References

- ADR-004: `specs/adrs/004-hypervisor-egress-policy.md`
- ADR-002: `specs/adrs/002-microvm-security-posture.md`
- Plan 32: `specs/plans/32-mcp-agent-adoption.md`
- Inspiration: archie-judd/agent-sandbox.nix's domain-allowlist
  proxy, mitmproxy upstream docs.
