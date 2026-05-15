# Plan 76 - Secure fast-boot and DX improvements

> **Status:** proposed
> **Owner:** TBD
> **Started:** -
> **Depends on:** ADR-002, ADR-007, ADR-013, ADR-046, plans 41, 43, 45, 57, 72
> **Implements:** tighter production vsock profile, earlier guest readiness, deferred guest init, builder-storage boot parallelism, portable image artifacts, libkrun tiering
> **Tracking:** cold-path UX improvements that preserve mvm's Firecracker / dm-verity / vsock-only security baseline

## Why

A fast local cold path is the product of several optimizations working together: daemonless hypervisor startup, a tiny guest init path, early control-plane readiness, deferred non-critical guest work, pre-packed artifacts, and portable distribution. mvm should adopt those product and systems lessons without weakening its security posture.

mvm's security claim is stronger and must remain first-class:

- No SSH inside microVMs.
- Vsock is the only default communication path.
- Sealed production images keep dm-verity, authenticated frames, least privilege, and restricted guest verbs.
- Firecracker remains the hardened production baseline.
- libkrun is used where it is best: local dev, builder VMs, macOS, and fast smoke paths.

The goal of this plan is to make mvm's cold path fast and ergonomic while making the production attack surface smaller than it is today.

## Current observations

The current code already has useful security scaffolding:

- `mvm_guest::vsock::GUEST_AGENT_PORT` is a single unprivileged control port, currently `5252`.
- Frame size is capped at 256 KiB in the guest vsock path.
- `GuestRequest` and `GuestResponse` use `serde(deny_unknown_fields)`.
- Authenticated frames use Ed25519 signatures, session IDs, and monotonically checked sequence numbers.
- `SecurityPolicy::default()` sets `require_auth = true`.
- Fuzz targets exist for request parsing and authenticated frame verification.
- Production/dev handler separation already exists in several places.

The main problem is not that vsock is unconstrained at the transport level. The problem is that the **logical production surface is too broad**. `GuestRequest` contains lifecycle verbs, entrypoint invocation, filesystem RPC, process RPC, console control, code eval, port forwarding, and volume mounting. Some are dev-gated in handlers, but the production contract is not obvious enough, and future edits can accidentally widen it.

## Whole-plan acceptance criteria

When this plan is done:

1. A sealed production guest exposes a small, explicit vsock profile.
2. Dev-only verbs return typed `UnsupportedInProfile` errors before handler logic runs.
3. `mvmctl up` can report control-plane readiness before entrypoint warmup, probes, integrations, or optional subsystems finish.
4. `RunEntrypoint` still refuses until entrypoint validation and readiness gates pass.
5. Builder VM boot emits phase timing and parallelizes safe setup work.
6. mvm can pack and verify a portable signed artifact containing kernel, rootfs, verity metadata, cmdline, manifest, SBOM/attestation hooks, and signatures.
7. libkrun is documented and enforced as a Tier 2 backend for local/dev/builder usage, not as the hardened production default.
8. CI has explicit tests that prove prod images reject SSH, TCP listeners, and dev-only vsock verbs.

---

## Design principle

Use this rule for every phase:

> Bind early, authorize narrowly, initialize lazily, measure every phase.

Early readiness is not permission. It is observability and latency reduction. The guest can be "control-plane ready" while still refusing workload execution until the relevant security and readiness checks finish.

## Compatibility stance

This plan intentionally does **not** preserve migration or backward compatibility. Implementations should prefer clean, secure breaking changes over compatibility layers.

Rules:

- No legacy vsock protocol support is required.
- No compatibility shim for old guest agents is required.
- No compatibility shim for old hosts is required.
- No old artifact format support is required.
- No old readiness behavior needs to remain available.
- No `--legacy-*` CLI flags should be added unless a later plan explicitly reverses this decision.
- Missing profile metadata should fail closed, not infer a permissive mode.
- Unknown protocol versions should fail closed.
- Unknown artifact format versions should fail closed.

