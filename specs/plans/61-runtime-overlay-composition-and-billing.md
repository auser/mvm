# Plan 61 — Runtime overlay composition (transparent dev/prod) + sandbox-runtime usage billing

## Context

mvm needs to support both fully-featured dev images and slim/secure prod images, plus a usage-based billing model comparable to other sandbox-runtime platforms (per-vCPU-time, per-GB-RAM-time, concurrent-sandbox caps, max session duration, etc.). Both concerns share the same touchpoints — VM lifecycle hooks, image fetch, disk attach — so they are sequenced in one plan.

The dev/prod image axis was originally framed as "rootless vs busybox containers." That framing doesn't apply: every mvm rootfs is already rootless-by-construction (W2.1–W2.4: per-service uid, setpriv `--bounding-set=-all --no-new-privs`, RO `/etc/passwd`, seccomp Standard) and busybox-based (`mkGuest` at `nix/lib/mk-guest.nix`). The real axis is **what extra tools live in the rootfs at boot**, and the design constraint is **transparency — the runtime decides based on invocation context**, not the user.

Architectural prerequisites (already done):
- Lima removed; first-class providers in `crates/mvm-providers/src/{libkrun,apple_container}` (commit 8ef455f).
- `StartMode` + `start_with_mode` + wait + detach (commit b50dec6).
- User-flake-centric dev-image docs (commit 3b0485b).

Decisions captured in the ADRs:
- ADR-039 — runtime overlay composition (this plan implements it).
- ADR-040 — usage-based billing model (this plan ships the metering surface it needs).

## Scope

### In scope

**Overlay composition (Phases 1–4):**
- A curated dev-tools overlay artifact built by mvm CI, versioned per mvmctl release, hash-verified (W5.1).
- Provider extension for additional read-only block devices (apple-container live; libkrun deferred-attach).
- CLI surface: `mvmctl run --debug`, `mvmctl dev` (overlay default), `mvmctl dev --run-service`, `mvmctl debug <vm>` (live attach).
- Workload init script extension to detect and mount the overlay.

**Billing / usage metering (Phase 5):**
- Per-VM metering event stream: lifecycle (start, stop, pause, resume), resource samples (vCPU-seconds, RSS bytes), disk-attach (overlay/volume), network egress counters, build-minute counters.
- Stable JSONL event schema in `mvm-core` consumed by mvmd for aggregation/billing.
- Runtime-enforced caps surfaced from mvmd (max session duration, max concurrent VMs per tenant) — read by the runtime, enforced at start/stop boundaries.
- `mvmctl usage` local command for development-time metering inspection.

### Out of scope

- The actual billing UI / Stripe integration / invoice generation — lives in mvmd.
- Cross-host metering aggregation — mvmd's job.
- Hot-attach on libkrun — first cut uses stop/restart-with-overlay; true hot-attach gated on libkrun upstream.
- Project-specific dev tools (psql, cargo) — explicitly punted; users add to `packages` if they accept prod-bloat.

## Phases

### Phase 1 — Overlay artifact and CI

- New flake: `nix/dev-overlay/flake.nix` producing per-arch ext4 image with curated tools (bash, coreutils, util-linux, busybox-extras, curl/wget/jq/less/vim-tiny, strace/lsof/tcpdump/dig, htop/procps, git). Target ≤ 50 MB; CI hard-fails over 75 MB.
- CI lane in `.github/workflows/release.yml`: build overlay per arch, sign with cosign, emit `dev-overlay-<arch>-checksums-sha256.txt`. Tag matches mvmctl version.
- ADR-039 covers contents + versioning rationale.

### Phase 2 — Fetch + verify infrastructure

- Generalize the W5.1 verifier currently in `download_dev_image` (under `crates/mvm-cli/src/commands/env/apple_container.rs::ensure_dev_image`). Lift into a shared helper at `crates/mvm-cli/src/commands/env/verified_fetch.rs` that handles both prebuilt dev image and dev overlay.
- Cache layout: `~/.mvm/dev/overlay/v<mvmctl-version>/<arch>.ext4` + `<arch>-checksums-sha256.txt`. Mode 0700 on the directory (W1.5).
- `MVM_SKIP_HASH_VERIFY=1` honored as the existing emergency escape; CI denies.
- Tests: positive fetch, hash-mismatch rejection + redownload, cosign-signature failure, partial-download resumption.

### Phase 3 — Provider API: extra disks + live attach

