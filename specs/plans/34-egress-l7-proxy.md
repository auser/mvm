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

## Files (summary)

| File | Change |
|---|---|
| `crates/mvm-runtime/src/vm/egress_proxy.rs` | Replace `StubEgressProxy` with `MitmdumpSupervisor` |
| `crates/mvm-runtime/src/vm/dns_pin.rs` | new — dnsmasq supervisor |
| `crates/mvm-runtime/src/vm/network.rs` | wire `apply_network_policy` to call the proxy |
| `crates/mvm-cli/src/commands/ops/egress.rs` | new — `mvmctl egress init-ca` |
| `crates/mvm-cli/src/commands/vm/up.rs` | new `--egress-mode` flag |
| `crates/mvm-cli/src/commands/env/doctor.rs` | report mitmdump availability |
| `crates/mvm-cli/src/commands/ops/cache.rs` | orphan-cleanup pass |
| `nix/lib/minimal-init/lib/04-etc-and-users.sh.in` | install CA cert at boot |
| `nix/images/examples/llm-agent/README.md` | recommend `--egress-mode l3-plus-l7` |

## Sequence

Roughly one sprint:

- Day 1-2: port allocator + mitmdump supervisor + filter-script
  generation. Unit tests.
- Day 3: CA-cert tooling (`mvmctl egress init-ca`, doctor probe).
- Day 4: wire-up in `apply_network_policy` + `up.rs` flag. Live
  KVM smoke against `claude-code-vm`.
- Day 5: `mvmctl cache prune` orphan handling. README updates.
- Day 6 (stretch): DNS pinning via dnsmasq.

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
