# ADR 063 - Boundary Language Policy

**Status**: Proposed
**Date**: 2026-05-28
**Cross-refs**: ADR-002 (security posture), ADR-060 (pid0 portability boundary), Plan 109 (guest control-layer dep-reduction + encryption design), Sprint 42 §Track E (Zig evaluation gates)

## Context

mvm's guest-side control surface (pid0 — agent, init, netinit, addon binaries) is Rust today, with a heavy transitive dependency tree at the boundary: `tokio`, `serde_json`, `ed25519-dalek`, `rtnetlink`, `seccompiler`, and hundreds of indirect crates. Sprint 42's Dependency Reduction Roadmap added a **Track E** that gates any Zig adoption behind two rules:

1. Do not introduce Zig for broad protocol-heavy replacements (`oci-client`, `pgp`) without a written tradeoff note covering native toolchain cost, cross-platform CI complexity, auditability, and risk reduction.
2. If Zig is evaluated at all, constrain it to narrow ABI shims or parser islands where the native surface is small and stable.

Track E names the gates but doesn't formalize them as a project-level policy. As Plan 109 runs the first concrete evaluation (a Zig vs lean-Rust-v2 A/B on `mvm-guest-netinit`), the team needs an ADR that the *next* contributor — or AI session — can consult without having to re-derive the discipline.

This ADR codifies the policy. It is deliberately framed broadly as "boundary language policy" — not "Zig policy" — so that the doc has value even if Plan 109's measurements reject Zig and the project stays Rust-only. The point is the rule, not the language.

## Decision

mvm's default boundary language is **Rust**. Adoption of any other language at the boundary requires evidence and is gated as below.

### 1. Default: Rust everywhere at the boundary

Every binary that runs in the guest's blast radius (pid0 control surface per ADR-060: init, netinit, agent, in-boot addons) and every host-side binary that participates in the audit chain or signs material is **Rust by default**.

The default is not negotiable case-by-case. Adopting a non-Rust binary requires the explicit process in §3.

### 2. First path for dependency reduction: lean Rust v2

Before considering a non-Rust language for any boundary binary, the team MUST evaluate whether the dep-reduction goal can be met within Rust. The "lean Rust v2" discipline:

- Replace `tokio` with `polling` + a small hand-rolled executor for binaries where async/await ergonomics are not load-bearing.
- Replace `serde_json` with hand-rolled per-variant parsers (or `nanoserde`) where the wire surface is small and stable.
- Replace `rtnetlink` / `netlink-packet-route` with `linux-raw-sys` + manual netlink for one-shot or narrow netlink usage.
- Replace heavy proc-macro derive chains with minimal-derive alternatives or inline implementations where reasonable.
- Existing precedent: `mvm-egress-proxy` (libc only, no tokio) demonstrates the discipline already shipping in this repo.

Plan 109's measurement framework (`specs/research/agent-evolution-tradeoff-note.md`) establishes the rubric for sizing this path. Subsequent dep-reduction proposals should use the same rubric so results are comparable.

### 3. Where non-Rust may be considered

Non-Rust languages MAY be proposed for boundary code only when **all** of the following hold:

- The binary has a **narrow ABI surface** (a small, stable set of syscalls or wire variants). Examples that qualify: a netlink-route installer, a virtio-vsock diagnostics probe, an ObjC/Swift bridge to a closed-source host framework (Apple Vz). Examples that do not qualify: anything with broad protocol stacks (OCI, PGP, OpenAPI, async runtimes).
- The binary is **not driving the audit chain**. Anything that emits to `~/.mvm/audit/<tenant>.jsonl` or computes/verifies signatures over `ExecutionPlan` material stays Rust. The audit chain is single-language by design (claim 8).
- The native language materially reduces the **supply-chain attack surface or boundary footprint**, with measurement evidence per Plan 109's rubric — not opinion.

Non-Rust languages MUST NOT be proposed for:

- Broad protocol stacks (OCI, PGP, OpenAPI, sigstore, OAuth, etc.).
- Async runtimes (`tokio` replacements). Building an event loop in another language is a workaround for "tokio is large" that re-creates the same async substrate without solving the underlying issue.
- Anything that interacts with the audit chain.
- Anything whose wire types are defined in `mvm-core` and shared across host/guest. The protocol stays Rust-canonical (Plan 109 §D2); any non-Rust implementation consumes a Rust-derived schema and round-trips byte-identically.

### 4. Required deliverables per non-Rust adoption

Any PR proposing a non-Rust boundary binary MUST include, before any prototype merge:

