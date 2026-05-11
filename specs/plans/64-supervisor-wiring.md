# Plan 64 — wire mvmctl up → ExecutionPlan → Supervisor::launch

> Plan 60's two cornerstones (`mvm-plan::ExecutionPlan` and
> `mvm-supervisor::Supervisor::launch`) are **shipped as crates** —
> 256 tests cover the substrate. The gap is integration:
> `mvmctl up` doesn't construct plans, doesn't drive the supervisor,
> and the audit / inspector / tool-gate substrate sits idle in
> production paths.
>
> Plan 64 closes that gap. Roughly 12 days, 6 workstreams.

**Status (2026-05-11)**: W1–W4 + W6 shipped — every `mvmctl up`
admits a signed `ExecutionPlan` and emits a chain-signed audit
trail; CLAUDE.md security claim 8 is true and tracked. **W5
(`PolicyRef → concrete component slots`) remains open** as the
final workstream before plan 64 closes. ADR-041
(`specs/adrs/041-signed-audited-execution-plans.md`) documents
the shipped surface and the `*Ref` semantics gap that W5 + plan
60 Phase 3 close.

| Workstream | Status | Landing commits |
|---|---|---|
| W1 — `ExecutionPlan` synthesis | ✅ shipped 2026-05-11 | ae81767 |
| W2 — host-side signing keypair | ✅ shipped 2026-05-11 | a71e60a |
| W3 — admission substrate + callsite | ✅ shipped 2026-05-11 | 2671f5f, bc91d77 |
| W4 — `FileAuditSigner` + `mvmctl audit` | ✅ shipped 2026-05-11 | 587a33e |
| W5 — `PolicyRef` resolver | ⏳ open | — |
| W6 — verification + ADR-041 + plan 60 Phase 6 mark-up | ✅ shipped 2026-05-11 | (this commit) |

## Discovery context

The 2026-05-11 cutover survey expected to cherry-pick `mvm-plan` and
`mvm-supervisor` from `legacy/v1`. Investigation showed both crates
were ported into v2 via Phase 0 commit `1c3e00c` and have been
extended since (28 plan tests + 228 supervisor tests in v2 vs.
19+19 in the v1 branches). The cherry-picks were busywork — the
substrate is already here.

What's missing: integration. Confirmed by
`grep "use mvm_plan\|use mvm_supervisor" crates/mvm-cli/src` →
zero matches. `mvmctl up` still routes through `backend.start()`
with `RunParams`-shaped args; the supervisor + plan crates are
islands.

## State of play

### Already in `origin/main` (substrate)

- `mvm-plan` — `ExecutionPlan`, `SignedExecutionPlan`, `sign_plan` /
  `verify_plan`, every `*Ref`/`*Spec` placeholder, `NonceStore` +
  `check_window` (G4 replay protection). 28 tests.
- `mvm-supervisor` — `Supervisor::launch` happy path
  (verify → check_window → nonce_check → state machine → backend
  dispatch). Full inspector chain (`SecretsScanner`, `SsrfGuard`,
  `InjectionGuard`, `PiiRedactor`), `L7EgressProxy`,
  `PolicyToolGate`, `CircuitBreaker`, `AuditSigner` +
  `FileAuditSigner` + `audit_dedup`, `Reaper`, `ArtifactCollector`.
  228 tests.
- All component slots default to `NoopXxx` impls (fail-closed when
  consulted; no-op otherwise). The fail-closed default means a
  misconfigured supervisor can't accidentally pass tenant traffic
  through an unwired component.

### Missing (integration)

1. No `ExecutionPlan` is ever synthesized. `mvmctl up` flag parsing
   ends at `VmStartConfig` / `FlakeRunConfig`; nothing constructs
   the typed plan.
2. No signing path. The crate exposes `sign_plan`/`verify_plan` but
   no host-side keypair is provisioned; nothing on disk holds a
   signer key.
3. `Supervisor::launch` has no caller in `mvm-cli`. The supervisor
   is `Default::default()`-only — every slot is Noop.
4. The audit chain doesn't fire. `FileAuditSigner` would write
   plan-bound entries; no caller installs it.
