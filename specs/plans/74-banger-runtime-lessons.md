# Plan 74 - Banger runtime lessons for mvm

> Status: active
> Owner: TBD
> Started: -
> Depends on: ADR-002, ADR-007, ADR-050, plan 41, plan 52, plan 64, plan 72
> Inputs: survey of `thaloco/banger` public GitLab mirror

## Why

`banger` is a small watch-party runtime built around a useful shape:
a thin local control process wraps a mature runtime (`mpv`), typed
messages carry control state, large media bytes move through a
separate streaming path, version mismatch is detected early, and
users see the system's current wait reason ("connected", "buffering",
"cache ready") instead of a silent hang.

The media domain is not relevant to `mvm`. The runtime pattern is:

1. Keep control messages small, typed, and versioned.
2. Move large data over dedicated streaming paths.
3. Treat backpressure as product state, not just socket plumbing.
4. Expose readiness transitions directly to users.
5. Make the first-use path feel native to the user's actual workflow,
   not like a separate infrastructure project.

The DX lesson is just as important as the protocol lesson. Banger does
not ask users to learn a new control plane before they can watch
something together: install `mpv`, drop in the scripts folder, launch
the app they already understand, and press a shortcut. For `mvm`, the
equivalent is not an mpv plugin; it is a short path from source code or
a shell command to "this ran in a microVM", with progress, receipts,
state, and cleanup handled by the tool.

The trust model is explicitly **not** something to copy. `banger`
uses self-signed TLS with verification skipped, header-based identity,
and ad hoc media identifiers. `mvm` must preserve its existing signed
plans, vsock-only control path, `deny_unknown_fields`, audit chain,
and fail-closed defaults.

This plan turns the useful runtime lessons into incremental `mvm`
workstreams.

## Sources surveyed

- `banger` main branch README: host/client UX, release shape,
  mpv plugin integration.
- `internal/common/messages.go`: typed message envelope with payload
  variants and timestamp.
- `internal/host/http.go`: control API + byte-range media data path.
- `internal/client/mpv.go` and `internal/host/client.go`: cache /
  buffering state reported as protocol messages.
- `wasm` branch README and `web/libmpv`: experimental "native runtime
  behind browser surface" packaging, plus its documented performance
  problems.

## Non-goals

- No TCP control plane. Host to guest control remains vsock or the
  backend-specific equivalent already abstracted by `VsockTransport`.
- No relaxed TLS/auth posture. This plan does not introduce skipped
  certificate verification, shared bearer tokens, or header-only auth.
- No browser UI work. The `wasm` branch is useful as a packaging
  warning, not as a near-term product target.
- No broad CLI redesign. The plan adds clearer states and errors to
  existing commands first.

## Recommended additions, in order

1. **Runtime readiness model** - distinguish backend launch,
   guest-agent reachability, service warmup, ready, degraded, and
   stopping without changing coarse lifecycle state.
2. **`mvmctl explain`** - turn receipts, protocol state, builder mode,
   readiness, and last error into a short cause + next-action report.
3. **Structured progress events** - make CLI, JSON, Studio, MCP, logs,
   and receipts render from one event vocabulary instead of separate
   strings.
4. **Receipts for more workflows** - ensure `build`, `up`, `run`,
   `invoke`, and copy-like workflows can leave redacted, verifiable
   summaries.
5. **Backpressure as user-visible state** - expose typed wait reasons
   such as service warming, queue full, output consumer slow, input
   buffer full, builder busy, or artifact transfer blocked.
6. **Builder-mode policy and doctor checks** - keep the managed builder
   VM as default, make `host-nix` explicit, and explain the security /
   reproducibility tradeoff when selected.
7. **First-use happy paths** - define three-command paths for CLI,
   Python SDK, TypeScript SDK, and bundle users that start from their
   workflow rather than architecture.
8. **Control-plane / data-plane cleanup** - document and enforce small
   typed control frames, with large logs/files/artifacts moving over
   bounded streaming or transfer paths.