1. **Tradeoff note** in `specs/research/<topic>-tradeoff-note.md`. Template: `specs/research/agent-evolution-tradeoff-note.md` (Plan 109 W1). Symmetric analysis of the non-Rust path AND a lean-Rust-v2 alternative. Cover: toolchain cost, CI complexity, auditability, supply-chain surface comparison, maintenance/contributor-pool impact.
2. **Measurement comparison** with the lean-Rust v2 baseline against the rubric in Plan 109 W2/W2′. Primary metrics: dep-tree LoC reaching the boundary, transitive crate / module count + advisory exposure, stripped binary size, compile time, fuzz parity, reproducibility (claim 7), CI cost delta.
3. **Fuzz harness** equivalent to the existing `cargo-fuzz` targets covering the binary's parser surface. The non-Rust fuzzer must run in CI on the same PR cadence.
4. **Reproducible-build proof** — byte-identical rebuilds on `aarch64-linux` and `x86_64-linux` musl-static. Claim 7's invariant doesn't get a language carve-out.
5. **Supply-chain attestation** equivalent to `cargo-deny` / `cargo-audit`: a documented inventory of every external dependency (stdlib modules, compiler version, transitive includes) and a CI gate that fails on yanked / advisory-flagged inputs.
6. **Schema parity test** (per ADR-060 §8): the non-Rust binary consumes the Rust-canonical wire types and the build fails if the Rust types drift.

Each deliverable is **a CI gate**, not a discretionary review item. Without all six, the PR is not mergeable regardless of measurement outcome.

### 5. Reversibility

Any non-Rust boundary binary MUST be kept side-by-side with the existing Rust implementation until the non-Rust path is proven across at least one full release cycle. The default kernel cmdline path selects the Rust binary; the non-Rust binary is opt-in via flake option. This is Plan 109's I4 invariant generalized.

Adoption is then a separate PR that flips the default. Removal of the Rust binary is a *third* PR, no earlier than one release cycle after the default flip. The transition is irreversible only at that point.

### 6. Apple VZ ObjC/Swift shim — explicit carve-out

`mvm-vz-supervisor` (Plan 98 / ADR-056) uses Swift to bridge to Apple's closed-source Virtualization.framework. This is an existing exception that predates this ADR and is grandfathered. Any future Apple-framework integration MAY use Swift or ObjC under the same scoping discipline as §3 — narrow, framework-mandated, not part of the audit chain — but MUST still satisfy §4's deliverables.

## Consequences

**Positive:**

- Future contributors and AI sessions have a single document explaining when Zig (or any non-Rust language) is acceptable at the boundary. No need to re-derive the discipline from Track E plus folklore.
- The lean-Rust-v2 path becomes the explicit first move for any dep-reduction proposal, not a runner-up considered after a more exotic option.
- The six-deliverable bar (§4) makes prototype-to-adoption a measurable process, not a tribal-knowledge decision.

**Negative:**

- The deliverable bar may slow legitimate experimentation. Mitigation: tradeoff notes and prototypes can be authored cheaply (Plan 109 demonstrates this); only *adoption* requires the full bar.
- The schema-parity requirement (§3 "Rust-canonical types") forces hand-mirroring or codegen for any non-Rust implementation. Engineering cost is real and called out in Plan 109's analysis. Accepted.
- Codifies a status-quo bias toward Rust. This is intentional — the team is Rust-first and the audit chain is Rust-anchored. Lowering the bar would re-introduce the toolchain-tax compounding that Plan 109 §"Systems-design recommendation" specifically identifies as the strongest argument against multi-language sprawl.

**Neutral:**

- Does not retroactively bless or revoke any existing non-Rust code. The Vz Swift shim is grandfathered (§6); there is no other non-Rust code at the boundary today.
- Does not commit the team to evaluating any particular non-Rust language. Plan 109's Zig prototype is the first evaluation; nothing here commits to a second.

## Alternatives considered

**Alt 1: prohibit non-Rust at the boundary entirely.** Rejected because the Apple Vz Swift shim is load-bearing and the framework is closed-source. A blanket prohibition would either force re-implementation of Vz support in Rust (technically infeasible — Apple's framework requires Swift/ObjC) or block macOS Apple-Silicon support. A scoped policy is the workable middle.

**Alt 2: allow non-Rust freely, gate only by code review.** Rejected because boundary code reaches every microVM and supply-chain hygiene compounds. Discretionary review provides no consistent floor; the six-deliverable bar gives one.

**Alt 3: enumerate permitted languages (Rust, Zig, Swift) by name.** Rejected because it overcommits to today's set. The current text scopes by *constraints* (narrow ABI surface, not in audit chain, schema parity to mvm-core), which any future language candidate must satisfy. If a fourth language ever becomes a candidate, this ADR applies without amendment.

## References

- ADR-002 (microVM security posture, claims 1-10)
- ADR-060 (pid0 portability boundary — what the boundary binaries must do regardless of language)
- ADR-056 (Vz backend — grandfathered Swift shim, see §6)
- Plan 98 (Vz builder VM — Swift supervisor wiring)
- Plan 109 (guest control-layer dep-reduction + encryption design — this ADR is W4c)
- `specs/research/agent-evolution-tradeoff-note.md` (Plan 109 W1 — template for §4.1)
- `specs/SPRINT.md` Sprint 42 Track E (origin of the Zig evaluation gates this ADR codifies)
- `deny.toml` (existing Rust supply-chain policy this ADR generalizes)
