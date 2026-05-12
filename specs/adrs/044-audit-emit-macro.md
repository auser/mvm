---
title: "ADR-044: `audit_emit!` macro is the canonical audit emit surface"
status: Accepted
date: 2026-05-12
related: ADR-041 (signed audited execution plans); plan 37 §6 (no unaudited control-plane mutation); plan 60 Phase 4 (persistent observability); plan 64 (supervisor wiring)
---

## Status

Accepted. Macro shipped in PR #106 along with the `LocalAuditBuilder` API, the `xtask check-audit-positional` lint, and the migration of 37 positional emit call sites. The lint is wired into the CI Test/Lint job after `check-no-display-on-secret-types`; new positional `emit(…)` / `event(…).….emit()` calls fail CI until they get the macro treatment or an `// allow(audit-positional): <reason>` annotation.

`tests/audit_emissions_live.rs` carries 40 live drive-and-assert tests as of PR #108 — every positive Emits row in `AUDIT_POSTURE` (`tests/audit_total_coverage.rs`) that can be exercised hermetically has at least one matching live pin, plus 15 negative pins on the ReadOnly leaves.

## Context

Plan 60 Phase 4 calls for "every state-changing CLI verb emits one audit record per attempt, even on no-op" (plan 37 §6). The original emit surface was positional:

```rust
mvm_core::audit::emit(
    mvm_core::audit::LocalAuditKind::ManifestTagAdd,
    None,
    Some(&format!("template={template} tag={tag}")),
);
```

Three problems compounded:

1. **Readability.** The positional `(None, Some(&format!(…)))` form was hard to scan at a glance, especially in multi-line invocations. Reviewers regularly missed which argument was the vm_name versus the detail.

2. **Forward-compatibility.** Plan 60's roadmap calls for an `outcome` field on every emit and (eventually) a `trace_id` for cross-stream correlation. Adding either to the positional signature would have churned ~40 call sites for a one-method change.

3. **Drift.** Two emit destinations coexisted: the canonical `audit::emit` writes to `<state>/log/audit.jsonl` (XDG state path); a handful of sites — notably `storage gc` — built their own `LocalAuditLog::open` against `<data_dir>/audit.log` (singular, `.log`, no `/log/` subdir). The bypass meant `mvmctl audit tail` couldn't see those entries and the live test suite couldn't observe them without per-verb fixture work.

The substrate scaffold (`tests/audit_total_coverage.rs`) caught classification gaps — every CLI subcommand at every level must have an `AuditPosture` declaration — but didn't catch *behavioral* drift. A verb classified `Emits("X")` could ship without an actual emit, or emit to the wrong file, and the static scaffold wouldn't flag it.

## Decision

### Three-layer surface, one canonical entry point

```
audit_emit!(Kind, …)              ← four-arm macro (callers use this)
  │ desugars to
  ▼
audit::event(Kind).….emit()       ← builder API (open-ended composition)
  │ writes through
  ▼
LocalAuditLog::open(default_audit_log()).append(…)
  │ which lands at
  ▼
~/.local/state/mvm/log/audit.jsonl
```

#### Layer 1: `audit_emit!` macro (preferred)

Four arms collapse the common shapes to one line each:

```rust
audit_emit!(CachePrune);                                    // bare
audit_emit!(StorageGc, "count={count}");                    // format-string detail
audit_emit!(SlotRemove, vm: hash);                          // vm_name only
audit_emit!(SlotRemove, vm: hash, "path={p}", p = "x");     // both, named args
```

The format-string arm accepts the full `format!` syntax — positional args, named args, named captures — because the macro passes the literal token stream through to `format!()` verbatim. That means a call site can capture local variables inline (`"k={v}"`) without an explicit binding.

#### Layer 2: `LocalAuditBuilder` (for unusual compositions)

Chained-builder for the cases that don't fit the four arms (e.g. conditional fields, runtime-resolved kinds):

```rust
let mut b = audit::event(kind);
if include_vm { b = b.vm_name(name); }
if let Some(d) = detail { b = b.detail(d); }
b.emit();
```

`LocalAuditBuilder` is marked `#[must_use]` so a dropped chain (forgot `.emit()`) is a compile warning.

#### Layer 3: legacy `emit(kind, vm_name, detail)` shims