5. `EgressProxy`, `ToolGate`, `KeystoreReleaser` slots are never
   replaced by real impls because no policy resolver maps
   `PolicyRef` → concrete configuration.

## Workstreams

Six workstreams, each independently mergeable.

### W1 — `ExecutionPlan` synthesis in `mvm-cli` (~2 days)

**Goal**: a function that turns `mvmctl up` CLI args (flake ref,
cpus, memory, ports, volumes, name) into a typed `ExecutionPlan`.

**Action**:

- New `crates/mvm-cli/src/commands/vm/plan_builder.rs`:
  `synthesize_plan(args: &UpArgs) -> Result<ExecutionPlan>`.
- Plan fields populated from CLI:
  - `plan_id`: fresh `Uuid::new_v4()` per invocation
  - `workload_id`: derived from the flake ref or `--name`
  - `tenant_id`: from `--tenant` flag (default `"local"` per ADR-002
    "one guest = one workload")
  - `resources`: cpus/memory/disk from flags
  - `runtime_profile_ref`: derived from flake's `passthru.mvm.profile`
    if present, else `"default"`
  - `image_ref`: `SignedImageRef` synthesized from the built rootfs
    path + SHA-256 (computed on first read; cached in the sidecar)
  - `policy_ref`: `"local-default"` until policy resolver lands
    (W5); `NoopXxx` slots stay Noop
  - `valid_from`/`valid_until`: now + 10 minutes (well past boot
    time; G4 protects against captured plans replayed days later)
  - `nonce`: 96-bit random per-invocation
- Plan canonical-JSON shape is fixed by `mvm-plan` already; no
  changes to the wire there.

**Exit tests**:

- `synthesize_plan_round_trips_through_serde` — build, serialize,
  parse, compare.
- `synthesize_plan_carries_cli_resource_overrides` — pinning
  `--cpus 4 --memory 2048` flows through.
- `synthesize_plan_generates_unique_plan_id_per_call`.
- `synthesize_plan_uses_default_validity_window_of_10_minutes`.
- `synthesize_plan_nonce_is_96_bits_and_random`.

### W2 — Host-side signing keypair (~1 day)

**Goal**: every `mvmctl up` plan is signed with a host-local
Ed25519 keypair on first use. Stored at `~/.mvm/keys/host-signer.{ed25519,pub}`
mode 0600 / 0644.

**Action**:

- New `mvm-cli::commands::vm::signer`:
  `load_or_init_host_signer() -> Result<SigningKey>`. Generates on
  first use with `OsRng`, writes both halves, idempotent on repeat.
- Plug into `synthesize_plan` from W1: caller signs the plan with
  this key, supplies the verifying half plus a signer_id of
  `"host:{hostname}"` to `Supervisor::launch`.
- `~/.mvm/keys/` directory gets 0700 perms via
  `mvm_core::config::ensure_data_dir` (already enforces this for
  the parent — pattern carries forward).

**Exit tests**:

- `load_or_init_creates_keys_with_correct_modes` (0600 for signing,
  0644 for verifying, 0700 for the dir).
- `load_or_init_idempotent_on_second_call`.
- `host_signer_signs_plan_envelope_verifiable_via_pubkey`.
- `host_signer_refuses_loose_perms_above_0600` — chmod 0644 +
  reopen → error (consistent with `snapshot_hmac::load_or_init_key`
  shape).

### W3 — `mvmctl up` dispatches through `Supervisor::launch` (~3 days)

**Goal**: every `mvmctl up` invocation routes through the
supervisor. The launch path becomes:

```
mvmctl up <flake>
   ↓ synthesize_plan(args)                    [W1]
   ↓ load_or_init_host_signer()               [W2]
   ↓ sign_plan(plan, signer)
   ↓ Supervisor::new()
   ↓    .with_backend(BackendLauncher::new(AnyBackend::auto_select()))
   ↓    .with_audit(FileAuditSigner::new(~/.mvm/audit.log))
   ↓ supervisor.launch(&signed, &trusted_keys).await
```

**Action**:

- Refactor `mvm-cli::commands::vm::up::run` to construct a `Supervisor`
  with at least `BackendLauncher` + `FileAuditSigner` wired in.
- New `BackendLauncher` adapter in `mvm-cli::commands::vm::backend_adapter`
  that translates the supervisor's `BackendLauncher` trait
  (`async fn launch(&self, plan: &ExecutionPlan) -> Result<()>`) into
  a call to today's `mvm_backend::AnyBackend::start()` with a
  `VmStartConfig` derived from the plan.
- Backward compat: a `--no-supervisor` escape hatch keeps the old
  path working for one release. Flag prints a deprecation warning.
- `mvmctl down`, `mvmctl ls` similarly route through supervisor
  (`stop`, `status`).

**Exit tests**:

- `mvmctl_up_synthesizes_and_signs_plan` (parses CLI, no boot).
- `mvmctl_up_supervisor_launch_invokes_backend` (mocked
  `BackendLauncher` records the plan it received; assertion that
  the plan's resources match the CLI).
- `mvmctl_up_rejects_replayed_signature` — call `up` twice with the
  *same* nonce (via test seam) → second invocation refused with
  nonce-replay error.
- `mvmctl_up_no_supervisor_escape_hatch_works` — flag bypass.

**Risk**: existing tests in `mvm-cli::commands::tests` may assume the
direct `backend.start()` shape. Audit + update.

### W4 — `FileAuditSigner` wired (~2 days)

**Goal**: every `Supervisor::launch` call emits at least
`plan.accepted` and `plan.launched` audit entries to
`~/.mvm/audit.log`, signed under the host signer key. Tampering
breaks `verify_audit_chain`.

**Action**:

- `mvm-cli::commands::vm::up` constructs the supervisor with
  `.with_audit(FileAuditSigner::new(~/.mvm/audit.log, host_signer))`.
- New `mvmctl audit tail` and `mvmctl audit verify` CLI surface
  consuming `mvm_supervisor::audit_file::verify_audit_chain`.
- Audit shape: each entry binds `plan_id`, `signer_id`,
  `nonce`, `state_transition` (e.g. `Pending→Verified`),
  `timestamp_utc`. The signer of the audit chain MAY differ from
  the signer of the plan; we'll start with same-key for v0 and
  split keys in plan 60 Phase 3.

**Exit tests**:

- `audit_log_carries_plan_id_for_every_launch`.
- `audit_chain_verifies_clean`.
- `audit_chain_rejects_inserted_line` (sketch a fresh log, splice a
  forged entry, verify fails).
- `mvmctl_audit_tail_prints_recent_entries`.
- `mvmctl_audit_verify_returns_nonzero_on_tampered_chain`.

### W5 — Component slot plumbing from `PolicyRef` (~3 days)

**Goal**: `EgressProxy`, `ToolGate`, `KeystoreReleaser`,
`ArtifactCollector` slots resolve to concrete impls when the
`PolicyRef` field of the plan names a policy that requires them.
First cut keeps Noop semantics in production paths but adds the
resolver substrate so plan 63's encryption work has a place to
hang.

**Action**:

- New `mvm-cli::commands::vm::policy_resolver`:
  `resolve_supervisor_components(plan: &ExecutionPlan) -> (EgressProxy, ToolGate, KeystoreReleaser, ArtifactCollector)`.
- Today this returns Noop for everything (no policy file loaded yet).
- When `policy_ref = "local-default"`: returns `NoopEgressProxy`,
  `NoopToolGate`, `NoopKeystoreReleaser`, `NoopArtifactCollector`.
- When `policy_ref = "<tenant>:<workload>"`: loads
  `~/.mvm/policies/<tenant>/<workload>.toml` (file shape: workstream
  for plan 60 Phase 3) and returns real impls. Unimplemented for
  v0 — return clear `NotYetImplemented` error.
- `Supervisor::with_*` builder methods get called from `mvmctl up`
  with whatever the resolver returns.

**Exit tests**:

- `policy_resolver_returns_noops_for_local_default`.
- `policy_resolver_rejects_tenant_policy_ref_with_clear_error`
  (until v1 of the policy file lands).
- `supervisor_with_resolved_slots_carries_plan_id_to_audit`.

### W6 — Verification + ADR update + plan 60 phase mark-up (~1 day)

**Goal**: Phase 6 cornerstones are user-observably true.

**Action**:

- `mvmctl audit tail` shows the `plan.launched` line referencing the
  plan that just booted.
- `mvmctl plan show <plan_id>` (new) dumps the parsed plan from the
  audit log for any plan ever launched on the host.
- ADR-040 (new) documents the integration: who builds plans, who
  signs, who consumes, what each `*Ref` field means today vs. plan
  60 Phase 3 (where policy and key resolvers land).
- `CLAUDE.md` "Security model" section grows: claim 8 (proposed) —
  *every workload runs from a signed, audited `ExecutionPlan`*.
- `specs/plans/60-mvm-microsandbox-migration.md` Phase 6 marked
  shipped.

**Exit tests** (these are integration-level):

- `mvmctl up && mvmctl audit tail` shows plan + state transitions.
- `mvmctl up && tamper with audit.log && mvmctl audit verify` fails.
- `mvmctl up && replay same signed plan via test seam` rejected.
- `cargo test --workspace --no-fail-fast` passes (target: ≥ 1980).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo run -p xtask -- check-no-display-on-secret-types` clean.

## Phasing

W1 → W2 (W2 depends on W1's plan shape). W3 depends on both.
W4 + W5 are independent and parallelizable after W3. W6 is
verification and closes the plan.

Suggested order:
**W1 → W2 → W3 → W4 → W5 → W6**.

12-day total fits a single sprint. Each workstream lands as one
PR + one tests-green checkpoint.

## Cross-repo dependencies

- **mvmd**: consumes `mvmctl::plan::synthesize_plan` for its
  remote-launch path. The `mvm-plan` wire format is already shared.
  Plan 64 doesn't change anything mvmd consumes; mvmd's `cargo build`
  blocker (sha2 dep conflict, separate issue) is orthogonal.
- **mvmforge**: function-call entrypoint workloads will carry an
  IR-derived plan shape. Plan 64's `synthesize_plan` defines the
  reference implementation; mvmforge's generated plans must satisfy
  the same schema. No coordination needed for plan 64 itself.

## Non-goals (explicit)

- **Real policy file format**. `policy_ref` resolves to Noop slots
  until plan 60 Phase 3 lands.
- **Multi-signer audit chain**. The host signer signs both plans
  and audit lines. Splitting these (plan-signer = mvmd; audit-
  signer = host) is Phase 3.
- **Trusted clock**. `Supervisor` uses `SystemClock` today; trusted
  clock via `HostBoundRequest::QueryHostTime` is a plan 60 Phase 3
  vsock-level addition.
- **Attestation**. `AttestationRequirement` field is read but
  ignored (defaults to `None`). TPM2 / SEV-SNP / TDX lands in
  Phase 3.
- **Plan-bound key release**. `KeystoreReleaser::release_for_plan`
  is plumbed but Noop — actual release ties to plan 63 W3
  (keyring).
- **mvmctl plan create / sign / verify CLI**. The synthesis path
  is internal-only for v0. User-facing plan CLI is a follow-up.

## Success criteria

By plan 64 close, the project can claim:

1. *Every `mvmctl up` invocation produces a signed `ExecutionPlan`
   with replay protection (`valid_from`/`valid_until`/`nonce`).*
2. *Every workload's audit entry binds to that plan's `plan_id`.*
3. *`mvmctl audit tail` shows the bound chain; `mvmctl audit verify`
   detects tampering.*
4. *The supervisor's component slots resolve from policy at launch
   time; the default `local-default` policy uses fail-closed Noops
   that don't bypass anything.*
5. *Plan 60 Phase 6 (signed plans / supervisor cornerstones) is
   shipped, not just substrate-true.*

Closes plan 60 Phase 6. Phase 2 (plan 63) and Phase 3
(network isolation) layer on top — `KeyRotationSpec` and `PolicyRef`
fields are already in `mvm-plan::types`.
