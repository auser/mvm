# Plan 73 — SDK Port Follow-ups

**Status**: Active follow-up to the merged SDK port branch
**Date**: 2026-05-13
**Cross-refs**: Plan 72 (builder VM via libkrun), SDK port commits Phase 1a → 10c on main

## Context

The SDK port from `../mvmforge/` landed on main as a single PR covering
Phases 1a through 10c. The port covered everything that didn't
require the libkrun builder VM to actually boot:

| Phase | Scope | State |
|-------|-------|-------|
| 1a    | Reserve `Hooks` + `HookCmd` IR field shapes | ✅ landed |
| 1b    | Port `mvmforge-addon` machinery → `mvm-sdk::addon` | ✅ landed |
| 2a    | Port compile rendering primitives | ✅ landed |
| 2b    | Port compile orchestration layer | ✅ landed |
| 2c    | `mvmctl compile <entry>` verb | ✅ landed |
| 4     | Python `@mvm.app` decorator parser | ✅ landed |
| 4b    | TypeScript `mvm.app({...})(fn)` decorator parser | ✅ landed |
| 5     | Python SDK helpers + hook kwargs | ✅ landed |
| 6     | TypeScript SDK helpers + hook kwargs | ✅ landed |
| 7a    | Rust runtime record-mode core | ✅ landed |
| 7b    | Python `Sandbox` class (record mode) | ✅ landed |
| 7c    | TypeScript `Sandbox` class (record mode) | ✅ landed |
| 7d    | `mvmctl compile --from-recording` | ✅ landed |
| 7e    | Python auto-exec for Sandbox-shaped scripts | ✅ landed |
| 7f    | Node + TypeScript auto-exec | ✅ landed |
| 8 (stub) | `mvmctl deploy` — single archive + `mvmd-spec.json` + log-only client | ✅ landed |
| 8 followup | Examples (hello-app) + SDK guide + mvmforge-migration guide + tombstone | ✅ landed |
| 9 (primitives) | `deps_audit` types + `seal_volume`/`verify_sealed_volume` + ADR-047 | ✅ landed |
| 10a   | Addon-aware hook merger + launch.json hook emission | ✅ landed |
| 10b   | Lifecycle-hook consumer wiring in `mkFunctionService` + `mkFunctionWorkload` | ✅ landed |
| 10c   | Readiness + shutdown hook runners (`mvm-guest::lifecycle_hooks`) | ✅ landed |

The remaining items in this plan cover everything that **could not
ship in the SDK port PR** because they depend on Plan 72
(builder-vm-via-libkrun) being hardware-validated end to end, or on
follow-on work that was deliberately scoped out to keep the SDK PR
landable.

## Followup A — `mvm-supervisor` admission verifier for dep volumes (claim 9)

The Phase 9 primitives in
`crates/mvm-sdk/src/compile/deps_audit.rs` ship
`seal_volume` + `verify_sealed_volume` + `derive_volume_hash` plus
12 tamper-detection unit tests, but the admission gate isn't wired
yet. Tasks:

- Extend `mvm-plan::ExecutionPlan` to carry the deps `VolumeRef`
  + `manifest.sha256` for any workload that mounts a deps volume
  at `/app/.venv` or `/app/node_modules`.
- Wire `mvm-supervisor`'s admission verifier to call
  `verify_sealed_volume(volume_dir)` before launching, comparing
  the derived hash against the plan's recorded value. Reject
  closed on mismatch.
- Emit `plan.admitted` audit-chain entries that record both the
  volume hash and the manifest sha so `mvmctl audit verify`
  detects drift.
- **Test gate:** hand-tamper to `cve.json` on a sealed volume
  makes the next `mvmctl up` fail with `E_VOLUME_TAMPERED`; the
  audit chain entry pins both hashes.

**Blocked on:** Plan 72 W4/W5 — without volumes to admit, this is
dead code.

## Followup B — Builder-VM-side install pipeline

The Phase 9 ADR-047 calls out a build-time pipeline that runs
inside the libkrun builder VM:

- `install_app_deps` phase in `crates/mvm-build/src/builder_vm.rs`
  that mounts the user's lockfile + source, runs the language
  installer (`uv pip install --no-deps`, `pnpm install
  --frozen-lockfile`) behind a strict egress allowlist (`pypi.org`,
  `files.pythonhosted.org`, `registry.npmjs.org`,
  `objects.githubusercontent.com`).
- Sealed-artifact emission: SBOM via `cyclonedx-py` /
  `pnpm sbom`; CVE scan via `pip-audit` / `pnpm audit --json`;
  fetch log via HTTP-client interception; `meta.json` rolled up
  via `seal_volume`.
- Gate semantics: `--prod` fails closed on missing attestations
  (PEP 740 / npm provenance) or high/critical CVEs; `--dev` warns.

**Blocked on:** Plan 72 W4/W5 — needs working builder VM to install
into. Pure-Rust primitives are ready (Phase 9 commits); this
followup is the in-VM glue.

## Followup C — `mvmctl deps` CLI verbs

User-facing surface for the volume audit pipeline:

- `mvmctl deps audit [--all | <volume_hash>]` — re-runs the CVE
  scan against the current feed, rewrites `cve.json`, rolls the
  volume hash, emits an audit-chain entry. Background `cron`
  variant for long-lived deployments.
- `mvmctl deps inspect <volume_hash>` — pretty-prints SBOM +
  fetch log + last-audit-time. Works against the local volume
  cache without a VM spawn.
- `mvmctl build --deps` — rebuild the deps volume on demand
  without re-running the full `nix build`.

**Blocked on:** Followup B (and therefore Plan 72 W4/W5) — no
volumes to inspect until the install pipeline lands.

## Followup D — CI gate: `.github/workflows/security.yml::app-deps-audit`

Mirrors the existing `cargo-deny` / `cargo-audit` jobs but for
application deps. Builds each example workload, asserts the four
sealed artifacts (`content/`, `sbom.cdx.json`, `fetch.log`,
`cve.json`) emit cleanly and no example's CVE scan returns
high/critical findings. Adds claim 9 to the security posture
report.

**Blocked on:** Followup B — CI needs a working install pipeline
to exercise.

## Followup E — Wire `lifecycle_hooks` into the worker-pool admission path

The Phase 10c primitives in
`crates/mvm-guest/src/lifecycle_hooks.rs` ship `poll_readiness`
(after_start probe) and `run_shutdown_hook` (before_stop), with
8 unit tests against shell stubs on the host. They are
**not yet called by `worker_pool.rs`**: the worker pool currently
accepts invokes immediately on warmup without polling the
after_start probe; SIGTERM handling doesn't fire before_stop.

Tasks:

- Extend `crates/mvm-guest/src/worker_pool.rs` to call
  `poll_readiness({ script_path: "/etc/mvm/hooks/after_start.sh",
  timeout: 30s, interval: 200ms })` between "pool spawned" and
  "ready to dispatch." Slow-warming workloads now block invokes
  until they say they're ready.
- Add a SIGTERM handler to the guest agent that fires
  `run_shutdown_hook("/etc/mvm/hooks/before_stop.sh", grace=10s,
  poll_interval=200ms)` before the agent exits. Best-effort;
  SIGKILL bypasses, by design.
- **Test gate:** boot a hand-authored workload whose
  `after_start.sh` exits 1 thrice then 0; the worker pool refuses
  invokes for the first ~600ms then accepts. A second workload
  whose `before_stop.sh` writes a marker file proves shutdown
  hooks fired on clean teardown.

**Blocked on:** Plan 72 W4/W5 — the full hooks-in-a-real-VM
integration test needs a working microVM. Primitives are
testable on the host today (see commit `9c82e48`).

## Followup F — TypeScript auto-exec runner installation guidance

Phase 7f's `mvmctl compile <script.ts>` requires a TS-aware runner
on PATH (`tsx`, `bun`, or `deno`). Today the CLI's error message
points users at `npm install -g tsx`; we should:

- Document the runner-installation choices in
  `public/src/content/docs/guides/sdk.md` (per-OS install
  recipes).