- `crates/mvm-core/src/protocol/vm_backend.rs`: extend `VmStartConfig` with `extra_disks: Vec<DiskAttachment>` where `DiskAttachment` carries `path: PathBuf, read_only: bool, label: Option<String>`. Backwards-compatible (empty default).
- `crates/mvm-providers/src/apple_container/mod.rs`: thread `extra_disks` into `VZVirtioBlockDeviceConfiguration` list. Add new `attach_disk(vm_handle, DiskAttachment)` for hot-attach (Apple Virtualization supports virtio-blk hot-add).
- `crates/mvm-providers/src/libkrun/mod.rs`: thread `extra_disks` via `krun_add_disk` (or whatever the libkrun-spike Plan 57 settles on). Live-attach is **deferred** — `attach_disk` returns `Error::NotYetWired` on libkrun and the CLI falls back to "stop, restart with overlay attached, restore state" with a warning.
- Round-trip serde test for `extra_disks`.
- Provider integration test: prod VM running, attach overlay (apple-container), assert it appears in guest as `/dev/vd[bc]`.

### Phase 4 — Workload init script + CLI surface

- Init script in `nix/lib/mk-guest.nix` (the embedded init): add a fifth stage *before* exec'ing the entrypoint. Scan `/dev/disk/by-label/mvm-dev-overlay`; if present, mount RO at `/usr/dev`, prepend `/usr/dev/bin` to `PATH`. POSIX shell only; busybox-only environment. Skip silently if absent.
- Kernel cmdline override: `mvm.entrypoint=<path>` honored by init when set; used for `mvmctl dev` to override declared entrypoint to `/bin/bash`.
- `crates/mvm-cli/src/commands/env/dev.rs`: refactor to fetch overlay (not a separate prebuilt dev image) + compose with user's `.default` workload. Add `--run-service` flag.
- `crates/mvm-cli/src/commands/run.rs`: add `--debug` flag that pulls overlay into start config.
- New: `crates/mvm-cli/src/commands/debug.rs` for `mvmctl debug <vm>` live-attach. Routes to provider `attach_disk`.
- Boot-time test: existing 300ms p50 boot smoke still passes; new overlay-mount test ≤ 20 ms p50.

### Phase 5 — Usage metering (sandbox-runtime billing dimensions)

This phase implements ADR-040. The runtime emits events; mvmd aggregates and bills.

- New module `crates/mvm-core/src/usage.rs`: defines the canonical event schema (see ADR-040 for full enumeration). Types are `#[serde(deny_unknown_fields)]`.
- New module `crates/mvm/src/usage/recorder.rs`: writes JSONL events to `~/.mvm/usage/<vm-id>/events.jsonl`. Append-only, durable, fsync on flush boundary.
- Lifecycle hook integration in `crates/mvm/src/vm/instance/lifecycle.rs`: emit `Started`, `Stopped`, `Paused`, `Resumed`, `DiskAttached`, `DiskDetached`.
- Periodic resource sampler: every 10 s while running, emit `ResourceSample { vcpu_s, rss_bytes, disk_bytes_read, disk_bytes_written, net_bytes_in, net_bytes_out }`. Sampling source: provider-specific (apple-container exposes via Virtualization framework stats; libkrun via `krun_get_stats` or proc-side polling of the host VMM process).
- Build minute accounting: `crates/mvm-build/src/dev_build.rs` emits `BuildStarted` / `BuildFinished` with elapsed wall-clock and the workspace ID it built for.
- Caps enforcement: `crates/mvm/src/vm/instance/caps.rs` reads `~/.mvm/caps.json` (populated by mvmd in fleet mode; user-editable in dev). Enforces:
  - `max_session_duration_secs` — VM is force-stopped on expiry; emits `LimitHit { kind: Duration }`.
  - `max_concurrent_per_tenant` — `start` returns `Error::QuotaExceeded` and emits `LimitHit { kind: Concurrent }`.
  - `max_vcpu_per_vm`, `max_ram_mib_per_vm` — `start_with_config` rejects oversize.
- New CLI: `mvmctl usage` and `mvmctl usage --since <ts> --until <ts>` for local inspection. Format: tabular summary + `--json` for raw events.

### Phase 6 — Docs and ADRs

- `public/src/content/docs/guides/dev-image.md`: rewrite around the overlay model. Make clear users declare workloads, mvm composes the rest.
- `public/src/content/docs/reference/usage-events.md`: new — documents the event schema as a stable public surface.
- `CLAUDE.md`: replace Lima-era "the dev environment is the Lima VM" prose with overlay-based language. Add billing model summary pointing to ADR-040.
- `specs/SPRINT.md`: new sprint section pointing at this plan.

## Critical files