`audit::emit` and `audit::emit_to` survive as thin wrappers over the builder. They exist for two reasons: (a) one-line backward-compat for external crates that import the function, and (b) the `emit_to` variant lets tests redirect to an explicit path without `XDG_STATE_HOME` juggling.

New code uses Layer 1 or 2. The xtask lint enforces this.

### Guardrails

**`xtask check-audit-positional`** walks `crates/*/src/**/*.rs` and flags any call to `mvm_core::audit::emit(...)`, `audit::emit_to(...)`, or chained `audit::event(...).….emit()`. Opt-out via `// allow(audit-positional): <reason>` directly above the call (matches the `secret-debug` lint's annotation shape so contributors learn one convention).

The lint runs in CI's Test/Lint job. The audit module itself (`crates/mvm-core/src/policy/audit.rs`) is exempt — the shims and the builder API live there by definition.

**`tests/audit_emissions_live.rs`** is the behavioral suite. For every Emits row in `AUDIT_POSTURE`, a live test spawns the real `mvmctl` binary via `assert_cmd` against a `tempfile::tempdir()` HOME and asserts the audit log carries the expected entry. The substrate (`AuditSandbox`, `read_audit_log`, `count_entries_with_kind`) is shared infrastructure; per-row test bodies are typically 20–30 lines each.

**`tests/audit_total_coverage.rs`** stays as the classification scaffold. It recursively walks `mvm_cli::commands::cli_command()` and asserts every clap subcommand has a declared `AuditPosture`. The live and static layers complement each other: classification catches "did you forget to classify this verb?", behavior catches "did you forget to actually emit?".

### Migration tooling

`scripts/migrate-audit-emit.sh` is a Perl one-shot for the four common positional shapes (bare, with format-detail, with vm, with both). It's idempotent — running twice is a no-op — and handles single- and multi-line forms. The xtask lint catches anything the script skipped; the human reviews and migrates manually.

## Consequences

### Positive

- **One-line call sites.** The most common emit shape collapses from a 4-line positional invocation to a single-line macro call. Readers don't have to mentally parse `None, Some(&format!(…))`.
- **Future fields are free.** Adding `outcome` or `trace_id` to `LocalAuditEvent` becomes a one-method change on the builder; the macro grows new arms; the existing call sites stay untouched.
- **One canonical destination.** Every state-changing verb lands in `<state>/log/audit.jsonl`. `mvmctl audit tail` sees everything. The live test suite reads one file.
- **CI-enforced drift protection.** A new positional emit fails CI before it lands. The bypass requires a written-out reason that surfaces in lint output.

### Negative

- **One more thing to learn.** New contributors see `audit_emit!(...)` and have to look up its arms before they can extend it. The trade is reasonable because the macro's arms cover the 90% case explicitly (see the module-level docs in `crates/mvm-core/src/policy/audit.rs`).
- **`jump-to-def` lands on the macro.** Rust analyzer expands `audit_emit!(StorageGc, ...)` and shows the macro body, not the underlying `event(...)` call. A reader who wants to read the production path needs one extra hop. Acceptable cost.
- **`format!` syntax leaks through.** Misuse (`audit_emit!(K, "{undefined}")`) becomes a `format!` compile error, not a macro error. The message still points at the macro call site, so debugging is fine.

### Bounded

- **The macro doesn't cover the supervisor's chain-signed audit stream.** Plan 64's `~/.mvm/audit/<tenant>.jsonl` chain emits via `AuditEmitter` middleware. That's a different stream with different semantics (chain hashing, plan-bound entries). The `audit_emit!` macro is specifically for the `LocalAudit` stream used by single-host operator-facing verbs.
- **Negative complement tests are still per-verb.** The `xtask check-audit-positional` lint catches *positional* call sites; it doesn't catch a verb that *should* emit but doesn't. That's what the live test suite is for — adding a `Emits("X")` classification to `AUDIT_POSTURE` and a matching live test together is the contract.

## References

- **PR #106** — macro + builder + lint + 19 positional migrations
- **PR #107** — cleanup-host-fallback refactor + 5 ReadOnly negative pins
- **PR #108** — MockBackend substrate + VM-lifecycle live tests (VmStart, VmStop, VmTtlSet)
- **Plan 37 §6** — "no unaudited control-plane mutation" invariant
- **Plan 60 Phase 4** — Persistent observability (audit chain + emission)
- **ADR-041** — Signed, audited `ExecutionPlan` (the chain-signed audit stream this complements)