Start with item 1. It produces immediate DX value, gives later
`explain` and receipt work something stable to read, and can land as a
small compatibility-preserving state-model slice.

## Acceptance criteria (whole-plan)

1. Every host to guest session begins with an explicit protocol hello
   or compatibility shim, and version/capability mismatch returns a
   typed error.
2. `mvmctl up`, `mvmctl ls/status`, and JSON output can distinguish
   backend launch accepted, guest agent reachable, services starting,
   services ready, degraded health, and shutdown.
3. Large output/file/artifact paths have documented data-plane
   handling and do not rely on unbounded single-frame control
   responses.
4. At least one high-volume operation, preferably `ProcWait` or
   `RunEntrypoint`, exposes structured backpressure/wait reasons.
5. First-use DX has an accepted flow with a three-command happy path,
   clear failure recovery, and no hidden prerequisite beyond `doctor`
   checks.
6. Builder mode is explicit: managed builder VM by default, host Nix
   only by opt-in, and no silent trust-boundary changes from
   auto-detection.
7. Progress, receipts, and error explanations are structured enough to
   power CLI, JSON output, Studio, MCP, and logs from the same event
   vocabulary.
8. Docs describe protocol versioning, readiness states, and
   control-plane/data-plane boundaries.
9. `cargo test --workspace`, `cargo check --workspace`, and clippy
   remain clean. Linux-only live coverage stays gated for the builder
   VM / Firecracker cases.

---

## W0 - ADR: guest protocol versioning and runtime readiness

**Goal**: record the architectural contract before touching the wire.

### Action

- New ADR:
  `specs/adrs/050-guest-protocol-versioning-and-readiness.md`.
- Define:
  - protocol version bump policy;
  - capability negotiation rules;
  - compatibility window for older guest agents;
- runtime readiness state taxonomy;
- control-plane vs data-plane boundary;
- backpressure event vocabulary.
- builder mode policy:
  `managed-builder-vm` default, `host-nix` explicit opt-in,
  `remote-builder` future;
- compatibility window for protocol hello and older guest images;
- structured progress and receipt vocabulary.
- Security constraints:
  - keep `#[serde(deny_unknown_fields)]`;
  - keep frame caps;
  - never log payload bytes or secrets in readiness/backpressure
    status;
  - unknown capability must fail closed unless explicitly optional.

### Exit tests / checks

- ADR accepted before W1 lands.
- ADR cross-links ADR-002, ADR-007, plan 41, and plan 52.

---

## W1 - Protocol hello and capability negotiation

> Status: in progress - protocol wire types, agent ack path, persistent
> per-connection request handling, and stream negotiation helper have
> landed. FS RPC, process RPC, run-entrypoint, console, and
> idle-timeout call sites now require their matching capabilities.
> Volume call-site migration remains pending until live attach/detach
> dispatch lands.

**Goal**: every new guest-agent session starts by proving protocol
compatibility before accepting operational requests.

### Proposed wire shape

Add request/response variants in `crates/mvm-guest/src/vsock.rs`:

```rust
GuestRequest::ProtocolHello {
    host_protocol_version: u32,
    min_supported_version: u32,
    host_version: String,
    requested_capabilities: Vec<GuestCapability>,
}

GuestResponse::ProtocolHelloAck {
    agent_protocol_version: u32,
    min_supported_version: u32,
    agent_version: String,
    capabilities: Vec<GuestCapability>,
}

GuestResponse::ProtocolMismatch {
    host_protocol_version: u32,
    agent_protocol_version: u32,
    required_action: ProtocolUpgradeAction,
    message: String,
}
```

`GuestCapability` is a closed enum, initially covering existing
surfaces:

- `Ping`
- `IntegrationStatus`
- `EntrypointStatus`
- `RunEntrypoint`
- `FilesystemRpc`
- `ProcessRpc`
- `Console`
- `VolumeMount`
- `UpdateIdleTimeout`

### Action

- Add `mvm_guest::vsock::PROTOCOL_VERSION` and
  `MIN_SUPPORTED_PROTOCOL_VERSION`.
