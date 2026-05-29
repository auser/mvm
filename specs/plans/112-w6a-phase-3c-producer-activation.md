# Plan 112 — Plan 102 W6.A Phase 3c: Producer Activation

**Sprint:** 56 (Plan 102 W6.A.5 follow-up)
**Parent:** [Plan 102](102-gateway-audit-substrate-impl.md) §W6.A.5
**Tracker:** [Plan 103](103-w6a-implementation-tracker.md) §Status
**ADRs:** [ADR-041](../adrs/041-signed-audited-execution-plans.md) (claim 8), [ADR-058](../adrs/058-claim-10-bytes-leaving-trust-boundary.md) (claim 10 leg 2)
**Status:** 🟡 in progress — PR pending

## Context

PR #487 (`worktree-plan-102-w6a-5-wire-up`) — **MERGED into `main`** — shipped the gateway
audit substrate: `mvm-libkrun-supervisor` bin entrypoint, `SupervisorConfig` gained five
`Option`-typed audit fields (`tenant_id`, `audit_dir`, `gateway_audit_socket`,
`gateway_events_socket`, `signing_key_path`) with `#[serde(default)]`, `mvm-libkrun`
gained `BridgeFds` + `configure_with_gateway_for_bridge` + `run_supervisor_with_bridge<F>`,
the supervisor `main` branches on `cfg.tenant_id` (`Some` → bridge factory, `None` →
legacy `run_supervisor`), Vz's host-side gvproxy lifecycle wired (`host_gvproxy.rs`), and
the Swift `makeBridgedGvproxyDevice` + 5 XCTest cases shipped.