This simplifies the implementation order: each phase may update the host and guest contract together, delete obsolete code, and update tests/docs to match the new single supported behavior.

---

## Phase 1 - Explicit vsock profiles

### Goal

Make the allowed production verb set explicit and centrally enforced.

### New types

Add a profile enum in the guest/protocol layer:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProfile {
    SealedProd,
    Dev,
    Builder,
}
```

Add a request classifier:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    ProdSafe,
    DevOnly,
    BuilderOnly,
}

impl GuestRequest {
    pub fn class(&self) -> RequestClass { ... }
    pub fn allowed_in(&self, profile: AgentProfile) -> bool { ... }
}
```

### Initial profile policy

`SealedProd` allows:

- `Ping`
- `WorkerStatus`
- `EntrypointStatus`
- `RunEntrypoint`
- `SleepPrep`
- `Wake`
- `PostRestore`
- `UpdateIdleTimeout`, if warm-process runtime is configured
- `MountVolume` / `UnmountVolume`, only if the security policy and mount policy allow volumes for this image

`Dev` additionally allows:

- `Exec`
- `ConsoleOpen`
- `ConsoleClose`
- `ConsoleResize`
- `ProcStart`
- `ProcList`
- `ProcSignal`
- `ProcSendInput`
- `ProcWait`
- `ProcKill`
- `RunCode`
- `FsRead`
- `FsWrite`
- `FsList`
- `FsStat`
- `FsMkdir`
- `FsRemove`
- `FsMove`
- `StartPortForward`

`Builder` should not reuse the full tenant protocol. Prefer a separate builder protocol. If a temporary bridge is needed, allow only the verbs required by the builder path and document each one.

### Dispatcher changes

Before matching on the request in `mvm-guest-agent`, run:

```rust
if !request.allowed_in(active_profile) {
    return GuestResponse::UnsupportedInProfile {
        profile: active_profile,
        verb: request.verb_name(),
    };
}
```

Add a closed response/error shape rather than returning free-text `GuestResponse::Error`.

### Where profile comes from

Source of truth should be immutable image configuration:

- sealed production image: profile defaults to `SealedProd`
- dev image: profile defaults to `Dev`
- builder image: profile defaults to `Builder`

CLI/session mode may further restrict, but must not widen a sealed image at runtime.

### Tests

- Unit tests for every `GuestRequest` variant mapping to `RequestClass`.
- Snapshot-style test proving the `SealedProd` allowlist contains only the expected verbs.
- Dispatcher tests:
  - prod rejects `Exec`
  - prod rejects `ConsoleOpen`
  - prod rejects `ProcStart`
  - prod rejects `RunCode`
  - prod rejects `FsWrite`
  - prod rejects `StartPortForward`
  - dev allows the same verbs when the image profile is dev
- Security regression: a sealed image cannot opt into `Dev` via runtime config.
- Fuzz corpus updated for the new `UnsupportedInProfile` response.

### Acceptance

`cargo test -p mvm-guest vsock_profile` passes and a prod build of `mvm-guest-agent` has no reachable handler path for dev-only verbs after profile rejection.

---

## Phase 2 - Early guest control-plane readiness

### Goal

Bind the vsock control port as early as safely possible and expose structured readiness instead of blocking host UX on all guest initialization.

### New readiness model