- Add a helper used by CLI call sites:
  `negotiate_protocol(stream, requested_capabilities)`.
- Keep `Ping` working as a temporary compatibility shim for older
  agents during one release window. New code paths that require new
  capabilities must call hello first.
- Update `crates/mvm-cli/src/commands/shared/vsock.rs`,
  `vm/session.rs`, `vm/proc.rs`, `vm/cp.rs`, and any helper that
  opens fresh guest-agent sessions.

### Exit tests

- Serde roundtrip for hello/ack/mismatch.
- Unknown field rejection for all new types.
- `old_agent_ping_compat_still_works` for the compatibility window.
- `protocol_mismatch_reports_upgrade` and
  `protocol_mismatch_reports_downgrade`.
- Capability request for an unsupported mandatory capability fails
  before dispatch.

---

## W2 - Runtime readiness state model

**Recommended starting point after W1.** This is the next thing to add
because it gives immediate user-visible DX improvement and creates the
state foundation for `mvmctl explain`, structured progress events,
receipts, and backpressure. The first slice should be intentionally
small: persist readiness on instance state, expose it in JSON, and
render a few progress lines during `up` without changing backend
lifecycle semantics.

**Goal**: expose the wait reason instead of collapsing everything into
`running` or a timeout.

### Proposed states

Keep `InstanceStatus` as the coarse lifecycle enum and add a nested
readiness model:

```rust
pub enum InstanceReadiness {
    LaunchAccepted,
    AgentConnecting,
    AgentReady,
    ServicesStarting { pending: Vec<String> },
    ServicesReady,
    Degraded { unhealthy: Vec<String> },
    Backpressured { reason: BackpressureReason },
    Stopping,
}
```

`InstanceStatus::Running` means the backend accepted the VM and it has
not stopped. `InstanceReadiness` explains whether it is usable.

### Action

- Add readiness fields to `InstanceState` with serde defaults for
  backward compatibility.
- Update transition validation only where needed; readiness changes
  should not require illegal coarse lifecycle transitions.
- Thread readiness through `mvmctl ls/status --json`.
- During `mvmctl up`, print concise progress:
  - backend accepted launch;
  - waiting for guest agent on vsock port;
  - agent ready;
  - waiting for integration health;
  - services ready or degraded.

### Exit tests

- Legacy `InstanceState` JSON still deserializes.
- Readiness enum roundtrips in JSON.
- `mvmctl status --json` fixture includes readiness.
- Health reports map to `ServicesStarting`, `ServicesReady`, and
  `Degraded` as expected.

### First implementation slice

Keep this slice scoped to data shape and display:

1. Add `InstanceReadiness` in the crate that owns persisted instance
   state, with serde defaults so existing `~/.mvm/instances/*` JSON
   continues to load.
2. Add a `readiness` field to `InstanceState` or the nearest persisted
   instance record. Default old records to `AgentConnecting` or the
   closest existing status-derived state.
3. Update state writes in the `up` path at these milestones:
   backend launch accepted, guest-agent connect loop started,
   guest-agent reachable, integration health starting, services ready
   or degraded.
4. Thread readiness into `mvmctl status --json` and `mvmctl ls --json`.
   Human output can stay conservative in the first slice; only add
   one compact readiness column or suffix if the existing layout has a
   clear place for it.
5. Add tests for legacy JSON compatibility, readiness roundtrip, and
   health-to-readiness mapping.

Do not introduce `mvmctl explain` in this slice. Reserve that for W6
after the readiness field is persisted and observable.

### New-session prompt

Use this prompt to start W2 in a fresh Codex session:

```text
We are in /Users/auser/work/tinylabs/mvmco/mvm. Follow AGENTS.md:
feature work in a sibling worktree, git only from the main checkout
with `git -C <worktree>`, cargo on the macOS host by default, and no
Nix/microVM/mvmctl runtime commands outside the builder VM.

Please continue Plan 74 from:
specs/plans/74-banger-runtime-lessons.md

Start W2: Runtime readiness state model. Keep the first slice scoped:
add a serde-compatible `InstanceReadiness` model to persisted instance
state, default legacy records safely, update the `up` path to persist
readiness milestones for backend launch accepted, guest-agent
connecting/reachable, services starting, services ready/degraded, and
expose readiness in `mvmctl status --json` and `mvmctl ls --json`.

Do not implement `mvmctl explain` yet. Add focused tests for legacy
JSON compatibility, readiness roundtrip, and health-to-readiness
mapping. Run `cargo fmt`, targeted tests, and clippy for touched
crates. Update specs/SPRINT.md and Plan 74 status when done.
```

---

## W3 - Control-plane / data-plane inventory

**Goal**: make it explicit which operations are small control requests
and which require streaming or chunking.

### Action

- Add a docs section to
  `public/src/content/docs/reference/guest-agent.md`:
  "Control plane and data plane".
- Audit current `GuestRequest` / `GuestResponse` variants:
  - `Ping`, `Status`, `EntrypointStatus`, `IntegrationStatus` are
    control-plane.
  - `RunEntrypoint`, `ProcWait`, `FsRead`, `FsWrite`, `cp`, logs,
    artifacts, and builder output are data-plane or streaming.
- Add a table with:
  - max frame size;
  - chunk size;
  - terminal event shape;
  - backpressure behavior;
  - whether payload bytes are logged.
- Confirm every data-plane path has a bounded response or a streaming
  alternative.

### Exit tests / checks

- No code path documents an unbounded single-frame response.
- Docs explicitly state that payload bytes are not included in audit
  or readiness messages.

---

## W4 - Backpressure event model

**Goal**: turn "waiting" into typed state that the CLI and supervisor
can display and test.

### Proposed shape

```rust
pub enum BackpressureReason {
    GuestAgentUnavailable,
    ServiceHealthPending { pending: Vec<String> },
    OutputConsumerSlow,
    InputBufferFull,
    ArtifactTransferBlocked,
    BuilderBusy,
}
```

For the first implementation slice, wire this into one operation:
`ProcWait` or `RunEntrypoint`. `ProcWait` is preferable because it
already has stdout/stderr chunk events and process lifecycle semantics.

### Action

- Add a wire event:
  `ProcWaitEvent::Backpressure { reason, detail }` or an equivalent
  terminal-safe event.
- Keep details redacted and bounded. No argv, env values, stdin,
  stdout, or stderr content.
- Teach `mvmctl proc wait` and any session wrapper to render concise
  wait messages.
- Emit metrics counters for each reason when the observability wiring
  is available.

### Exit tests

- Unit test for event roundtrip.
- Slow-consumer fake emits `OutputConsumerSlow`.
- Full stdin ring fake emits `InputBufferFull`.
- CLI human output names the reason without dumping payload bytes.

---

## W5 - First-use DX and workflow polish

**Goal**: replicate the part of Banger that matters most to adoption:
the user can get from "I have a thing to run" to "it is running under
the runtime" without learning the internals first.

### DX target

Banger's happy path is:

1. Install the host runtime (`mpv`).
2. Put the bundled integration where the runtime expects it.
3. Launch the runtime and press one shortcut.

The `mvm` analogue should be:

1. `mvmctl doctor` tells the user whether the host can run the chosen
   backend and what exact setup step is missing.
2. `mvmctl init` or SDK recording creates the minimal project files.
3. `mvmctl run` / `mvmctl up` shows progress and produces a result,
   receipt, or running VM without requiring the user to understand
   builder VM internals.

### Action

- Define a "three-command happy path" for each primary audience:
  - CLI user with an existing command;
  - Python SDK user;
  - TypeScript SDK user;
  - operator running a prebuilt bundle.
- Define success metrics:
  - fresh macOS user runs the first workload in three commands with no
    host Nix installed;
  - warm rebuild path explains whether time was spent in VM boot, Nix
    eval, dependency fetch, cache miss, or guest startup;
  - every first-run failure names exactly one next action.
- Update `mvmctl doctor` requirements so missing prerequisites are
  grouped by workflow, not by implementation layer. Example:
  "Python SDK run is blocked because no builder VM image is available"
  instead of "rootfs.ext4 missing".