**Read first** (existing patterns):
- `nix/lib/mk-guest.nix` — image builder + embedded init.
- `crates/mvm-cli/src/commands/env/apple_container.rs` — `ensure_dev_image`, W5.1 verifier usage, launchd install.
- `crates/mvm-providers/src/{apple_container,libkrun}/mod.rs` — provider scaffolds.
- `crates/mvm-core/src/protocol/vm_backend.rs` — `StartMode`, `VmStartConfig`.
- `crates/mvm/src/vm/tenant/quota.rs` — existing `TenantUsage` / `TenantQuota` (overlap with new metering — see Compatibility).
- `specs/plans/{25,26,27}-*.md` — security floor (must remain intact).
- `specs/plans/45-filesystem-volumes.md` — prior sandbox-runtime feature parity work; reuse where it overlaps with billing dims.

**Modify or create** (full list in Phase descriptions above).

## Compatibility notes

- `TenantUsage` / `TenantQuota` in `mvm-core::tenant` and `mvm::vm::tenant::quota` are **fleet-level rollups** (Postgres-backed in mvmd). The new `usage` module in this plan is the **per-VM event stream** that feeds those rollups. They do not conflict; the rollups become consumers of the JSONL stream.
- mvmd's `BuildEnvironment` impl in `mvmd-runtime` keeps working; `BuildStarted` / `BuildFinished` are emitted via the existing `record_revision` hook point.

## Verification

End-to-end happy-path matrix:

1. **`mvmctl run` (sealed prod)**: workload boots; `which strace bash` fail; `~/.mvm/usage/<vm-id>/events.jsonl` shows `Started`, periodic `ResourceSample`, eventual `Stopped`.
2. **`mvmctl run --debug`**: same workload boots; `which bash strace` resolve; events also include `DiskAttached { kind: Overlay }`.
3. **`mvmctl dev` (default)**: drops to bash prompt; workload binaries present at their declared paths.
4. **`mvmctl dev --run-service`**: declared service runs; `mvmctl console <vm>` opens overlay-tooled shell.
5. **`mvmctl debug <vm>` (apple-container)**: overlay hot-attaches to running prod VM without service interruption.
6. **`mvmctl debug <vm>` (libkrun, deferred)**: warns user; falls back to stop+restart+overlay path.

Hash and security regressions:
7. SHA-256 of workload rootfs **identical** between `run` and `dev` runs.
8. W3 dm-verity: workload roothash check passes in both modes; tampered overlay (single byte flip) panics at mount time before userspace.
9. W2 regression suite passes under `dev` — per-service uid intact, RO `/etc/passwd` intact, setpriv flags intact.
10. W4.3 `prod-agent-no-exec` lane unchanged.
11. `~/.mvm/dev/overlay/` mode 0700; corrupted cached overlay → reject + redownload + succeed on retry.

Boot-time budget:
12. Existing 300ms p50 boot smoke passes for `mvmctl run` (no overlay path).
13. New overlay-mount adds ≤ 20 ms p50.

Billing / metering:
14. `mvmctl usage --json` produces well-formed events conforming to the `mvm-core::usage` schema (serde round-trip).
15. Force-killing mvmctl mid-run: events written so far are durable (fsync-on-flush).
16. Cap enforcement: VM with `max_session_duration_secs = 60` is force-stopped at T+60s and emits `LimitHit { kind: Duration }`.
17. Cap enforcement: 11th concurrent VM with `max_concurrent_per_tenant = 10` is rejected at start with `Error::QuotaExceeded`.
18. Sampler: `ResourceSample` events appear at ~10 s cadence; non-zero `vcpu_s` deltas while VM is busy; zero deltas while paused.
19. Build minute accounting: `mvmctl build` emits a `BuildStarted` and `BuildFinished` pair; elapsed matches wall-clock within 100 ms.

## Risks and follow-ups

- **libkrun live-attach**: deferred. First cut works on apple-container; libkrun gets a stop/restart fallback. Tracked as a follow-up gated on libkrun upstream.
- **Overlay closure size discipline**: ~30–50 MB target. CI fails at >75 MB. If creep is real, split into "overlay-min" (10 MB: bash, coreutils only) and "overlay-full" (50 MB: full set) — keeping selection runtime-driven, not user-driven.
- **Sampling overhead**: 10 s cadence is conservative. Profile under `mvmctl dev` workload; if sampler costs >0.5% CPU, drop to 30 s.
- **Cap config trust boundary**: in fleet mode, `caps.json` is written by mvmd and the runtime trusts it. In standalone dev mode, it's user-editable — not a billing-grade trust path. Document explicitly in ADR-040.
- **Overlay update cadence**: pinned to mvmctl release. CVE in (say) curl → fix ships next mvmctl. Acceptable for dev-only scope; documented in ADR-039.
- **Project-specific dev tools**: explicitly out of scope. If friction surfaces, future plan can add a per-project secondary overlay (overlay-of-overlays); the runtime composition layer already supports the disk count.