Add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessReport {
    pub control_plane: ComponentState,
    pub entrypoint: ComponentState,
    pub warm_pool: ComponentState,
    pub integrations: ComponentState,
    pub probes: ComponentState,
    pub volumes: ComponentState,
    pub profile: AgentProfile,
    pub boot_millis: BootTimingReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Disabled,
    Starting,
    Ready,
    Failed { message: String },
}
```

Add `GuestRequest::ReadinessStatus` and `GuestResponse::ReadinessReport`.

### Boot sequence change

Current high-level shape:

1. parse config
2. validate entrypoint
3. initialize warm pool
4. install signals
5. create/bind/listen vsock
6. start monitoring/integration/probe threads
7. accept requests

Target shape:

1. parse minimal config and profile
2. install signal handlers
3. initialize shared readiness state
4. create/bind/listen vsock
5. mark `control_plane = Ready`
6. start accept loop
7. initialize entrypoint validation in background
8. initialize warm pool in background
9. load integrations/probes in background
10. run lifecycle hooks in background

### Security invariants

- `RunEntrypoint` must reject while `entrypoint != Ready`.
- Warm-process dispatch must reject while `warm_pool == Starting`.
- Dev-only verbs remain profile-gated before readiness checks.
- A failed optional subsystem must be visible in readiness, not silently ignored.

### Host UX

Add host-side wait modes:

- `mvmctl wait <vm> --for control-plane`
- `mvmctl wait <vm> --for entrypoint`
- `mvmctl wait <vm> --for warm-pool`
- `mvmctl wait <vm> --for all`

Update `mvmctl up` to show:

```text
control plane ready in 94 ms
entrypoint ready in 138 ms
warm pool warming...
```

### Tests

- Unit test readiness state transitions.
- Agent dispatcher test: `ReadinessStatus` works before entrypoint validation completes.
- Agent dispatcher test: `RunEntrypoint` rejects with typed `NotReady` until entrypoint is ready.
- Integration test with a fake slow `after_start.sh` proving control plane is ready first.

### Acceptance

`mvmctl up` can return or stream useful status as soon as vsock is bound, while `mvmctl invoke` remains blocked until entrypoint readiness passes.

---

## Phase 3 - Defer non-critical guest initialization

### Goal

Move slow, optional, or non-security-critical initialization out of the synchronous boot path.

### Defer after vsock bind

Defer:

- integration drop-in scanning
- integration health loop startup
- probe drop-in scanning
- probe loop startup
- warm-process pool creation
- lifecycle `after_start` hook
- optional filesystem diff baseline
- optional port-forward preparation

Keep before vsock bind:

- minimal config parse
- profile selection
- signal handler install
- socket bind/listen
- any immutable security policy load needed to reject verbs

Keep before `RunEntrypoint`:

- entrypoint file validation
- wrapper mode/owner/path checks
- runtime config validation for warm-process mode
- readiness hook success, when configured as mandatory

### Work breakdown

1. Introduce `AgentBootState` with atomics/mutexes for each component.
2. Split `init_entrypoint_validation()` into a background-capable function that reports state.
3. Split `init_warm_pool()` into parse/validate/start phases.
4. Make integrations/probes lazy background tasks with startup result reporting.
5. Teach `handle_client` to consult readiness state for affected verbs.

### Tests

- Slow probe directory fixture does not delay `Ping`.
- Malformed `runtime.json` marks warm pool failed and makes `RunEntrypoint` return a typed readiness/config error.
- Malformed integration drop-in does not kill control-plane readiness.

### Acceptance

On a fixture with slow probes and warm pool startup, `Ping` and `ReadinessStatus` respond before optional subsystems finish.

---

## Phase 4 - Boot phase telemetry

### Goal

Make cold-path performance visible and enforceable.

### Add timings

Track monotonic elapsed milliseconds for:

- host launch requested
- VMM process started
- kernel/init entered, when guest can report it
- agent process started
- vsock socket bound
- first host connection accepted
- entrypoint validation started/finished
- warm pool started/ready
- integrations loaded
- probes loaded
- first `RunEntrypoint` accepted
- first `RunEntrypoint` completed

### Data model

Add `BootTimingReport` to readiness:

```rust
pub struct BootTimingReport {
    pub agent_started_ms: Option<u64>,
    pub vsock_bound_ms: Option<u64>,
    pub first_accept_ms: Option<u64>,
    pub entrypoint_ready_ms: Option<u64>,
    pub warm_pool_ready_ms: Option<u64>,
    pub integrations_ready_ms: Option<u64>,
    pub probes_ready_ms: Option<u64>,
}
```

Host-side VMM timings live in mvm runtime state and are joined with guest timings for display.

### CLI

Add:

- `mvmctl boot-report <vm>`
- `mvmctl up --timings`

Output should be compact and copyable for issue reports.

### Tests

- Unit tests for monotonic timing report construction.
- CLI snapshot test for `boot-report`.
- Live smoke test gated by existing live env vars, with budgets initially informational.

### Acceptance

Every boot can produce a timing report that identifies whether time was spent in VMM launch, guest init, entrypoint validation, warm pool, probes, integrations, or first invocation.

---

## Phase 5 - Builder VM storage parallelism

### Goal

Apply the same cold-path discipline to `mvm-builder-init`.

### Current serial path

`mvm-builder-init` currently does:

1. mount `/proc`, `/sys`, `/dev`, `/tmp`
2. probe/format/mount `/dev/vdb`
3. seed `/nix-store`
4. bind `/nix-store` over `/nix`
5. modprobe `fuse` and `virtiofs`
6. mount virtio-fs `work`, `out`, `job`
7. setup network
8. run job
9. write result
10. power off

### Target safe parallelism

Keep serialized:

- `/dev/vdb` format/mount
- first-boot seed of `/nix-store`
- bind mount over `/nix`
- job execution

Parallelize:

- start network setup after pseudofs mounts
- start module loading as soon as `/sys` and `/dev` exist
- mount `job` and `out` virtio-fs as soon as modules are loaded
- mount `work` before job execution, but do not block failure reporting on `work` if `job`/`out` are available

### Add builder boot timings

Write `/job/boot-timings.json` and mirror to stderr:

- init start
- pseudofs mounted
- network start/end
- module load start/end
- nix device ready
- nix mounted
- seed start/end
- virtiofs mount start/end per tag
- job start/end
- poweroff start

### Failure behavior

- If `job` is mounted, write failure result there.
- If `out` is mounted, write structured result there too.
- If only stderr is available, emit enough timing and failure context for the host supervisor to surface.

### Tests

- Unit tests for timing JSON writer.
- Linux-only test for helper scheduling with mocked command runner.
- Existing `mvm-builder-init` tests remain green.
- Live libkrun builder smoke records `boot-timings.json`.

### Acceptance

Builder VM logs show overlapped network/module/share setup, and the first failed setup phase is visible without reading the full console log.

---

## Phase 6 - Portable signed artifacts

### Goal

Add an mvm-native packed artifact that is portable across hosts while preserving mvm's sealed-image security model.

### Artifact layout

Use a deterministic tar or zstd-compressed tar. Suggested extension: `.mvm`.

```text
artifact.mvm
  manifest.json
  kernel/vmlinux
  rootfs/rootfs.ext4
  rootfs/rootfs.verity
  rootfs/roothash
  initrd/verity-initrd.cpio.gz
  cmdline.txt
  sbom/sbom.json
  attestations/
  signatures/manifest.ed25519