- Add workflow-specific preflight checks:
  - `mvmctl doctor --workflow cli-run`;
  - `mvmctl doctor --workflow python-sdk`;
  - `mvmctl doctor --workflow typescript-sdk`;
  - `mvmctl doctor --workflow bundle-run`;
  - `mvmctl doctor --workflow dev-shell`.
  If `doctor` becomes too broad, introduce `mvmctl quickstart --check`
  as a thin alias over these workflow checks.
- Define builder mode policy:
  - default: `managed-builder-vm`;
  - explicit opt-in: `host-nix`;
  - future: `remote-builder`;
  - no silent host-Nix auto-detection that changes the trust boundary.
- If `host-nix` is selected, print and record a reproducibility /
  security note explaining that mvm is now relying on the host's Nix
  daemon, substituters, sandbox settings, and store state.
- `mvmctl doctor` may detect host Nix, but should report it as optional
  acceleration/debug tooling unless the user explicitly selected
  `host-nix`.
- Make generated scaffolds include a ready-to-run sample command and
  a cleanup command.
- Keep outputs short by default; link to verbose logs when needed.
- Ensure failure recovery is explicit:
  - rebuild guest image;
  - upgrade host binary;
  - run bootstrap;
  - rerun with a named backend;
  - inspect audit receipt.

### Exit tests

- CLI tests assert `doctor` / quickstart output names workflow-level
  blockers.
- Scaffold tests assert generated projects include the runnable sample
  command.
- Golden tests for first-run failure messages: missing builder image,
  protocol mismatch, backend unavailable, and service health timeout.
- Builder-mode tests assert host Nix is not selected implicitly.
- `host-nix` opt-in path records the builder mode and warning in the
  receipt.
- Docs show the three-command paths and do not start with architecture.

---

## W6 - Structured progress, receipts, and explainability

**Goal**: progress and failure information should be a stable internal
API, not just CLI strings. The CLI, JSON output, Studio, MCP tools,
logs, and receipts should all render the same event vocabulary.

### Progress events

Add a closed event taxonomy for long-running commands:

```rust
pub enum ProgressEvent {
    BuilderModeSelected { mode: BuilderMode },
    BuilderImageReady { source: BuilderImageSource },
    NixEvalStarted,
    NixBuildStarted,
    CacheStateObserved { state: CacheState },
    BackendLaunchAccepted,
    AgentConnecting { port: u32 },
    AgentReady { protocol_version: u32 },
    ServicesStarting { pending: Vec<String> },
    ServicesReady,
    Backpressure { reason: BackpressureReason },
    ReceiptWritten { path: String },
}
```

The exact Rust location is deferred to implementation, but the design
rule is fixed: human text is a renderer over structured events.

### Receipts

Every successful `run`, `up`, and `build` should be able to leave a
small receipt with:

- command id;
- plan id / signed-plan hash when present;
- image hash / bundle hash;
- backend;
- builder mode;
- protocol version;
- capability set;
- build duration split by phase;
- cache state summary;
- audit-chain pointer;
- output hashes, not raw stdout/stderr.

Receipts must not store argv values, env values, stdin, stdout, stderr,
secrets, tokens, or user data unless an existing command already has a
specific receipt contract that allows a hash of that data.

### Explain command

Add or reserve:

```bash
mvmctl explain <receipt-path-or-error-id>
```

It should map common failures to specific causes and next actions:

- protocol mismatch;
- missing required capability;
- missing builder image;
- backend unavailable;
- egress denied;
- service health timeout;
- cache miss / cold store;
- host-Nix selected and failed due to cross-system limits.

### Exit tests

- Progress event serde roundtrip.
- Human renderer golden tests for key events.
- JSON renderer golden tests for key events.
- Receipt redaction tests prove no stdout/stderr/stdin/env payload is
  stored.
- `mvmctl explain` fixture tests for the common failure classes.

---

## W7 - CLI progress and error polish

**Goal**: make common waits debuggable without asking users to tail
logs.

