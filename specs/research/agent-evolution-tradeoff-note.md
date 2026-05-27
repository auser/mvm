# Agent Evolution Tradeoff Note: Zig vs Lean Rust v2 for the guest control-layer

> Research document — no implementation changes in this note itself.
> Track E §"Zig evaluation gates" in [Sprint 42](../SPRINT.md) requires a written tradeoff note before any Zig prototype.
> This document satisfies that gate by analyzing **both** the Zig path and the lean-Rust v2 path symmetrically.
> Implementing plan: [Plan 105 — Guest control-layer dep-reduction + encryption design](../plans/105-zig-pid0-exploration.md).
> Date: 2026-05-27.

---

## Context

The guest-side control-layer in mvm is currently Rust + a heavy transitive dependency tree (`tokio`, `serde_json`, `ed25519-dalek`, `rtnetlink`, `seccompiler`, hundreds of crates downstream). Track E in Sprint 42 set the discipline that any Zig evaluation must be preceded by this kind of written tradeoff. Plan 105 turns that gate into a concrete A/B evaluation between Zig and a "lean Rust v2" alternative.

**Recommendation up front (from Plan 105 §"Systems-design recommendation"):**

> Stay Rust. Adopt **lean Rust v2** as the agent's evolution path. Treat the Zig prototype as a measurement check, not a direction.

This note's job is to lay out the evidence behind that recommendation, and to define the rubric the W2/W2′ prototypes must fill in before the final decision is made.

---

## 1. Current Rust pid0 dep footprint per binary

The "pid0-class" boundary binaries — anything running in the guest's blast radius at boot or as part of the control plane — and their current Rust dependency profiles. Crate paths cited so they're easy to verify.

| Binary | Crate path | LoC (Rust) | Key deps | Boundary role |
|---|---|---|---|---|
| `mvm-verity-init` | `crates/mvm-guest/src/bin/mvm-verity-init.rs` | 833 | `libc` only | PID 1 in verity initramfs; dm-verity setup; `switch_root` |
| `mvm-guest-netinit` | `crates/mvm-guest/src/bin/mvm-guest-netinit.rs` (+ `netinit.rs`, 100 LoC support) | 87 | `tokio`, `rtnetlink`, `netlink-packet-route` | One-shot blackhole-route installer, pre-agent |
| `mvm-guest-agent` | `crates/mvm-guest/src/bin/mvm-guest-agent.rs` | 1,409+ | `tokio`, `serde_json`, `ed25519-dalek` | uid-901 control-plane endpoint, vsock :5252, 33 `GuestRequest` variants |
| `mvm-builder-agent` | `crates/mvm-guest/src/bin/mvm-builder-agent.rs` | 447 | `tokio`, `serde_json` | Builder-VM control plane |
| `mvm-builder-init` | `crates/mvm-builder-init/` | — | `nix` 0.29, `libc` | PID 1 in builder VM; mount/reboot |
| `mvm-seccomp-apply` | `crates/mvm-guest/src/bin/mvm-seccomp-apply.rs` | — | `seccompiler`, `libc` | BPF emitter for per-service seccomp |
| `mvm-egress-proxy` | `crates/mvm-egress-proxy/` | — | `libc` only (no tokio!) | HTTP CONNECT relay in builder VM |
| `mvm-addon-dns` | `crates/mvm-addon-dns/` | — | `hickory-server`, `hickory-proto`, `tokio` | In-VM DNS resolver |
| `mvm-addon-vsock-bridge` | `crates/mvm-addon-vsock-bridge/` | — | minimal | TCP loopback → vsock peer bridge |

**Notable**: `mvm-egress-proxy` already demonstrates the "lean Rust" discipline — `libc` only, no async runtime, no tokio. This is the existence proof that the lean-Rust v2 path is realistic, not theoretical.

### Direct dependency families pulled into the guest boundary

| Family | Workspace pin | Transitive reach (rough) | Used by | Notes |
|---|---|---|---|---|
| `tokio` | 1.x | ~25 crates | mvm-guest, mvm-guest-netinit, mvm-addon-dns, mvm-builder-agent | Largest single boundary cost. Features are narrowed but the runtime is unavoidable today. |
| `serde` + `serde_json` + `serde_derive` | latest | ~10 crates + proc-macro chain | every wire type | `#[serde(deny_unknown_fields)]` is load-bearing for claim 5. |
| `ed25519-dalek` | latest | ~6 crates (`curve25519-dalek`, `rand_core`, `signature`, `subtle`, `zeroize`) | mvm-guest, mvm-core | Already comparatively small. |
| `rtnetlink` + `netlink-packet-route` | latest | ~5 crates | mvm-guest-netinit only | Used for 87 LoC of one-shot route installation — heavy ratio. |
| `seccompiler` | 0.5 | small | mvm-seccomp-apply | No reasonable alternative; stays. |
| `libc` | latest | 1 crate | every boundary binary | Lowest-level FFI; stays in every path. |

