# Plan 77 — Stage 0 builder VM bootstrap via the cached dev image

**Status:** W0 + load-bearing W1 shipped on `main`. W0's `LibkrunBuilderVm` refactor + invariant landed via commit chain ending at 843ef18 (W0 PR #280 was superseded by this same chain). W1's `bootstrap_builder_vm_image_via_dev_image_stage0` shipped in 843ef18 with hardening in 4bbb615 (atomic staging), a8f47e9 (no-feature cache helpers), 0aac0f2 (source-fingerprint binding), and 68ae7db (artifact-digest manifest). The W1 seed-image gap — `find_local_fallback_image` missing `~/.mvm/dev/current/` — was closed by PR #293. W2 (advisory lock), W3 (audit emit beyond what's already in the cache-promotion path), and W4 (gate the download path behind a feature flag) have shipped. W5 (preflight seed-contract check) + W6 (host-side kernel-panic detector) added 2026-05-15 after a reproducible kernel panic on a Stage 0 seed whose rootfs lacked `/sbin/mvm-builder-init` — see "Why W5 + W6 were added" below.

## Why W5 + W6 were added

On 2026-05-15 a contributor host hit `Kernel panic - not syncing: Requested init /sbin/mvm-builder-init failed (error -2)` at boot t=0.08s of the Stage 0 VM. Root cause: the seed dev image at `~/.mvm/dev/current/rootfs.ext4` was built before commit 843ef18 (2026-05-14), which is when the dev-image flake started shipping `mvm-builder-init` at `/sbin/`. Plan 77 W1's Stage 0 contract assumed every seed has that PID-1 binary; a stale seed silently violates the contract.

Two failure modes compounded:

1. **No preflight contract check.** `bootstrap_builder_vm_image_via_dev_image_stage0` launched libkrun against the contract-stale seed and only Stage0FailureStage::Validate could have caught it — but Validate runs *after* the build completes, and the build never did because the kernel panicked at PID 1.
2. **No host-side panic detector.** The libkrun supervisor blocks in `krun_start_enter` until the VM cleanly exits. A kernel panic doesn't trigger a clean exit. The supervisor — and therefore `mvmctl dev up` — hangs indefinitely. Even Ctrl-C / SIGTERM doesn't reliably propagate (existing memory entry `reference_libkrun_gotchas.md`).

W5 closes mode (1) by making the contract explicit and validated before any VM boot. W6 closes mode (2) so any *future* contract drift (or unrelated kernel bring-up failure) surfaces as a clear, prompt error instead of a 10-minute hang plus an orphaned `mvm-libkrun-supervisor` process holding 4 GiB and the stage0 advisory lock.

## Goal

Make `mvmctl dev up` work on a source-checkout contributor host when the builder VM image cache is empty, **without** downloading a prebuilt artifact and **without** depending on host Nix.

## Why this exists

PR #230 ("Drop microsandbox backend") removed the only local Stage 0 path that built `~/.cache/mvm/builder-vm/<arch>/{vmlinux,rootfs.ext4}` from the in-repo `nix/images/builder-vm/flake.nix`. Its successor (Plan 75: mvm-oci + libkrun Stage 0) shipped only W0 (claims hygiene + ADRs). With Stage 0 unwired, source-checkout dev hosts whose builder VM cache is missing have **no path forward** — the only fallback is a GitHub release download, which contradicts two hard invariants now recorded in AGENTS.md and CLAUDE.md:

1. **No prebuilt builder VM artifact, ever — until we cut a release.**
2. **Source-checkout builds never depend on mvm-published artifacts** (existing ADR-046 rule).

Plan 75's mvm-oci approach is the eventual right answer but it requires building OCI fetch + ext4 materialization (host-side mkfs on macOS is impossible per ADR-050) + a published nix-bearing OCI image. That's multi-PR work.