```

### Manifest requirements

`manifest.json` includes:

- artifact format version
- mvm version
- target architecture
- backend compatibility
- rootfs hash
- kernel hash
- initrd hash
- dm-verity sidecar hash
- roothash
- cmdline hash
- SBOM hash, if present
- build provenance pointer
- security posture: sealed/dev, profile, requires auth, allows volumes, allows egress

The manifest signature covers every referenced byte by hash. Verification happens before launch.

### CLI

Add:

- `mvmctl artifact pack --manifest <mvm.toml> --out app.mvm`
- `mvmctl artifact verify app.mvm`
- `mvmctl up --artifact app.mvm`
- later: `mvmctl artifact push/pull`

### Security requirements

- Refuse unsigned artifacts by default.
- Refuse artifacts whose manifest declares `SealedProd` but omits verity metadata.
- Refuse unknown artifact format versions unless explicitly allowed.
- Verify before extraction or launch.
- Extract only into a private temp/cache dir with restrictive permissions.
- Never allow artifact contents to write outside the extraction dir.

### Tests

- Pack/verify roundtrip.
- Tampered rootfs rejected.
- Tampered manifest rejected.
- Missing signature rejected.
- Missing verity metadata rejected for sealed prod.
- Path traversal entries in tar rejected.
- Unknown format version rejected.
- CLI help/reference docs updated.

### Acceptance

A sealed image can be packed, copied, verified, and launched from one file without losing dm-verity or signature guarantees.

---

## Phase 7 - libkrun backend tiering

### Goal

Make libkrun a first-class fast local/backend path without overstating its isolation properties.

### Backend tiers

Document and enforce:

- **Tier 1:** Firecracker. Hardened production default on Linux/KVM.
- **Tier 2:** libkrun. Fast local/dev/builder backend for macOS and non-KVM dev hosts.
- **Tier 3:** fallback/non-production backends, with warnings.

### Policy

`mvmctl up --prod` should not silently select libkrun unless an explicit override says the operator accepts Tier 2 isolation.

Suggested explicit flag:

```text
mvmctl up --prod --hypervisor libkrun --accept-tier2-isolation
```

Dev commands can auto-select libkrun where it is the best user experience.

### Tests

- Backend auto-select tests:
  - prod on Linux/KVM chooses Firecracker
  - dev on macOS may choose libkrun
  - prod with libkrun but no acknowledgement refuses
  - prod with acknowledgement logs/audits the decision
- `doctor` reports backend tier and reason.

### Acceptance

No production path silently downgrades from Tier 1 isolation to Tier 2 isolation.

---

## Phase 8 - Security regression gate

### Goal

Make the no-SSH/vsock-only claim mechanically checked.

### Checks

Add CI/runbook checks for sealed prod images:

- no `sshd` binary
- no SSH host keys
- no port 22 listener
- no TCP listeners unless explicitly configured
- guest agent binds only vsock control port by default
- dev-only vsock verbs rejected
- `SecurityPolicy.require_auth = true`
- rootfs verity metadata present
- agent runs without ambient capabilities where expected

### Tests

- Static image inspection test.
- Guest agent profile test.
- Live smoke, gated, that runs:
  - `mvmctl doctor vsock-profile <vm>`
  - `mvmctl invoke <vm>` succeeds
  - `mvmctl exec <vm>` fails in sealed prod
  - `mvmctl console <vm>` fails in sealed prod
  - port 22 is not open

### Acceptance

The security claim appears in CI as a named gate, not as an assumption in docs.

---

## Additional ideas not yet phased

These are not required for the first implementation pass, but they are worth keeping visible. Each should become a separate phase or follow-up plan only after Phases 1-4 make the security and readiness model explicit.

### Split wire schema by profile

Phase 1 keeps one `GuestRequest` enum and classifies variants. A stronger future shape is separate protocol enums:

- `ProdRequest`
- `DevRequest`
- `BuilderRequest`

The host would serialize only the profile-specific enum after handshake. This reduces accidental prod exposure at compile time, not just dispatch time. The tradeoff is migration churn across CLI helpers and fuzz targets.

### Capability negotiation after handshake

After authenticated session setup, add a `Capabilities` report:

- profile
- protocol version
- allowed verbs
- max frame size
- streaming support
- volume support
- warm-pool support
- artifact format support

The host can then fail with precise messages before sending a verb that will be rejected. This helps DX and also gives CI a simple way to assert the prod contract.

### Per-verb authz and audit events

Profiles are coarse. Add a second layer where each verb has:

- required profile
- required security-policy bit
- audit event type
- whether request/response payloads may be logged
- redaction policy

This avoids future handlers inventing ad hoc authorization rules. It also protects against accidentally logging stdin, env, file contents, or command output.

### Separate control socket from data sockets

Keep the single control port small and stable. Move high-volume streams to explicitly negotiated secondary vsock ports with short-lived tokens:

- console data
- proc wait streams
- large fs read/write streams
- future artifact transfer streams

The control channel remains bounded and easy to fuzz. Data channels can have separate byte budgets, timeouts, and token expiry.

### Stronger key identity model

Current authenticated frames prove possession of session keys. A future hardening pass should bind session keys to image identity and launch identity:

- manifest signing key
- expected guest public key or measured key derivation
- VM instance ID
- artifact digest
- roothash

This prevents a host-side bug from connecting to the wrong guest and accepting a valid but unintended session.

### Readiness as a state machine

The plan currently sketches readiness fields. Make it a real state machine:

- valid transitions only
- monotonic component states unless reset by `Wake` or `PostRestore`
- typed failure reasons
- no ambiguous "ready but failed optional hook" states

This makes CLI output clearer and prevents future code from setting readiness flags inconsistently.

### Lazy subsystem loading by first use

Phase 3 defers non-critical initialization in the background. A later optimization can avoid initializing optional subsystems until first use:

- load probe definitions only on `ProbeStatus`
- initialize filesystem diff only on `FsDiff`
- start console support only on `ConsoleOpen`
- initialize port-forwarding only on `StartPortForward`

This is useful for minimal production images where most features are never used.

### Snapshot and restore readiness

If snapshots are used for fast boot, readiness needs snapshot-aware semantics:

- readiness state must not be restored as `Ready` if external resources changed
- session keys must not survive restore unless explicitly designed for it
- sequence numbers must not roll back
- `PostRestore` should force revalidation of entrypoint, volumes, network, time, and secrets

This should be a dedicated hardening phase before using snapshots as a production fast-start feature.

### Precomputed image index

For portable artifacts and faster startup, generate an image index at build time:

- entrypoint metadata
- allowed profile
- expected files and modes
- volume declarations
- probe/integration declarations
- security posture summary

The guest can read one small file instead of scanning multiple directories on boot. The host can verify the same file before launch.

### Host-side warm artifact cache

For `.mvm` artifacts, keep a content-addressed verified cache:

- artifact digest directory
- extracted kernel/rootfs/verity sidecars
- verification stamp bound to verifier version
- restrictive permissions
- cache GC

This makes `mvmctl up --artifact` fast after the first verified extraction.

### OCI distribution as a compatibility layer

Do not make OCI the internal artifact format first. Instead, make `.mvm` the signed internal unit and optionally wrap it in OCI for registry distribution. This preserves one verification path and avoids splitting security semantics between tar and OCI layouts.

### Dev UX commands

Small commands that make the model obvious:

- `mvmctl doctor vsock-profile <vm>`
- `mvmctl capabilities <vm>`
- `mvmctl readiness <vm> --watch`
- `mvmctl boot-report <vm> --json`
- `mvmctl artifact inspect app.mvm`
- `mvmctl artifact diff a.mvm b.mvm`

### Kernel and init tuning

After telemetry lands, consider kernel/init changes only where measurements justify them:

- built-in virtiofs/fuse modules instead of `modprobe`
- smaller kernel config for dev images
- deterministic cmdline presets per backend
- initramfs only where it reduces net time or strengthens verified boot

Do not tune blindly. Require boot timing evidence first.

---

## Security concerns to track explicitly

These concerns are documented here so implementation sessions do not optimize through them accidentally.

### Early readiness can become an auth bypass

Binding vsock earlier is safe only if authorization and readiness gates run before every sensitive verb. `Ping` and `ReadinessStatus` may work early; `RunEntrypoint`, dev verbs, volume operations, and data streams must still check profile, policy, and component readiness.

### Dispatcher allowlists are not enough by themselves

A profile check in the dispatcher is necessary but not sufficient. Handlers must still enforce local invariants because future code may call helpers directly from tests, alternate dispatchers, or builder paths.

### Dev-only code compiled into prod binaries

Even if prod rejects dev verbs at runtime, compiled handler symbols increase audit burden. Where practical, keep dangerous handlers behind feature gates or profile-specific modules:

- shell exec
- process spawn
- code eval
- console shell
- filesystem write/remove
- port forwarding

CI should continue checking that prod builds do not accidentally include direct exec paths.

### Read-only filesystem access can leak secrets

`FsRead`, `FsList`, and `FsStat` look safer than writes, but they can expose secrets, environment files, tokens, application data, or mounted volume contents. Default sealed prod should treat all filesystem RPC as dev-only unless a specific product requirement justifies a constrained read-only subset.

### Volume mounts can shadow trusted paths

Runtime mounts can bypass assumptions made at image-build time. Never allow mounts over:

- `/bin`
- `/sbin`
- `/usr`
- `/lib`
- `/lib64`
- `/etc`
- `/proc`
- `/sys`
- `/dev`
- `/nix`
- `/nix/store`
- the entrypoint path
- the agent binary path

Prefer boot-declared production volumes over runtime mount/unmount for sealed prod.

### Port forwarding widens the network boundary

`StartPortForward` converts vsock-only communication into TCP reachability. It should remain dev-only by default. If production forwarding is ever required, it needs a separate policy with explicit host bind address, port allowlist, audit events, and no wildcard binds.

### Console is equivalent to shell access

Console support must be treated like SSH from a security perspective even though it uses vsock. It is acceptable for dev images and forced recovery workflows, but sealed prod must reject it by default.

### RunCode is remote code execution by design

`RunCode` must remain dev-only. Do not attempt to make it production-safe by adding filters. The production-safe primitive is `RunEntrypoint` against a baked and validated wrapper.

### Process RPC can bypass entrypoint policy

`ProcStart` and related verbs can run programs other than the baked entrypoint. They must remain dev-only unless a future plan creates a strict allowlisted production process model with no shell, no arbitrary env, no writable cwd escape, and clear audit semantics.

### Authenticated frames need freshness and identity

Sequence checks prevent simple replay within a session, but future snapshot/restore and reconnect flows can reintroduce replay risk if counters roll back. Session identity should eventually bind to VM instance ID, artifact digest, and guest key identity.

### Timestamps are not security until clocks are trustworthy

Frame timestamps are useful for audit ordering, but should not be used as the primary freshness control unless guest/host clock trust is explicitly designed. Prefer nonces, challenges, session IDs, and sequence numbers.

### Denial of service via early bind

An early-bound agent can receive requests while still initializing. Add bounds:

- connection backlog
- per-peer request timeout
- max concurrent handlers
- max queued readiness waiters
- per-verb byte budgets
- fail-fast behavior while warming

### Background init races

Deferring init introduces races between readiness state and handler state. Use a single shared state object and avoid duplicated booleans. Tests should force slow init and concurrent requests.

### Logging and audit redaction

Boot reports, readiness errors, artifact manifests, and vsock error messages must not leak:

- stdin payloads
- env vars
- tokens
- file contents
- full command lines when they may contain secrets
- host filesystem paths beyond what is already user-visible

Every new audit event should define redaction rules.

### Artifact extraction is an attack surface

Portable artifacts must be treated as untrusted input until fully verified. Risks:

- path traversal entries
- symlinks escaping extraction root
- hardlinks
- special files
- huge sparse files
- decompression bombs
- duplicate paths
- case-collision issues on case-insensitive filesystems
- mismatched manifest paths vs archive paths

Verification must happen before launch, and extraction must happen into a private restrictive directory.

### Artifact signatures need key policy

Signing a manifest is not enough without a trust policy:

- which keys are trusted
- how keys rotate
- how revoked keys are handled
- whether dev artifacts can be unsigned
- how CI/release keys differ from local developer keys
- how verification results are cached

Do not let `--insecure-skip-verify` become a normal path.

### libkrun isolation is not Firecracker isolation

libkrun is valuable for speed and macOS UX, but its host/guest boundary should not be described as equivalent to Firecracker with jailer/seccomp. Production selection must require explicit operator acknowledgement when using Tier 2 isolation.

### virtio-fs trust boundary

virtio-fs shares host filesystem semantics into the guest. Risks include metadata leakage, symlink behavior, cache incoherence, permission surprises, and host path exposure. Production virtio-fs mounts need strict path policy, read-only defaults where possible, and audit events.

### Builder VM supply chain

The builder VM is security-critical because it creates artifacts. It needs:

- verified builder image download
- signed builder image manifest
- pinned hashes for kernel/rootfs/cmdline
- restricted workspace mount
- no secret leakage into artifacts
- egress policy for dependency fetches
- audit logs for install/build inputs

Fast builder UX must not bypass supply-chain gates.

### Warm caches can preserve unsafe state

Persistent Nix stores, artifact caches, and warm worker pools improve speed but can retain compromised or stale state. Add cache identity, validation, GC, and invalidation rules. Never let a warm cache override a manifest hash or policy change.

---

## Suggested implementation order

1. Phase 1: vsock profiles.
2. Phase 8 partial: profile/security regression tests for existing behavior.
3. Phase 2: readiness report.
4. Phase 3: deferred init.
5. Phase 4: boot telemetry.
6. Phase 5: builder init parallelism.
7. Phase 6: portable artifacts.
8. Phase 7: libkrun tier enforcement and docs.
9. Phase 8 complete: live sealed-prod gate.

Reasoning: shrink and test the security surface before making boot faster. Then make readiness visible. Then optimize. Then package/distribute.

## Documentation updates

Each implementation PR must update relevant docs:

- `public/src/content/docs/reference/guest-agent.md`
- `public/src/content/docs/reference/cli-commands.md`
- `public/src/content/docs/reference/architecture.md`
- `public/src/content/docs/guides/building-microvm-images.md`
- `public/src/content/docs/guides/builder-vm.md`
- `public/src/content/docs/guides/manifests.md`
- `public/src/content/docs/guides/troubleshooting.md`

Docs must clearly distinguish:

- control-plane readiness vs workload readiness
- sealed production vs dev profile
- Firecracker Tier 1 vs libkrun Tier 2
- artifact verification vs artifact extraction

## AI session handoff prompt

Use this prompt to start an implementation session:

```text
We are implementing specs/plans/76-secure-fast-boot-and-dx.md.