### Action

- Update `mvmctl up` progress lines using W2 readiness.
- Update `mvmctl invoke`, `exec`, `session run`, and `proc wait` to
  distinguish:
  - cannot reach guest agent;
  - protocol mismatch;
  - missing required capability;
  - service health still pending;
  - backpressure.
- JSON output gets stable machine-readable fields; human output stays
  short.

### Exit tests

- CLI fixtures assert human strings for protocol mismatch and
  capability mismatch.
- JSON fixtures assert stable `readiness` and `backpressure.reason`
  fields.
- Error messages include the next action when known:
  upgrade host, rebuild guest image, or rerun after service health.
- Progress output comes from W6 structured events, not independent
  string construction.

---

## W8 - Builder egress and cache explainability

**Goal**: builder networking and cache behavior should be visible
enough to support both security claims and DX.

### Action

- Document that the builder VM is the default egress exception, not
  runtime VMs.
- Define builder egress event fields:
  - destination host / SNI when available;
  - protocol;
  - bytes sent / received;
  - allowlist decision;
  - build phase;
  - receipt command id.
- Decide the default allowlist for common build paths:
  - `cache.nixos.org`;
  - configured Nix substituters;
  - GitHub source fetches when flakes require them;
  - language registries only when the app-deps pipeline explicitly
    enables them.
- Add cache diagnosis fields to receipts and progress:
  - cold builder image;
  - cold Nix store;
  - substituter unavailable;
  - dependency registry fetch;
  - local rebuild after source change;
  - guest boot/startup time.

### Exit tests

- Unit tests for cache-state classification.
- Builder egress event redaction tests.
- Receipt fixture includes cache diagnosis without recording URLs that
  contain credentials.

---

## W9 - Documentation and sprint bookkeeping

**Goal**: make the new behavior discoverable and keep the active specs
accurate.

### Action

- Update:
  - `public/src/content/docs/reference/guest-agent.md`;
  - `public/src/content/docs/reference/cli-commands.md` for changed
    `status` / JSON fields;
  - getting-started docs for the first-use DX paths;
  - SDK guide examples for Python and TypeScript happy paths;
  - install docs to say host Nix is optional and explicit, not an
    automatic default;
  - troubleshooting docs for `mvmctl explain`;
  - any affected guide that references "running" as equivalent to
    "ready".
- Reorder getting-started pages so "run your first thing" comes before
  architecture and threat-model detail.
- Update `specs/SPRINT.md` as workstreams land.
- Add release notes when the compatibility window for pre-hello agents
  starts and again when it ends.

### Exit tests / checks

- CLI reference matches Clap output for any changed flags or commands.
- Sprint spec has completed workstreams checked off.

## Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Protocol hello breaks old guests | Existing images become unusable | One-release `Ping` compatibility shim; hello required only for new capabilities |
| Readiness duplicates lifecycle state | Confusing status output | Keep coarse `InstanceStatus` separate from nested `InstanceReadiness` |
| Backpressure leaks sensitive context | Secret disclosure in logs/audit | Closed reason enums, bounded redacted detail strings, tests asserting payload absence |
| Streaming refactor grows too wide | Large PR becomes hard to merge | W4 wires one operation first, likely `ProcWait` |
| Capability negotiation becomes stringly typed | Drift between host/guest | Closed enum + serde tests + docs table |
| Host Nix silently changes behavior | Repro/security claims become host-config-dependent | Managed builder VM default; host Nix explicit opt-in only |
| Progress text diverges across CLI/Studio/MCP | Confusing UX and brittle tests | Structured progress events with renderers |
| Receipts leak payload data | Secret/user data exposure | Hashes and metadata only; redaction tests |
| Builder egress looks like runtime egress | Security model confusion | Separate builder egress events and docs |

## Suggested first PR

The first PR should be W0 only:

1. Add ADR-050.
2. Add this plan cross-reference to the sprint spec.
3. Do not change the wire yet.

That gives us a reviewed contract before implementation and avoids
mixing protocol design with broad CLI changes.