- Add a `mvmctl doctor` check that surfaces "no TS runner found"
  with the same install hint.
- Consider auto-resolving `tsx` from a project-local
  `node_modules/.bin/tsx` before falling back to PATH lookup
  (lets users pin the runner via their `package.json`).

**Blocked on:** nothing — straightforward CLI polish. Defer
because the v1 surface is functional without it.

## Followup G — Per-language base image registry beyond v1's closed list

`mvm_sdk::runtime::resolve_base_image` ships a closed v1 list:
`python-3.12`, `python-3.13`, `node-22`, `node-lts`, `minimal`.
The plan's "Well-known base-image trust" consideration calls for
a `mvmctl image push <template>` flow that lets users register
their own base templates (cosign-signed). Out of scope for v1;
tracked here as a followup once the trust model is settled.

**Blocked on:** trust-model design + a signing flow that doesn't
exist yet.

## Followup H — Live + plan modes for `mvmctl run`

Phase 7's record mode is end-to-end for Python + TypeScript. The
SDK plan also defined two other modes:

- **Live** (default for `mvmctl run`): SDK shells `Sandbox`
  operations to existing `mvmctl up`/`exec`/`down`/etc. Needs
  the dev VM working — currently blocked because `mvmctl dev up`
  is mid-libkrun-transition.
- **Plan** (`--mode plan`): synthesizes one `ExecutionPlan` per
  `Sandbox` operation via `mvm_supervisor::admit_for_run`.
  Useful for dry-running admission gates without booting a VM.

The Python + TypeScript SDKs already throw `SandboxModeError`
with a "blocked on Plan 72" message when these modes are
requested, so the surface is reserved.

**Blocked on:** Plan 72 W4/W5 (live mode) + extending the
supervisor for record-mode-style plan synthesis (plan mode).

## Followup I — Cross-corpus parity drop

The original SDK port plan included a Phase-1-only
`xtask cross-corpus-check` task that asserted IR fixtures from
`../mvmforge/crates/mvmforge-ir/tests/` round-trip identically
through the ported types. The cross-corpus check was a one-shot
sanity gate, not a permanent contract — per the plan's "No
backwards compatibility" decision, the SDK port owns its own
corpus going forward.

If a follow-on Plan 73-J adds back a mvmforge → mvm migration
helper for any users still on the legacy package name, the
fixture parity check can be reused. Until then this is closed.

## Followup J — `mvmforge` user migration guidance

The SDK port plan deliberately dropped back-compat: no
`MVMFORGE_*` env-var aliases, no `mvmforge` Python re-export, no
`@mv.func()` decorator alias. If any external users surface, a
short migration note + an optional one-shot rename script could
land here. Out of scope until that demand materializes.

## Plan 72 dependency chain

Most of this plan's followups thread through Plan 72:

```text
Plan 72 W4 (network/vsock plumbing)
   ↓
Plan 72 W5 (cutover: rename builder/ → dev-shell/, wire find_builder_vm_flake)
   ↓
   ├── Followup A (admission verifier)
   ├── Followup B (builder-VM install pipeline)
   ├── Followup C (mvmctl deps CLI)
   ├── Followup D (CI gate)
   ├── Followup E (worker_pool readiness)
   └── Followup H-live (mvmctl run --mode live)
```

Plan 72 W4/W5 are the unblocker for most of this plan. Followups
F, G, H-plan, I, J are independent of Plan 72.

## Test gates (rolled up)

When the followups land, the workspace gate adds:

- `cargo test --workspace` continues to pass.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `mvmctl audit verify` reports clean for a deps-volume-bound
  workload (Followup A).
- The Phase 9 acceptance commands from the SDK port plan exercise
  end-to-end (`mvmctl build` produces a sealed volume; a
  hand-tamper to `cve.json` makes the next `mvmctl up` fail
  closed; `mvmctl deps audit --all` re-scans the cache).
- The Phase 10c acceptance commands exercise end-to-end
  (`mvmctl up hello-lifecycle` waits for `after_start.sh` to
  exit 0 before serving invokes; `mvmctl down` fires
  `before_stop.sh`).