**Phase 3c is the producer side that activates the substrate.** Today (post-#487):

- `mvm_core::vm_backend::VmStartConfig` carries no audit-substrate fields.
- `crates/mvm-backend/src/libkrun.rs::start()` hard-codes the new `SupervisorConfig`
  fields to `None` with a comment: *"backend orchestrator population … lands alongside
  the run_supervisor_with_bridge wire-up (commit 6.5 / 7)"* — that's this plan.
- `mvm-libkrun-supervisor::main` therefore takes the **legacy `run_supervisor` path on
  every spawn**, so claim-10 leg 2 (per ADR-058) is dormant.

Phase 3c scope (verbatim from PR #487's merged body): *"widens
`mvm_core::vm_backend::VmStartConfig` with three `Option<String>` fields (`tenant_id`,
`plan_json`, `bundle_json`) and threads `AdmittedPlan` through to backend `start()` at the
~6 mvm-cli `VmStartConfig` construction sites."*

The fields are `Option<String>` (not typed) so `mvm-core` keeps zero dep on `mvm-plan`
(avoids closing the `mvm-plan → mvm-libkrun → mvm-core` cycle).

## Goal

End-to-end activation: every `mvmctl up` (direct-boot, main, watch-loop) populates
`VmStartConfig.{tenant_id, plan_json, bundle_json}` from its in-scope `AdmissionContext`;
`libkrun.rs::start()` and `vz.rs::start()` thread those into `SupervisorConfig` (deriving
`audit_dir` / gateway sockets / signing key from `mvm_data_dir()`); the libkrun supervisor
takes the **bridge-factory path** on at least one live smoke per backend × network combo
(libkrun+passt, libkrun+gvproxy, Vz+gvproxy).

## Pick-up command (for fresh sessions)

Read `specs/plans/102-gateway-audit-substrate-impl.md` §W6.A.5 first for the substrate
context, then resume here from the first unchecked task below. Worktree:
`.claude/worktrees/plan-102-phase-3c`; branch: `worktree-plan-102-phase-3c`.

## Architecture decisions

- **Helper home:** `populate_audit_substrate(&mut VmStartConfig, &AdmittedPlan)` lives in
  `crates/mvm-cli/src/commands/vm/plan_admission.rs`. mvm-cli already depends on mvm-plan +
  mvm-core, so it's the natural home. `mvm-libkrun` can't depend on `mvm-plan` (would
  close the cycle), so the helper can't live there.
- **Supervisor re-verifies plan_json** at `crates/mvm-supervisor/src/supervisor.rs:308`
  via `mvm_plan::verify_plan(signed, trusted_keys)`. Host is in the TCB per ADR-002, but
  the supervisor still doesn't trust the host's pre-decoded fields — it re-runs Ed25519
  verification against its own trusted-keys list.
- **TenantId path-safety (allowlist, not deny-list):** `TenantId` is a `String` newtype
  with no validation in mvm-plan. The supervisor uses `tenant_id` to construct
  `<audit_dir>/<tenant_id>.jsonl`, so a malicious tenant value (`../foo`, Unicode
  confusables, pathological length) could escape or DoS. Validate inside
  `audit_substrate::validate_tenant_id` with an RFC 1123 DNS-label-shaped allowlist:
  `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}$`. ASCII-only, max 63 chars. Deny-list approaches
  are fail-open by construction; allowlist is fail-closed.
- **vm_name path-safety:** Same risk as `tenant_id` — `vm_name` flows into the gateway
  socket filename. Apply the same allowlist guard.
- **Envelope size caps:** `plan_json` ≤ 1 MiB, `bundle_json` ≤ 4 MiB. DoS guard.
- **Never log the envelope:** Both `plan_json` and `bundle_json` may carry secret
  bindings, env vars, and policy refs that resolve to credentials. Treat as opaque
  transport bytes. Doc warnings at producer + consumer sites.
- **`AdmissionContext` is the carrier:** `admit_plan_for_boot` returns
  `Result<Option<AdmissionContext>>`. `AdmissionContext { admitted: AdmittedPlan, … }`.
  Producer sites use `if let Some(ctx) = &admission { populate_audit_substrate(..., &ctx.admitted)?; }`.
- **`--prod` gate is not in scope.** Production-mode admission policy is fleet-level and
  belongs in mvmd, not mvm. See memory `feedback_prod_gate_lives_in_mvmd.md`.
- **Vz backend in scope.** Live smokes need all three backends.
- **Firecracker and AppleContainer are deliberately untouched.** They consume the same
  `VmStartConfig` (since the trait is shared) and will receive the new fields when
  admission is in scope, but their `start()` impls don't read the fields — the
  `Option<String>` values are silently dropped. **Impact: claim-10-leg-2 gateway flow
  events do not fire on those backends; claim 8 (plan.admitted / plan.launched /
  plan.failed) continues to work everywhere** because those events are emitted in
  mvm-cli before `backend.start()`. Adding flow-event coverage to Firecracker would
  need a jailer-side bridge factory (separate plan, separate process model);
  AppleContainer would need a design that accommodates Apple's opaque
  `containerization` framework. See memory
  `project_gateway_audit_substrate_backend_coverage.md`.
- **Forward-compat factoring for a future `NetworkProvider` trait.** The next plan
  after 112 is expected to be "`NetworkProvider` trait + Firecracker substrate,
  combined" — abstracts the audit-substrate + network-gateway wiring behind a trait
  so libkrun / Vz / Firecracker (and later, AppleContainer + egress-secret-detection
  wrapper) are pluggable impls. To make that extraction mechanical, Phase 3c does
  **not** define a trait, but it **does** extract the shared substrate-resolution
  logic into `crates/mvm-backend/src/audit_substrate.rs`. Both libkrun and Vz call
  into it. When the trait lands, the rename is mechanical:
  `audit_substrate::compute(...)` becomes `provider.activate_audit(...)`. No new
  crate; no trait yet; one shared file.
- **Top-level `crates/mvm-cli/src/exec.rs` sites stay None.** `boot_session_vm` and the
  `restore_via_snapshot` VmStartConfig have no `AdmittedPlan` in scope — they're
  MCP-session and template-restore paths that pre-date the admission contract.
  Annotated with doc comments; the supervisor takes the legacy path for these.

## Backend impact matrix

| Backend | File | Reads new fields? | Claim 8 (admission) | Claim 10 leg 2 (flow events) | Change in this plan |
|---|---|---|---|---|---|
| libkrun | `crates/mvm-backend/src/libkrun.rs` | Yes (Task 5) | Yes (mvm-cli emits) | **Yes** (substrate activated) | `build_supervisor_config` helper added |
| Vz | `crates/mvm-backend/src/vz.rs` | Yes (Task 6) | Yes (mvm-cli emits) | **Yes** (substrate activated) | Mirror of libkrun's pattern |
| Firecracker | `backend.rs::FirecrackerBackend` | No (silently drops) | Yes (mvm-cli emits) | No (no bridge wiring) | None |
| AppleContainer | `apple_container.rs` | No (silently drops) | Yes (mvm-cli emits) | No (no bridge wiring) | None |
| MicrovmNix | `microvm_nix.rs` | No (silently drops) | Yes (mvm-cli emits) | No (out of substrate scope) | None |
| CloudHypervisor | `cloud_hypervisor.rs` | No (builder VM only) | n/a | n/a | None |
| Docker | `docker.rs` | No (silently drops) | Yes (mvm-cli emits) | No (Tier 3) | None |
| Mock | `mock.rs` | No (silently drops) | Yes (mvm-cli emits) | n/a (test fixture) | None |

**Reading guide:** "silently drops" = backend receives the populated `VmStartConfig` but
its `start()` impl never touches the three new fields — equivalent to its pre-Phase-3c
behavior. No regression risk.

## Field map: `AdmittedPlan` → `VmStartConfig`

| `VmStartConfig` field | Source | Encoding |
|---|---|---|
| `tenant_id: Option<String>` | `admitted.plan.tenant.0.clone()` | raw `String` |
| `plan_json: Option<String>` | `&admitted.signed` (the **signed envelope**) | `serde_json::to_string(&admitted.signed)?` |
| `bundle_json: Option<String>` | `admitted.plan.bundle.as_ref()` when `Some` | `serde_json::to_string(pin)?` else `None` |

`plan_json` carries the **signed envelope**, not the inner `ExecutionPlan` — the
supervisor re-verifies before trusting decoded fields.

## Files

- **Create:**
  - `crates/mvm-backend/src/audit_substrate.rs` — shared module with
    `validate_tenant_id` / `validate_vm_name` (DNS-label allowlist) and
    `compute_audit_substrate(vm_name, tenant_id) -> Result<AuditSubstrate>`. Seam for
    the future `NetworkProvider` trait.
- **Modify:**
  - `crates/mvm-core/src/protocol/vm_backend.rs` — append three fields after
    `runner_dir`; extend `test_vm_start_config_default`.
  - `crates/mvm-cli/src/commands/vm/plan_admission.rs` —
    `populate_audit_substrate(&mut VmStartConfig, &AdmittedPlan)` helper with size
    caps + unit test.
  - `crates/mvm-cli/src/commands/vm/up.rs` — three `VmStartConfig` construction sites
    (direct-boot `admission`, main `admission_main`, watch-loop `watch_admission`).
  - `crates/mvm-cli/src/exec.rs` — doc-comment-only annotations on
    `restore_via_snapshot` and `boot_session_vm` VmStartConfig sites.
  - `crates/mvm-backend/src/lib.rs` — `pub(crate) mod audit_substrate;`.
  - `crates/mvm-backend/src/libkrun.rs` — extract `build_supervisor_config` helper;
    delegate to `audit_substrate::compute_audit_substrate`.
  - `crates/mvm-backend/src/vz.rs` — same shape as libkrun.
  - `specs/plans/102-gateway-audit-substrate-impl.md` — tick Phase 3c checklist.
  - `specs/plans/103-w6a-implementation-tracker.md` — bump §Status.

## Tasks

### Task 0 — Worktree + preamble + plan file

- [x] **0.1** Stash 17-file uncommitted main state (managed_secrets / Plan 108 stream).
- [x] **0.2** Create worktree at `.claude/worktrees/plan-102-phase-3c` off `main`.
- [x] **0.3** Pop stash into worktree.
- [x] **0.4** Commit preamble as a single topic commit ("managed_secrets / Plan 108
      stream"); auto-fmt fixes as a second commit.
- [x] **0.5** Verify worktree builds + tests pass (apple_container flake confirmed
      pre-existing, passes single-threaded per PR #487 note).
- [x] **0.6** Save this plan into `specs/plans/112-w6a-phase-3c-producer-activation.md`
      (Plan 112 — 108 gap left vacant; 109-111 claimed).
- [ ] **0.7** Commit the plan file.

### Task 1 — Widen `VmStartConfig`

- [ ] **1.1** Extend `test_vm_start_config_default` with assertions on `tenant_id` /
      `plan_json` / `bundle_json` defaulting to `None`.
- [ ] **1.2** Run; expect FAIL with "no field 'tenant_id'".
- [ ] **1.3** Append three fields to `VmStartConfig` after `runner_dir` (append-at-end
      avoids line conflicts with future inserts higher in the struct).
- [ ] **1.4** Run gates: `cargo test -p mvm-core`, fmt, clippy. Expect clean.
- [ ] **1.5** Commit: `feat(mvm-core): VmStartConfig audit-substrate fields (Plan 102 Phase 3c)`.

### Task 2 — `populate_audit_substrate` helper

- [ ] **2.1** Write failing test in `plan_admission.rs`'s test module
      (`populate_audit_substrate_threads_tenant_and_signed_envelope`).
- [ ] **2.2** Run; expect FAIL.
- [ ] **2.3** Implement helper with size caps (`PLAN_JSON_MAX_BYTES = 1 MiB`,
      `BUNDLE_JSON_MAX_BYTES = 4 MiB`) and doc warning "do not log the envelope".
- [ ] **2.4** Gates + commit: `feat(mvm-cli): populate_audit_substrate helper (Plan 102 Phase 3c)`.

### Task 3 — Wire three `up.rs` producer sites

- [ ] **3.1** Extend the `use super::plan_admission::{...}` import in `up.rs` with
      `populate_audit_substrate`.
- [ ] **3.2** Wire direct-boot (`MVM_DIRECT_BOOT == "1"` branch) — `admission` in scope:
      change `let start_config` to `let mut start_config`, call
      `if let Some(ctx) = admission.as_ref() { populate_audit_substrate(&mut start_config, &ctx.admitted)?; }`.
- [ ] **3.3** Wire main path — `admission_main` in scope; same pattern after
      `into_start_config()`.
- [ ] **3.4** Wire watch-loop — `watch_admission` in scope; same pattern after
      `into_start_config()`.
- [ ] **3.5** Gates + commit: `feat(mvm-cli): populate audit substrate at mvmctl up sites (Plan 102 Phase 3c)`.

### Task 4 — Annotate top-level `exec.rs` (no code change)

- [ ] **4.1** Add doc comment above `restore_via_snapshot`-feeding VmStartConfig
      explaining no admission in scope.
- [ ] **4.2** Add doc comment above `boot_session_vm` VmStartConfig with same rationale.
- [ ] **4.3** Gates + commit: `docs(mvm-cli): annotate top-level exec.rs as Phase 3c legacy path`.

### Task 4b — Shared `audit_substrate` module

- [ ] **4b.1** Create `crates/mvm-backend/src/audit_substrate.rs` with tests only first:
      `validate_tenant_id_allowlist_dns_label_shape`,
      `validate_vm_name_allowlist_dns_label_shape`,
      `compute_with_tenant_derives_all_paths`, `compute_without_tenant_returns_default`,
      `compute_with_unsafe_tenant_errors`.
- [ ] **4b.2** Run; expect FAIL.
- [ ] **4b.3** Implement `AuditSubstrate` struct, `validate_dns_label` /
      `validate_tenant_id` / `validate_vm_name`, `compute_audit_substrate(vm_name,
      tenant_id)`.
- [ ] **4b.4** Locate canonical `mvm_data_dir()` (likely
      `mvm_core::config::mvm_data_dir()` per CLAUDE.md "XDG directory functions"); fill
      the placeholder.
- [ ] **4b.5** Wire `pub(crate) mod audit_substrate;` in `crates/mvm-backend/src/lib.rs`.
- [ ] **4b.6** Gates + commit:
      `feat(mvm-backend): shared audit_substrate module (Plan 102 Phase 3c; NetworkProvider seam)`.

### Task 5 — `libkrun.rs::start()` delegates to `audit_substrate`

- [ ] **5.1** Extract `build_supervisor_config(config: &VmStartConfig, state_dir:
      &Path) -> Result<SupervisorConfig>` helper near `effective_cmdline`. Delegate
      substrate resolution to `audit_substrate::compute_audit_substrate`; map the
      `AuditSubstrate` into the five `SupervisorConfig` audit fields.
- [ ] **5.2** Replace the inline `KrunContext + SupervisorConfig` block at the existing
      hard-coded-`None` site with a call to the helper.
- [ ] **5.3** Add libkrun-side wiring tests (mapping into `SupervisorConfig`, not
      substrate logic — that lives in `audit_substrate.rs` tests):
      `build_supervisor_config_maps_substrate_into_supervisor_config`,
      `build_supervisor_config_no_tenant_keeps_substrate_none`.
- [ ] **5.4** Gates + commit: `feat(mvm-backend): libkrun start() delegates to audit_substrate (Plan 102 Phase 3c)`.

### Task 6 — `vz.rs::start()` mirrors libkrun

- [ ] **6.1** Locate the Vz supervisor-config construction site
      (`rg -n "SupervisorConfig|tenant_id|events_ingest_socket_path" crates/mvm-backend/src/vz.rs`).
- [ ] **6.2** Extract `build_vz_supervisor_config` (or analog) helper that calls
      `audit_substrate::compute_audit_substrate` and maps into the Vz supervisor config
      type. No tenant-validation or path-derivation duplication.
- [ ] **6.3** Add Vz-side wiring test (analog of Task 5.3 against the Vz config type).
- [ ] **6.4** Gates + commit: `feat(mvm-backend): vz start() delegates to audit_substrate (Plan 102 Phase 3c)`.

### Task 7 — Plan-doc tick

- [ ] **7.1** Tick Phase 3c checklist items in `specs/plans/102-…md` (VmStartConfig
      widening, mvm-cli populate, libkrun consumer, Vz consumer, tenant_id +
      vm_name safety).
- [ ] **7.2** Bump `specs/plans/103-…md` §Status from "🟡 in progress" to "✅ shipped —
      PR #<NNN>".
- [ ] **7.3** Commit: `docs(specs): tick Plan 102 Phase 3c (Plan 103 status bump)`.

### Task 8 — Workspace gates + PR

- [ ] **8.1** Full workspace gates: `just lint`, `just test`, plus `cargo test -p
      mvm-core -p mvm-cli -p mvm-backend` (touched crates clean in isolation).
- [ ] **8.2** Push branch: `git push -u origin worktree-plan-102-phase-3c`.
- [ ] **8.3** Open PR against `main` titled "feat: Plan 102 Phase 3c — VmStartConfig
      audit substrate producer activation" with body referencing this plan,
      [Plan 102 §W6.A.5](102-gateway-audit-substrate-impl.md#w6a5--vz-swift-bridge--fd-interception-wire-up),
      and ADR-058.

## Verification

### Live smoke (manual, per backend × network)

For each combo {libkrun+passt (Linux), libkrun+gvproxy (macOS), Vz+gvproxy (macOS 26+)}:

1. `cargo build --release`
2. `mvmctl init` (create `~/.mvm/{audit,keys}/`)
3. `mvmctl up --flake . --profile minimal --tenant smoke`
4. In another shell: `nc -U ~/.mvm/audit/gateway-<vm-name>.sock | head -20`
5. `mvmctl exec <vm-name> -- curl https://example.com`
6. Expect: `{"event":"FlowOpened",…}` line on connect, `{"event":"FlowClosed",…}` on close.
7. `mvmctl down <vm-name>`
8. `mvmctl audit verify --tenant smoke` — expect: chain verifies, no `chain_drift`.

### Legacy path continues to work

1. `mvmctl dev up` (no tenant, no plan).
2. Expect: no audit socket created, dev shell drops in normally, no supervisor errors.

### Audit chain mixed-events smoke

1. `mvmctl up --tenant smoke` (admitted plan, bridge path).
2. Drive some events via `mvmctl exec`.
3. `mvmctl down <vm-name>`.
4. Inspect `~/.mvm/audit/smoke.jsonl` — expect: `plan.admitted` → some `FlowOpened` /
   `FlowClosed` pairs → `plan.launched`.
5. `mvmctl audit verify --tenant smoke` — must pass.

## Security follow-ups (out of Phase 3c scope; tracked separately)

- **Supervisor-side tenant cross-check.** Add at `mvm-libkrun-supervisor::main`'s
  admission step: when `cfg.tenant_id` is `Some`, deserialize `cfg.plan_json`, verify
  the signature (already done), refuse with a clear error if
  `cfg.tenant_id != verified.tenant.0`.
- **Typed `ValidatedTenantId(String)` wrapper in mvm-plan.** Touches every
  `TenantId` consumer — separate refactor PR.
- **Symlink hardening on socket binding paths.** Supervisor's socket-create path
  should use `O_NOFOLLOW` + `O_EXCL`. Verify during execution; if missing, file a
  supervisor-side issue.
- **Audit-event PII (5-tuple in `FlowOpened` / `FlowClosed`).** Redaction layer is
  its own future plan and ADR (memory `project_egress_secret_detection_is_core`).

## Out of scope

- **`--prod` admission requirement** — belongs in mvmd. Memory:
  `feedback_prod_gate_lives_in_mvmd.md`.
- **Session VM admission** (top-level `exec.rs` `boot_session_vm`) — would need a
  synthesis step. Tracked as a follow-up.
- **Template restore admission** (top-level `exec.rs` `restore_via_snapshot` path) —
  same as above.
- **Firecracker substrate** — needs jailer-side bridge factory (separate plan).
  Phase 3c doesn't regress anything on Firecracker.
- **AppleContainer substrate** — needs research into Apple's `containerization`
  framework. Phase 3c doesn't regress AppleContainer.
- **MicrovmNix / Docker / Mock** — silently drop the new fields. Out of substrate scope.
- **deps_volume binding** — already wired (Plan 73 Followup B.3).

## Deferred follow-ups

- [ ] Supervisor-side `cfg.tenant_id == verified.tenant.0` cross-check (security follow-up).
- [ ] Typed `ValidatedTenantId` wrapper in mvm-plan (refactor PR).
- [ ] Symlink hardening verification + (if needed) `O_NOFOLLOW + O_EXCL` fix
      (supervisor-side).
- [ ] `NetworkProvider` trait + Firecracker substrate (next plan in the
      claim-10-leg-2 arc; uses the `audit_substrate` module as the trait-extraction
      seam).
- [ ] AppleContainer substrate research (further-future plan).
- [ ] Egress secret detection + redaction layer (memory
      `project_egress_secret_detection_is_core`).