Start with Phase <N>. Follow AGENTS.md strictly:
- create a feature worktree under ../.worktrees/
- run git only from the main checkout with git -C <worktree>
- run cargo on the macOS host unless the test genuinely needs Linux
- do not use limactl
- keep security first: no SSH, vsock-only by default, sealed prod profile must reject dev-only verbs
- do not preserve backward compatibility unless this plan is explicitly amended; fail closed on old/unknown protocol and artifact formats

Before editing, inspect:
- crates/mvm-guest/src/vsock.rs
- crates/mvm-guest/src/bin/mvm-guest-agent.rs
- crates/mvm-core/src/policy/security.rs
- specs/SPRINT.md
- relevant docs under public/src/content/docs/

Implement only Phase <N>, including tests and docs. Do not widen production behavior. At the end, run:
- cargo test --workspace
- cargo check --workspace
- cargo clippy --workspace -- -D warnings

If Linux-only or live-KVM validation is required, add a gated test/runbook and explain exactly what remains for the builder VM or KVM host.
```

## Open questions

1. Should read-only filesystem RPC be allowed in sealed prod, or should all filesystem RPC be dev-only by default?
2. Should volume mount/unmount be prod-safe in v1, or should production volumes be boot-declared only?
3. Should artifact packing use tar+zstd, OCI layout, or both?
4. Should `ReadinessStatus` be a new request or folded into `WorkerStatus`?
5. Should the builder VM use a separate vsock protocol from day one, or only after current libkrun builder stabilization?

## Non-goals

- Adding SSH or ssh-agent forwarding to guests.
- Replacing Firecracker as the production baseline.
- Removing dm-verity from sealed artifacts.
- Making dev-only process/filesystem/code-eval verbs production-safe.
- Building a custom VMM.
- Rewriting the guest agent in async Rust unless sync/threaded code proves insufficient.