Full per-crate audit is in [Track E of Sprint 42](../SPRINT.md#track-e--zig-evaluation-gates). This note focuses on what an agent-evolution path would replace, not the full workspace.

### Vsock surface — broader than the agent control channel (Plan 104 / ADR-059 context)

Plan 104 (host services broker over vsock, merged 2026-05-26) and the in-flight ADR-059 broaden the vsock surface from a single host↔agent control channel to **three named ports**, all sharing the same `AuthenticatedFrame` envelope. Any agent-evolution decision must account for the full set:

| Vsock port | Host-side endpoint | Guest-side caller | Wire envelope | Purpose |
|---|---|---|---|---|
| **5252** | `mvm-supervisor` agent dispatcher | `mvm-guest-agent` (uid 901, listener in guest) | `AuthenticatedFrame` → `GuestRequest`/`GuestResponse` | Agent control — the 33-variant RPC surface (RunEntrypoint, FilesystemRpc, ProcessRpc, ConsoleOpen, MountVolume, …) |
| **5300** | `mvm-supervisor` in-process broker | mvm SDK in workload (or agent, for built-in services) | `AuthenticatedFrame` → `ServiceCall`/`ServiceResponse` | General broker: `host.time.v1`, `host.cost.v1`, `broker.v1` (observational/low-criticality) |
| **5301** | `mvm-secrets-dispatcher` subprocess (uid 902, seccomp-locked, separate address space from the supervisor) | mvm SDK in workload | `AuthenticatedFrame` → `ServiceCall`/`ServiceResponse` | Secrets channel: `host.secrets.v1` only — out-of-process for blast-radius isolation per Plan 104 §"Host-side: two-process architecture" |
| (53 — legacy) | supervisor in-process | guest | `HostBoundRequest` | Being deprecated by Plan 104; replaced by the broker on :5300 |

**Implications for Plan 105:**

- **W3 (Noise_NK encryption design)** must cover **all three guest↔host vsock connections** (5252, 5300, 5301), not just the agent control channel. The same host static Ed25519 pubkey (already baked in via `mkGuest { hostPubkey = ...; }`) authenticates all three. Each port runs its own session — three independent Noise_NK handshakes, same key material.
- **W4 ADR "control-plane vs data-plane"** must name the broker explicitly as part of the control plane. Workload→host service calls (secrets, time, cost) are control-plane traffic — they cross the boundary, carry audit-emitting RPCs, and gate on the signed `ExecutionPlan.services` bindings. They are **not** data plane (D3) — the data plane is workload bytes (stdout, network, fs); broker calls are workload→host *service* invocations.
- **W4 ADR "pid0 portability boundary"** stays about the guest *agent*; the broker doesn't move the pid0 contract, but the boundary doc should cross-reference Plan 104 to make clear pid0 isn't the only control surface a backend has to support.
- **W5 capability matrix** column "vsock-encryption support" must be evaluated against the *multi-port* surface — a backend that supports vsock at all supports all three ports (Firecracker, libkrun, Apple VZ, Cloud Hypervisor), so this is uniform — but the matrix should note the breadth.
- **I1 (control-plane audit chain)** spans the broker too. Plan 104 funnels all broker calls (including the out-of-process secrets dispatcher's calls) through the supervisor's `AuditEmitter` so the chain stays linear. Any agent change must preserve this contract — but the broker work has already done the audit integration; W1 inherits it.
- **Process-supervision question is bigger than the agent.** `mvm-secrets-dispatcher` is host-side (uid 902 host subprocess, not a guest binary), so it's *not* in our pid0 scope. But Plan 104's discipline ("the broker's general dispatcher and the secrets dispatcher share zero code paths and zero address space") is the same architecture pattern the agent itself could adopt (split monolith → core dispatcher + composable handlers, possibly out-of-process for the most security-critical verbs). The lean Rust v2 agent-v2 follow-on should learn from the broker's two-process design.

**What this does NOT change in the recommendation:**

- The Zig vs lean Rust v2 question for the **netinit prototype** is unchanged — netinit doesn't speak the broker at all.
- The dep-reduction story for the **agent** is unchanged — the broker is host-side; the agent's tokio/serde_json footprint is what we're measuring.
- The encryption design (W3) is unchanged in *shape* — Noise_NK still fits; it now applies to three ports instead of one. Lean Rust v2 with `snow` handles three ports as easily as one; Zig from-scratch implementation gets three handshake state machines (small additional cost).

### Existing audit/hardening posture on the boundary

- **`deny.toml`** (`./deny.toml`, lines 1-139): strict advisory policy, six audited exemptions, license allowlist limited to MIT / Apache-2.0 / BSD / ISC / MPL family. CI `deny` + `audit` jobs run per-PR (claim 7).
- **Fuzz harnesses** in `crates/mvm-guest/fuzz/fuzz_targets/`: `GuestRequest`, `AuthenticatedFrame`, `BuilderRequest`, `EntrypointEvent`, `AuthedPath`. Plus `crates/mvm-libkrun/fuzz/`, `crates/mvm-vz/fuzz/`, `crates/mvm-oci/fuzz/`.
- **Prod-build symbol gate**: `prod-agent-no-exec` + `prod-agent-runentry-contract` CI lanes assert `do_exec` and friends are absent from sealed-prod agent builds (claim 4).
- **Reproducible builds**: double-build verification across CI (claim 7).

Any agent-evolution path — Zig or lean-Rust v2 — must preserve all of this. The lean-Rust path inherits it for free (same toolchain); the Zig path has to reproduce each invariant in Zig's tooling.

---

## 2. Zig path analysis

### What Zig would and would not displace

| Boundary cost | Replaced by Zig? | Notes |
|---|---|---|
| `tokio` async runtime | **Yes** | Drop entirely. Use epoll directly via Zig syscalls or build a small per-binary event loop. |
| `serde_json` parser | **Yes** | `std.json.parseFromSlice` in Zig stdlib. Schema mirroring vs Rust is the cost (D2: Rust still defines the conversation). |
| `serde_derive` proc-macro chain | **Yes** | Zig has comptime; no proc-macro chain to amortize. |
| `ed25519-dalek` + `curve25519-dalek` | **Yes** | Zig stdlib `std.crypto.sign.Ed25519` covers the signing path. Constant-time properties are documented. |
| `rtnetlink` + `netlink-packet-route` | **Yes** | Hand-rolled netlink in ~150 LoC of Zig over `std.posix.socket`. |
| `seccompiler` BPF emitter | **Yes-but** | Workable in Zig but mvm-seccomp-apply is small (~100s of LoC) and the BPF rules are the load-bearing surface, not the emitter. Marginal win. |
| `libc` | **No** (kept via `@cImport`) | Zig wraps libc cleanly; no displacement. |
| The 33 `GuestRequest` variants and their handlers | **No** | Logic must be re-implemented in Zig; quantity is the same, language is the variable. |
| Process supervisor (fork/exec/wait/PTY) | **No** (must reimplement) | `std.posix.fork`, `std.posix.execve`, `std.posix.waitpid` + manual PTY allocation. Roughly the same complexity as Rust today. |

### Toolchain cost

- **CI compiler**: Zig isn't in any current mvm CI lane. Adding it: pin a version (Zig is still pre-1.0 — 0.13 / 0.14 series), cache toolchain artifacts, cross-compile musl-static aarch64 + x86_64.
- **Reproducibility**: Zig's hermetic build cache hashes inputs; should hit claim 7's byte-identical rebuild bar, but unproven in this repo. W2 measurement #9 settles this empirically.
- **Fuzzer integration**: `afl++` works with Zig via the standard instrumentation flow. `libFuzzer` linkage is unclear at Zig 0.13. The existing Rust fuzz infrastructure uses `cargo-fuzz` (libFuzzer-based); a Zig-side fuzz harness would be a parallel pipeline. Cost: one new GHA workflow + corpus management.
- **`cargo-deny` analog**: Zig has no first-class supply-chain audit tool. The Zig stdlib + project source are the entire dep tree (no external packages by convention for a leaf binary), so the supply-chain question shifts from "audit transitive crates" to "audit Zig stdlib + Zig compiler." Both are large; both are bring-your-own-trust.
- **Debugger workflow**: lldb works with Zig but the experience differs from rustc. Stack-trace quality and panic information are good in Zig but different.

### Auditability comparison (LoC and surface)

For a faithful port of `mvm-guest-netinit` (the W2 target):

| Property | Current Rust | Hypothetical Zig |
|---|---|---|
| Binary's own source LoC | 87 + ~100 (netinit.rs support) | ~200 (estimate, faithful port) |
| Transitive Rust crates in build | 30-40 (tokio + rtnetlink + netlink-packet-route + their deps) | 0 |
| Lines of Rust code in transitive deps (rough) | ~50,000+ | 0 |
| Zig stdlib modules touched | n/a | ~5-10 (std.posix, std.os.linux, std.crypto, std.json, std.heap, std.mem) |
| Lines of Zig stdlib in surface | n/a | ~15,000-25,000 (rough; Zig stdlib for those modules) |
| Compiler-trust surface | rustc + LLVM | Zig compiler (self-hosted now) + LLVM |

**Honest read**: the Zig surface isn't *zero* supply-chain risk — it's a *different* supply chain. The argument for Zig is that the surface is smaller, more uniform, and inspectable from one repo. The argument against is that it shifts trust to a younger compiler with a smaller user base.

### Risk reduction

What Zig shrinks:
- Proc-macro execution at build time (Rust derive chain) — gone.
- `build.rs`-style arbitrary build-time code execution (we have some via `bindgen` gated; Zig has comptime which is *different* but also code-at-build-time).
- Transitive yanked-crate exposure (`deny.toml` line 14: `yanked = "deny"`) — gone since there are no transitive crates.

What Zig adds:
- A new compiler in the trust chain.
- A new stdlib whose audit status varies by module.
- No `cargo-audit` workflow — manual review of every stdlib bump.

### Maintenance impact

- **Contributor pool**: the team is Rust-first. Each Zig PR is a contributor-pool tax — fewer people who can write or review. mvm has ~10 Rust crates today, each with active maintainers; adding Zig means one more language-specific on-call rotation skill.
- **Code review skill**: the team would need to develop Zig idioms, memory-management patterns (Zig's allocator passing is explicit and unfamiliar to Rust devs), comptime patterns.
- **Debugging**: at-scale debugging of a Zig agent in a libkrun guest at 3am requires expertise the team would have to build.

### Full-agent-rewrite cost estimate

If a future decision is "do `mvm-guest-agent` in Zig":

| Component | Rust LoC today | Zig LoC estimate | Notes |
|---|---|---|---|
| Core protocol parser + dispatcher | ~600 | ~2,500 | Hand-mirrored types for 33 variants |
| Async event loop | ~200 (via tokio) | ~500 | Epoll + state machine |
| Process supervisor (fork/exec/wait/PTY) | ~300 | ~400 | std.posix wrappers |
| Warm-process pool + integration probes | ~200 | ~500 | More verbose without async/await |
| Filesystem RPC + Process RPC + Port forwarding + Console PTY + Volume mount | ~400 | ~600 | Largely posix syscalls — similar in both langs |
| Vsock framing + auth | ~150 | ~200 | |
| Noise envelope (post-W3) | 0 today | ~800 | From-scratch Noise_NK in Zig |
| Tests + fuzz harnesses | ~400 | ~1,000 | More test scaffolding needed |
| **Total** | **~2,250 (just agent)** | **~6,500** | |

Wall-clock estimate: **6-12 weeks** of focused engineering for usable feature parity + audit-chain integration + CI lanes.

**This is the cost the team would pay to drop ~25 transitive crates** (`tokio` and friends). It's the strongest single reason this note recommends *not* doing the agent in Zig.

---

## 3. Lean Rust v2 path analysis

The dep-reduction goal can be met substantially within Rust. This section maps the swaps.

### Dep swaps

| Current dep | Lean Rust v2 replacement | Transitive crates dropped | Notes |
|---|---|---|---|
| `tokio` | `polling` (~1k LoC) + hand-rolled small executor (~300 LoC) | ~20 | `polling` is the smol-team's epoll/kqueue abstraction. Proven for one-shot binaries (mvm-egress-proxy is already in this discipline). |
| `serde_json` for the 33 GuestRequest variants | Hand-rolled per-variant parser, or `nanoserde` | ~8 | Hand-rolled preserves the explicit `deny_unknown_fields` semantics (claim 5). `nanoserde` is a one-crate alternative without a proc-macro chain. |
| `rtnetlink` + `netlink-packet-route` | `linux-raw-sys` + ~150 LoC of hand-rolled netlink | ~5 | Same syscalls as the Zig path, just in Rust. |
| `ed25519-dalek` (optional swap) | `ed25519-compact` (~1k LoC, no `rand_core`/`subtle`/`zeroize`) | ~4 (already small) | Optional; current `ed25519-dalek` is already comparatively lean. |
| (new) Noise_NK | `snow` (~1.5k LoC, audited Noise impl) | ~6 added | Not a dep cleanup — net new for W3 implementation. Cost paid for the encryption feature. |

**Estimated transitive-crate reduction**: ~30-40 crates dropped if all swaps land. Target: **50-70% reduction** of the boundary transitive set.

### What the agent rewrite costs in Rust

If a future decision is "do `mvm-guest-agent` in lean Rust v2":

| Component | Rust today | Lean Rust v2 estimate | Notes |
|---|---|---|---|
| Core protocol parser + dispatcher | ~600 | ~600 (mostly type-preserving) | Swap `serde_json` to `nanoserde` or hand-rolled |
| Async event loop | ~200 (via tokio) | ~500 (via `polling` + custom executor) | Real cost vs Zig: maybe smaller because Rust's type system helps |
| Process supervisor | ~300 | ~300 (refactored, separated from dispatcher) | Architectural cleanup, not a rewrite |
| Warm-pool + probes | ~200 | ~200 | Likely keeps `async`/`await` syntax — just on the new executor |
| FS/Process/Port/Console/Volume RPC | ~400 | ~400 | Same code, different deps |
| Vsock framing + auth | ~150 | ~150 | Unchanged |
| Noise envelope (post-W3) | 0 today | ~200 (`snow` wrapper) | Library does the work |
| Tests + fuzz harnesses | ~400 | ~400 | Unchanged (cargo-fuzz reuses) |
| **Total** | **~2,250** | **~2,750** | +500 LoC for executor + raw netlink |

Wall-clock estimate: **2-4 weeks** for the executor swap + dep cleanup + supervisor decoupling. Real risk: the executor at agent-scale (long-lived loop, 33 RPC variants, warm-pool state) is unproven.

### Auditability impact

- Drops ~50-70% of transitive crates → smaller `cargo audit` surface → fewer advisory exemptions over time.
- Drops the `tokio` macro layer (`#[tokio::main]`) — comparable to Zig's gain on proc-macros, except `serde_derive` is still around unless we also hand-roll JSON.
- `deny.toml` policy unchanged; advisory exposure (rsa, fxhash, etc.) likely drops as transitive trees shrink.

### Maintenance impact

- **Zero new toolchain.** No CI compiler swap, no second debugger, no second fuzzer pipeline.
- **Same contributor pool.** Reviewers already trained on Rust ownership / async / trait dispatch.
- **Same fuzz infrastructure** (`cargo-fuzz` keeps working).
- **Schema-first protocol**: type-preserving — `mvm-core` definitions stay canonical (D2 satisfied trivially).

---

## 4. Side-by-side comparison

| Dimension | Zig path | Lean Rust v2 path |
|---|---|---|
| **Transitive crate reduction at boundary** | ~100% of Rust-side transitive deps gone | 50-70% reduction |
| **Net new compiler-trust surface** | Yes (Zig compiler + stdlib) | No |
| **CI complexity delta** | Significant: new lane, new fuzz pipeline, new reproducibility verification | Negligible — same toolchain |
| **Audit tooling** | None equivalent to `cargo-deny` / `cargo-audit`; manual stdlib review | `cargo-deny` + `cargo-audit` keep working; smaller tree → less to audit |
| **Wire-protocol drift risk vs `mvm-core`** | Permanent (Zig hand-mirrors Rust types) | None (same Rust types) |
| **Audit-chain integration** | Either fork the chain in Zig or IPC back to Rust supervisor — both worse | Free — same in-process Rust calls |
| **Encryption (W3) implementation cost** | ~800 LoC of careful Noise crypto in Zig | `snow` does it (~1.5k LoC dep, audited) |
| **Estimated full-agent rewrite cost** | ~6,500 LoC, 6-12 weeks | ~2,750 LoC, 2-4 weeks |
| **Process-supervisor complexity reduction** | None (still need fork/exec/wait/PTY) | None |
| **Contributor-pool tax** | Yes — new language skill | None |
| **Tiny-binary win** (e.g. netinit, future single-purpose addons) | Stronger | Real but smaller |
| **Agent-scale win** | Marginal (complexity is architectural) | Material if executor scales |
| **Reversibility (I4)** | Hard once Zig is in CI | Trivial (it's all still Rust) |

---

## 5. Recommendation rationale

Given the table above:

- **For tiny boundary binaries** (`mvm-guest-netinit`, future single-purpose addons, ObjC/Swift shims for VZ), Zig has a real edge because there's no async runtime cost to amortize over a small body of logic. The 87-LoC netinit pulls ~30 transitive crates today; that ratio is brutal.
- **For the agent**, complexity is architectural (33 RPC variants, process supervisor, warm-pool state machine) and doesn't shrink because of language choice. Zig pays the toolchain tax but doesn't address the actual complexity driver.
- **Lean Rust v2 likely captures most of the boundary-dep win** without paying the toolchain tax, while preserving the audit chain, the `mvm-core` type story, and the contributor pool.

Hence the recommendation **stay Rust + adopt lean Rust v2** for the agent's evolution, and treat **Zig as a measurement check** on the netinit prototype to put a number on what we're leaving on the table for future tiny addons.

This is testable. The W2/W2′ prototypes' measurement table will either confirm this (Outcome B) or flip the recommendation (Outcome A). Outcome C (neither prototype shrinks the surface meaningfully) is also possible — in which case W3/W4/W5/W6 still land and the encryption design is the real milestone deliverable.

---

## 6. Open questions for the future decision review

These need W2/W2′ measurement output before they can be answered:

- [ ] **Headline number**: how does dep-tree LoC reaching the guest boundary compare between Zig, lean Rust v2, and current baseline?
- [ ] **CI cost delta**: minutes added per PR for the Zig lane (toolchain pull + fuzz + reproducibility check) vs the lean-Rust lane (negligible)?
- [ ] **Reproducibility**: do both prototypes hit byte-identical rebuilds on aarch64 + x86_64? Any pathology specific to Zig's hermetic build cache at 0.13?
- [ ] **Fuzz parity**: time-to-first-crash on the netlink parser surface for both, against the same adversarial corpus?
- [ ] **Schema oracle health** (per D2): does the Rust-canonical schema reference work cleanly from Zig (build-fail-on-drift)?
- [ ] **Agent-scale executor unknown**: lean Rust v2 with `polling` is proven for one-shots; does it scale to the agent's 33-variant dispatcher + long-lived loop + warm-pool state? W2′ on netinit doesn't settle this — would need a separate agent-v2 prototype.
- [ ] **`serde` retention question**: does dropping `serde` cost us `#[serde(deny_unknown_fields)]` ergonomics enough to compromise claim 5, or can a hand-rolled parser preserve the invariant explicitly?
- [ ] **Memory ceiling under stress**: both prototypes under 100k route attempts — any GC-style pause behavior or unbounded allocation?

---

## 7. What this note does NOT decide

- Whether to do the agent rewrite *at all*. Plan 105 is exploration; the agent rewrite is explicitly out of scope.
- Which specific Rust JSON library replaces `serde_json` if lean Rust v2 wins. Hand-rolled vs `nanoserde` vs `miniserde` is a sub-decision for a future agent-v2 plan.
- Whether Zig is suitable for the Apple VZ Swift shims (separate surface; ADR-056 / Plan 98 own it).
- Whether `mvm-builder-init` should swap off `nix` 0.29. That's a stronger second-target candidate if W2/W2′ measurements support it; deferred to a follow-on plan.

---

## Related

- [Plan 105 — Guest control-layer dep-reduction + encryption design](../plans/105-zig-pid0-exploration.md) (this note implements its W1)
- [Sprint 42 Track E — Zig evaluation gates](../SPRINT.md#track-e--zig-evaluation-gates) (this note satisfies the prerequisite)
- [ADR-002 — microVM security posture](../adrs/002-microvm-security-posture.md) (claims 1-10; the audit invariants this exploration must preserve)
- [ADR-053 — Guest protocol versioning and readiness](../adrs/053-guest-protocol-versioning-and-readiness.md) (informal control/data-plane hint that W4 promotes to a contract)
- [Plan 104 — Host services broker over vsock](../plans/104-host-services-broker.md) (merged 2026-05-26 — adds vsock ports :5300 + :5301 hosting `host.time.v1`, `host.cost.v1`, `host.secrets.v1`; same `AuthenticatedFrame` envelope; W3 encryption design must cover the broker surface, not just the agent's :5252)
- [ADR-059 — Host services broker over vsock](../adrs/059-host-services-broker.md) (PR #470 in flight — companion ADR to Plan 104; the architectural decision behind the two-port broker + out-of-process secrets dispatcher; cross-link is forward-looking until ADR-059 merges)