Plan 77 is the pragmatic in-between: **boot the contributor's already-cached dev image as the Stage 0 Linux env**, have it run `nix build` of the builder VM flake, copy the result back. The dev image is exactly the seed we need — full Linux + Nix store + mvm-guest-agent with `do_exec` already wired up.

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│ Host (macOS Apple Silicon / Linux KVM)                         │
│                                                                │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ mvmctl dev up                                            │  │
│  │   └─ ensure_dev_image()                                  │  │
│  │       └─ build_image_via_libkrun()                       │  │
│  │           └─ bootstrap_builder_vm_image()                │  │
│  │               ├─ cache hit?  yes → return                │  │
│  │               └─ no → bootstrap_via_dev_image_stage0     │  │
│  │                       │                                   │  │
│  │                       │  ┌──────────────────────────┐    │  │
│  │                       │  │ dev image VM (libkrun)   │    │  │
│  │                       │  │                          │    │  │
│  │                       ├─→│ /work  (workspace, ro)   │    │  │
│  │                       ├─→│ /out   (staging, rw)     │    │  │
│  │                       │  │                          │    │  │
│  │                       │  │ guest-agent exec:        │    │  │
│  │                       │  │   nix build              │    │  │
│  │                       │  │     /work/nix/images/    │    │  │
│  │                       │  │       builder-vm# ...    │    │  │
│  │                       │  │   cp result/* /out/      │    │  │
│  │                       │  └──────────────────────────┘    │  │
│  │                       │                                   │  │
│  │                       └─ promote /out → cache (atomic)   │  │
│  └──────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────┘
```

## Components

### W0 (this PR) — landed below this plan
- `BuilderVmImage::new()` public constructor in `crates/mvm-build/src/libkrun_builder.rs`
- `LibkrunBuilderVm::with_image_override(image)` builder method
- `LibkrunBuilderVm::image_override` field — bypasses `ensure_builder_vm_image()` cache lookup in `run_build` when set
- AGENTS.md + CLAUDE.md invariant: no prebuilt builder VM artifact downloaded; not published until release
- Memory entry: `feedback_no_prebuilt_builder_vm_artifact.md`

### W1 — Stage 0 implementation
- New function `bootstrap_via_dev_image_stage0(workspace_root, staging_out) -> Result<()>` in `crates/mvm-cli/src/commands/env/apple_container.rs`
- Boots dev image (kernel + rootfs from `~/.mvm/dev/current/`) via `LibkrunBackend::start`
- Configures `VmStartConfig.volumes` with `/work` (workspace, read-only) and `/out` (staging, read-write)
- Waits for guest agent via `crate::exec::wait_for_agent`
- Sends nix build via `crate::exec::dispatch_in_session` (uses `SessionVm { vm_name: stage0_vm_name }`)
- On success: validates `/out` contains `vmlinux` + `rootfs.ext4` (non-zero size, ext4 magic check on rootfs)
- Atomically renames staging dir to `~/.cache/mvm/builder-vm/<arch>/`
- Tears down VM via `tear_down_session_vm`
- `bootstrap_builder_vm_image` calls `bootstrap_via_dev_image_stage0` on source-checkout cache miss (when `~/.mvm/dev/current/{vmlinux,rootfs.ext4}` both exist)
- Failure mode when dev image is missing: clean error with remediation hint

### W2 — concurrency + atomic promotion
- Advisory lock at `~/.cache/mvm/builder-vm/stage0.lock` (fcntl on Unix); serialize concurrent Stage 0 invocations
- Stage 0 writes to `~/.cache/mvm/builder-vm/<arch>-staging-<unique>/`; on success, `rename(2)` to `~/.cache/mvm/builder-vm/<arch>/` (atomic at filesystem level)
- Cleanup of orphaned staging dirs in `cache prune`

### W3 — observability + tests
- Audit emit: `stage0.boot`, `stage0.exec.started`, `stage0.exec.completed`, `stage0.cache.promoted`, `stage0.failed` to `~/.mvm/audit/stage0.jsonl`
- Unit tests on Stage 0 entrypoint via mock `LibkrunBackend`
- Integration test: clean cache + present dev image → cache populates; assert byte equivalence of resulting rootfs with a `nix build`-direct run when host Nix is available (smoke only, not CI gate)
- Failure path tests (dev image missing, build fails, write fails on rename)

### W4 — lint the download path closed
- Move `download_builder_vm_image` behind a `#[cfg(feature = "release-artifact-bootstrap")]` gate that is off by default and only on for end-user-binary release builds
- Source-checkout flow can never reach the download path; failed Stage 0 is a hard error

### W5 — preflight seed contract check
Move the contract drift detection from "implicit, panics inside the VM" to "explicit, validated on the host before any libkrun boot."

- `nix/images/builder/flake.nix` (the dev image flake — the source of every Stage 0 seed) emits `$out/manifest.json` next to `vmlinux` / `rootfs.ext4`. The sidecar carries:
  - `schema_version: 1` — flake-side contract for the manifest's own shape.
  - `contract_version: 2` — bumped each time the Stage 0 boot contract changes in a backward-incompatible way (e.g. init binary moves, kernel cmdline shape changes, expected mount points shift). `contract_version: 2` is the first published version, set to the value Plan 77 W5 requires.
  - `init_paths: ["/sbin/mvm-builder-init"]` — the binaries Stage 0 needs to find inside the rootfs. The host-side validator confirms the dev-image flake added them via `extraFiles`; it doesn't peek inside the ext4 (no ext4 walker on the host, no mount on macOS).
  - `image_kind: "dev"` — distinguishes the dev image manifest from the builder-vm manifest's shape (`name: "mvm-builder-vm"`); they're sister artifacts and a wrong-kind manifest must fail validation.
  - `system: "<flake system tuple>"` — passthrough from the flake's `system` for diagnostics.
- New `fn validate_stage0_seed_contract(seed_dir: &Path) -> Result<(), SeedContractError>` in `crates/mvm-cli/src/commands/env/apple_container.rs`.
  - Reads `<seed_dir>/manifest.json`.
  - Fails fast (no manifest, malformed JSON, wrong `image_kind`, `schema_version` too new for this binary, `contract_version` below the required minimum, missing required `init_paths`).
  - Each error variant carries a structured `reason` suitable for `Stage0Failed.reason` and an end-user remediation string (e.g. "Stage 0 seed at <path> is contract-stale — rebuild the dev image with `mvmctl dev rebuild`, or import a signed published image with `mvmctl dev import-image`").
- Wire into `bootstrap_builder_vm_image_via_dev_image_stage0`: call `validate_stage0_seed_contract(seed_dir)` immediately after `find_local_fallback_image` returns and before the staging directory is created. On failure, emit a `Stage0Failed` audit line with `stage=preflight reason=seed_contract_<variant>` and bail with the structured remediation message.

The check is a pure host-side file read — no VM boot, no nix evaluation, no network — so the new failure path costs milliseconds and surfaces a precise, actionable error before any expensive work runs.

### W7 — Stage 0 reads the source-checkout vendored seed slot (the two-track design)

W5 turns a hang into a clear error. W7 makes the error actionable: a fresh contributor on a source checkout has a *path forward* that doesn't depend on a teammate's `~/.mvm/dev/current/` or a release-pipeline round-trip.

#### The two-track design

Before W7 the source-checkout bootstrap had a chicken-and-egg gap: Stage 0 reads `find_local_fallback_image`, which only searches `~/.mvm/dev/{current,prebuilt,builds}/`. On a fresh contributor host all four are empty. The `nix/images/dev-prebuilt/<arch>/` vendored slot (which `cargo xtask build-dev-image` populates) was checked only by the *installed-binary* branch of `ensure_dev_image`, never by Stage 0 itself. So even with a freshly-`xtask`-built seed sitting in the source tree, source-checkout `mvmctl dev up` ignored it and bailed.

W7 codifies the two-track design that CLAUDE.md's "source-checkout builds never depend on mvm-published artifacts" invariant already implied:

- **Dev track** (contributor source checkout, every `mvmctl dev up`): build the builder VM and dev image from the in-tree flakes via libkrun. Stage 0 seeds from the vendored slot (or a previously-cached `~/.mvm/dev/current/`) — no published-artifact dependency. Edits to either flake show up on the next run.
- **Release track** (installed-binary users): the release pipeline runs `cargo xtask build-dev-image` on a Linux Nix runner, signs the result, and publishes `dev-rootfs-*.ext4` + `builder-vm-*` to GitHub releases. Installed binaries download + cosign-verify them (existing ADR-005 / Plan 36 path).

The one host-Nix step in the dev track is the contributor's **one-time bootstrap**: `cargo xtask build-dev-image --arch <arch>` populates the vendored slot from the contributor's in-tree flake. After that, mvmctl never touches host Nix again — Stage 0 uses the vendored slot as the seed, every subsequent `mvmctl dev up` rebuilds via libkrun, and every flake change shows up immediately.

#### Code changes

- **`find_local_fallback_image_with_workspace_root`** (new testable inner form in `crates/mvm-cli/src/commands/env/apple_container.rs`): replaces the open-coded enumeration in `find_local_fallback_image_with` with a body that takes an explicit `workspace_root` for the source-checkout vendored slot, so unit tests can substitute a tempdir without mocking `env!("CARGO_MANIFEST_DIR")`. The four locations enumerated are `~/.mvm/dev/current/`, `~/.mvm/dev/prebuilt/v*/`, `~/.mvm/dev/builds/*/`, and **`<workspace_root>/nix/images/dev-prebuilt/<arch>/`** (the W7 addition). Each candidate passes the existing `validate_dev_image_artifacts` size + ext4-magic sanity check; the closure-filter form continues to flow through to `find_local_stage0_bootstrap_image` for the `rootfs_contains_builder_init` byte-scan.
- **`find_local_fallback_image_with`** (production wrapper): unchanged signature, now resolves the workspace_root via the new `workspace_root_for_vendored` helper and delegates to the `_with_workspace_root` form. Both `find_local_fallback_image` (line-891 installed-binary fallback caller) and `find_local_stage0_bootstrap_image` (the Stage 0 caller, filtering on `rootfs_contains_builder_init`) pick up the vendored slot for free.
- **`workspace_root_for_vendored` + `stage0_seed_arch` + `vendored_dev_image_dir_for`** (new internal helpers): central path resolution for the vendored slot. `find_vendored_dev_image` (the line-858 caller that pre-dates W7) re-uses these instead of its previous open-coded layout, so the two callers can never drift.
- **`bootstrap_builder_vm_image_via_dev_image_stage0`** (updated error path): when `find_local_stage0_bootstrap_image` returns `None`, the caller now emits a Thread-B "first-time setup" message that names the exact `cargo xtask build-dev-image --arch <arch>` to run, lists every checked location including the vendored slot, and points at the Plan 77 W7 two-track design. The audit emit distinguishes `Stage0Failed stage=preflight reason=seed_missing` (no candidate at all) from `reason=seed_missing_builder_init` (candidates exist but none contain `/sbin/mvm-builder-init`).
- **`stage0_seed_doctor_status` + `Stage0SeedDoctorStatus`** (new, feature-gated, `pub(crate)`): `mvmctl doctor` consumes this to render the seed-landscape one-liner — `usable seed: <label> (vendored slot fresh/stale/absent)` on the happy path, or the corresponding remediation hint when no usable seed exists. Always `ok: true` (informational) so installed-binary users — who legitimately bootstrap via the release-pipeline download path, not the source-checkout vendored slot — don't see a spurious doctor failure.

#### Reuse of upstream's `rootfs_contains_builder_init` byte-scan

The Stage 0 seed-discovery filter was already PR-316 + upstream's `find_local_stage0_bootstrap_image` which closes over `rootfs_contains_builder_init` — a streaming byte-scan that checks whether the rootfs.ext4 contains the literal path `/sbin/mvm-builder-init`. W7 layers on top of that: the vendored slot is just one more candidate dir that goes through the same filter. A vendored slot whose rootfs was built before commit 843ef18 (which added the init binary to the dev-image flake) is detected and skipped the same way `~/.mvm/dev/current/` would be. The byte-scan is preferred over the W5 `manifest.json` sidecar check for selection because it inspects the actual rootfs rather than its declared metadata — W5's manifest stays as the post-pick sanity check (PR #316).

#### Test surface

| Test | Layer | Gate |
|------|-------|------|
| Unit: `find_local_fallback_image_with_workspace_root` picks up the vendored slot when it's the only candidate | mvm-cli | every PR |
| Unit: `find_local_fallback_image_with_workspace_root` returns `None` when neither `~/.mvm/dev/` nor workspace_root has files | mvm-cli | every PR |
| Unit: `find_local_fallback_image_with_workspace_root` picks the newer of vendored vs `current/` by mtime | mvm-cli | every PR |
| Unit: `stage0_seed_doctor_status_at` reports `vendored_usable=true` when the rootfs contains `/sbin/mvm-builder-init` | mvm-cli | every PR |
| Unit: `stage0_seed_doctor_status_at` reports `vendored_present=true, vendored_usable=false` when the rootfs is sanity-passing but lacks the init binary | mvm-cli | every PR |
| Unit: `stage0_seed_doctor_status_at` reports all-`false` on a host with no candidates | mvm-cli | every PR |

#### Out of scope for W7

- Auto-running `cargo xtask build-dev-image` from inside `mvmctl dev up`. Keeping the host-Nix invocation as an explicit contributor-side step preserves the "mvmctl never uses host Nix at runtime" invariant in CLAUDE.md. The error message names the exact command; that's the user-visible affordance, not auto-execution.
- A CI lane that exercises the full source-checkout bootstrap end-to-end (host Nix install → xtask → mvmctl dev up → microVM boots). That's Plan 77 W8 — separate, follow-up PR, prevents future contract drift from regressing into this trap.
- macOS GitHub Actions E2E. No GitHub-hosted macOS-with-libkrun runner exists today; revisit when a self-hosted one is available.

### W6 — host-side kernel-panic detector in `spawn_supervisor_and_wait`
Defense in depth so the next contract drift (or any kernel-bring-up failure that W5 doesn't catch) doesn't hang the host. Lives in `crates/mvm-build/src/libkrun_builder.rs::spawn_supervisor_and_wait` because that's the function that owns the supervisor child handle and knows the `console_output_path` from the `SupervisorConfig` it serializes.

- Before spawning the child, capture `cfg.krun.console_output_path` (`Option<&str>`). When `None`, behavior is unchanged.
- After spawning + before `child.wait()`, start a watcher thread via `std::thread::scope` (so it can't outlive the function and is guaranteed joined before return):
  - Polls for the console-log file to appear (libkrun creates it on first hvc0 write — typically within 100 ms of spawn).
  - Once it exists, opens it and reads forward through any newly-appended bytes every ~100 ms.
  - Detection predicate matches the Linux kernel's stable panic banner: `Kernel panic - not syncing:` (a single substring match; the version-stable prefix has been unchanged in upstream `kernel/panic.c` for >a decade).
  - On detection: store the first matching line in a `Mutex<Option<String>>`, call `child.kill()` (SIGKILL, which bypasses the SIGTERM-brittle handling documented in `reference_libkrun_gotchas.md`), then exit the watcher.
  - Exits cleanly when `child.wait()` returns (the main thread sets a shared atomic flag the watcher checks each poll).
- After `child.wait()` returns, inspect the panic-detector result:
  - If the watcher captured a panic line, return `BuilderVmError::SeedKernelPanic { line, console_log_path }` regardless of exit code. The caller (`run_build` → Plan 77 W1's `run_stage0_bootstrap`) maps this to `Stage0FailureStage::Build` with a `reason=kernel_panic` tag.
  - Otherwise, return the exit code as today.

The watcher is opt-in via the `console_output_path` already being set — no new config surface. Latency from panic to host-visible failure is ≤ 500 ms in practice. The same path catches non-contract-drift panics (e.g. wrong kernel cmdline, missing virtio-blk module, `EXT4-fs: VFS: Can't find ext4 filesystem`) so future bring-up regressions surface promptly too.

## Security considerations

The invariant in AGENTS.md / CLAUDE.md is the security spine of this plan. Each item below maps to a specific failure mode I considered before recommending this design.

### 1. Trust boundary
The Stage 0 VM is the contributor's own dev image, booting the contributor's own flake, against the contributor's own workspace. **No new trust boundary** is introduced beyond what `mvmctl dev shell` already grants. A contributor who runs `mvmctl dev up` is already trusting the dev image's rootfs (it's loaded into a libkrun VM with `do_exec` enabled).

### 2. Workspace mount is read-only
`/work` is configured with `read_only = true` in `VmStartConfig.volumes`. The Stage 0 VM cannot mutate the contributor's source checkout. This matches the existing builder VM contract (where `/work` is also r/o).

### 3. Output dir is staged before promotion
Stage 0 writes to a unique staging dir (`~/.cache/mvm/builder-vm/<arch>-staging-<pid-timestamp>/`). After the VM exits:
- Validate `vmlinux` + `rootfs.ext4` both present
- Validate `rootfs.ext4` size ≥ minimum sane threshold (~16 MiB)
- Validate `rootfs.ext4` carries the ext4 magic `0x53EF` at offset `0x438` (matches the existing dev image cache-poisoning check)
- Only on all validations passing: `rename(2)` staging → cache dir

A failed or partial build never replaces a working cache; a partial cache cannot be served to subsequent `dev up` invocations. The "no silent fallback" invariant is preserved — Stage 0 failure surfaces a clear error and the contributor sees the underlying problem.

### 4. No new attack surface beyond existing dev shell
The dev image's mvm-guest-agent already exposes `do_exec` over vsock (`dev-shell` feature gate; security claim 4 in CLAUDE.md). Stage 0 reuses that exact RPC. Any vulnerability in the agent's exec handler already affects regular `mvmctl dev shell`. **Stage 0 introduces zero new code paths in the agent.** This is deliberately the case: the agent surface stays unchanged so we don't enlarge the attack surface of the production agent (which still has `do_exec` stripped per claim 4).

### 5. Network via libkrun TSI
Stage 0 has no virtio-net interface. libkrun's transparent socket impersonation (TSI) hijacks AF_INET/AF_INET6 socket calls and routes them through the host network stack. Same posture as the existing dev shell. DNS resolution requires a usable resolver inside the VM — the current dev image rootfs ships with one (or doesn't; this is the secondary issue tracked in Plan 72 W5.D). If DNS is broken, the nix build inside Stage 0 fails the same way `mvmctl dev shell` + `nix build` would. **Stage 0 is not a DNS fix.**

### 6. Concurrency safety
Two `mvmctl dev up` invocations on the same host could race the builder VM cache. W2 adds an `fcntl`-based advisory lock at `~/.cache/mvm/builder-vm/stage0.lock`:
- Acquire `LOCK_EX` on Stage 0 entry; bail with a clear "another Stage 0 build is in progress" message if already held
- Release on exit (RAII guard)

Worktrees share the same `~/.cache/` so the lock naturally serializes across them. This is consistent with the AGENTS.md "Builder VM sharing" guidance.

### 7. Bounded exec timeout
The vsock exec request carries `timeout_secs = 1800` (30 min). A hung build (network stall, Nix daemon wedge) does not leave a zombie VM holding host resources. On timeout: the VM is forcibly stopped, staging dir cleaned up, the cache is not promoted.

### 8. Audit chain entries
Stage 0 emits chain-signed audit entries to `~/.mvm/audit/stage0.jsonl`:
- `stage0.boot { vm_name, dev_image_kernel_sha, dev_image_rootfs_sha, workspace_path }`
- `stage0.exec.started { nix_attr, timeout_secs }`
- `stage0.exec.completed { exit_code, stdout_bytes, stderr_bytes, duration_ms }`
- `stage0.cache.promoted { staging_path, cache_path, vmlinux_sha256, rootfs_sha256 }`
- `stage0.failed { reason, staging_path }` (cleanup happens after emit)

The chain-signing scheme reuses `mvm_supervisor::audit::AuditEmitter` (claim 8). Stage 0 is a contributor-only path, so the rigor is "diagnosable after the fact" rather than "production-non-repudiable" — `mvmctl audit verify` still passes if the chain isn't tampered.

### 9. No download fallback on source-checkout flow
W4 closes the door on `download_builder_vm_image` being reachable from the source-checkout path. The function still exists for the future end-user-binary release case, but gated behind a feature flag that's off by default. The invariant is enforced by code structure, not by runbook.

### 10. mvm-guest-agent `do_exec` is not enlarged
Per claim 4, prod agent builds omit `do_exec`. Stage 0 only ever boots a **dev image** agent (the one in `~/.mvm/dev/current/rootfs.ext4`, built from `nix/images/builder/flake.nix` which always sets `entrypoint.shell` → dev variant). Stage 0 does not, and cannot, target a production-agent rootfs. The CI gate `prod-agent-no-exec` continues to assert the symbol is absent from prod builds.

### 11. Verity sidecar handling
The builder VM rootfs built by Stage 0 will not carry a dm-verity sidecar (claim 3) unless the in-repo flake produces one. That's by design: the builder VM is a contributor-side artifact, not a workload-side rootfs. Production workloads continue to boot from verity-signed rootfs (claim 3 unchanged). **Stage 0 doesn't lower the production security posture.**

### 12. Cache poisoning via malicious flake?
A contributor with write access to `nix/images/builder-vm/flake.nix` can change what the builder VM does. That's already the case — they could change `nix/images/builder/flake.nix` (the dev image) and `mvmctl dev shell` would happily boot it. Stage 0 doesn't add a new vector; it relies on existing source-checkout trust.

For an end-user-binary install (separate codepath from this plan), the source flakes don't exist on disk; the contributor-side flake-injection vector doesn't apply.

### 13. W5 manifest is metadata, not a trust anchor
The `manifest.json` sidecar declares what the dev image flake intended to ship, not what the rootfs actually contains. A malicious local flake could lie. That's accepted because Plan 77's overall trust boundary is unchanged (see consideration 12 and 1): a contributor with write access to `nix/images/builder/flake.nix` already controls what Stage 0 boots. W5 catches **honest drift** (stale cache, missed flake update, version skew with the host binary) — it's a UX + correctness check, not an integrity check.

For signed end-user-binary release artifacts the dev-image manifest is already cosign-verified end-to-end via `mvmctl dev import-image` (ADR 005 / Plan 36). W5 adds nothing to that trust path and removes nothing from it; it only reads metadata the flake already emits.

### 14. W6 watcher cannot escalate
The kernel-panic watcher thread runs in the parent `mvmctl` process (or the test harness, in tests), reads a host-side file the supervisor writes to, and on detection calls `Child::kill()` on a process it spawned. It does not interact with the guest, has no vsock, and has no privilege beyond what the parent already has over its own child. The watcher's failure modes are bounded:

- Watcher panics → scoped thread's panic propagates; `wait` is still called via the scope's join logic.
- Watcher races with a clean exit → the atomic "child exited" flag is set, watcher's next poll exits its loop, no kill is issued.
- Watcher false-positive on a non-panic line containing the banner literal → the seed is rejected and the contributor sees the captured line in the error. False positives are diagnosable from the console log; there's no silent fallback.

## Test strategy

| Test | Layer | Gate |
|------|-------|------|
| Unit: `bootstrap_via_dev_image_stage0` happy path with mock `LibkrunBackend` | mvm-cli | every PR |
| Unit: dev image missing → clean error | mvm-cli | every PR |
| Unit: `/out` empty after build → fail closed, no cache promotion | mvm-cli | every PR |
| Unit: `rootfs.ext4` magic check rejects a stub | mvm-cli | every PR |
| Unit: concurrent invocations serialize via lock | mvm-cli | every PR |
| Unit: audit emit produces expected entries | mvm-cli | every PR |
| Integration: clean `~/.cache/mvm/builder-vm/` + present `~/.mvm/dev/current/` → `mvmctl dev up` succeeds | host | nightly on macOS + Linux KVM runners |
| Integration: byte-equivalence of resulting `rootfs.ext4` between two consecutive Stage 0 runs on the same flake source | host | nightly |
| Negative integration: forced exec timeout → VM torn down, cache unchanged | host | nightly |
| Unit: `validate_stage0_seed_contract` rejects missing `manifest.json` | mvm-cli | every PR |
| Unit: `validate_stage0_seed_contract` rejects manifest with `contract_version` below required | mvm-cli | every PR |
| Unit: `validate_stage0_seed_contract` rejects manifest missing required `init_paths` entry | mvm-cli | every PR |
| Unit: `validate_stage0_seed_contract` rejects wrong `image_kind` | mvm-cli | every PR |
| Unit: `validate_stage0_seed_contract` accepts a well-formed manifest | mvm-cli | every PR |
| Unit: panic detector kills a fake child within ≤ 500 ms when the test writes the banner line to a temp console log | mvm-build | every PR |
| Unit: panic detector does not kill a fake child that exits cleanly without the banner | mvm-build | every PR |
| Unit: panic detector tolerates a delayed console-log creation (file appears after spawn) | mvm-build | every PR |

The unit-test layer keeps the PR-level signal high without needing a libkrun-capable CI runner for every iteration.

## Sequencing

- **W0** (this plan) — invariant + refactor + plan doc. Behavior unchanged. Ships as `docs/builder-vm-flow` PR.
- **W1** — `bootstrap_via_dev_image_stage0` + wire into `bootstrap_builder_vm_image`. Drop the source-checkout download path. End-to-end `dev up` works on a host with a cached dev image.
- **W2** — host-side lock + atomic promotion.
- **W3** — audit emit + the test matrix above.
- **W4** — close the download path behind `#[cfg(feature = "release-artifact-bootstrap")]`.
- **W5** — `manifest.json` sidecar emission + `validate_stage0_seed_contract` preflight. Hard error on contract drift before any VM boot.
- **W6** — kernel-panic detector inside `spawn_supervisor_and_wait`. Defense in depth: future contract drift, bring-up regressions, or any in-VM panic surfaces in ≤ 500 ms on the host.

W1 is the load-bearing slice. W2–W4 harden it. W5 + W6 close the contract-drift hole that surfaced on 2026-05-15.

## Rollback

W1's diff is contained to `crates/mvm-cli/src/commands/env/apple_container.rs` and the `LibkrunBuilderVm` refactor that W0 establishes. Reverting both leaves `mvmctl dev up` failing in the same way as today's main (the missing-cache + 404-download error), so no behavior regression from the current broken state.

W2/W3/W4 are additive over W1. W5 and W6 are additive over W1–W4: reverting either one leaves the host with no preflight check and no panic detector respectively, restoring the failure mode that produced this plan's "Why W5 + W6 were added" section but not breaking any other path. The dev-image flake's new `manifest.json` is read-only metadata; older mvmctl binaries simply ignore the file. Older dev images without the manifest are detected by W5's "missing manifest" branch.

## Out of scope

- **mvm-oci-based Stage 0** — the eventual Plan 75 design that pulls a published Nix-bearing OCI image and boots it without depending on a cached dev image. That handles the true clean-slate case where the host has neither a dev image nor a builder VM image. Plan 77 covers the contributor-with-dev-image case; Plan 75 W1+ covers the contributor-with-nothing case. Both can coexist; Stage 0 first checks for the dev image, falls through to mvm-oci pull if missing.
- **Publishing builder VM release artifacts** — forbidden by the invariant until a release is explicitly cut. Plan 77 does not add a release-workflow job for the builder VM. End-user-binary download support is a separate, future, gated codepath.
- **DNS / network inside the Stage 0 VM** — covered by Plan 72 W5.D's nine-bug list. Stage 0 inherits whatever network posture the dev image already has. Fixing DNS-in-VM is orthogonal.
- **Apple Container backend** — Stage 0 uses libkrun specifically. Apple Container's macOS-26+ path is the **runtime** path; the builder VM image is what its `mvmctl dev up` ends up booting, but the Stage 0 *build* of that image uses libkrun on every macOS variant.
