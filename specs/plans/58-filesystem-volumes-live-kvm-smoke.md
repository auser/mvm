# Plan 58 — Filesystem-volumes live KVM smoke fixture

> **Status:** deferred — needs real KVM-capable hardware to execute. Companion to [Plan 45 §"Verification" Phase 11](./45-filesystem-volumes-e2b-parity.md).
> **Why a separate plan:** Phases 1–10 of Plan 45 (workspace + mvm-side foundation) are complete and merged-able as a self-contained unit. Phase 11 (live KVM smoke) needs hardware that no longer fits in a software-only PR — capturing here so the work isn't lost in the Plan 45 §"Out of scope" backlog.

## Context

Plan 45 introduced the `Volume` primitive end-to-end across mvm:
- `mvm-core::volume` wire types (Phase 1)
- `mvm-storage::Backend` trait + `LocalBackend` impl (Phase 2)
- mvm-runtime `volume_registry` replacing `share_registry` (Phase 5)
- mvm-cli `volume` subcommand replacing `share` (Phase 6)
- mvm-guest `MountVolume` / `UnmountVolume` vsock verbs (Phase 7)
- `mvm-security::policy::MountPathPolicy` extended for `/nix*` denials (Phase 8)
- `mvmctl doctor` host-FDE check (Phase 9)
- `mkGuest.volumeMounts` Nix attrset extension (Phase 10)

All ten phases are tested with unit + integration tests against in-memory fixtures. What's missing is a **live, end-to-end test** that boots a real Firecracker microVM and exercises the full mount path: host-side virtiofsd spawn → Firecracker virtio-fs device attach → guest agent `MountVolume` vsock call → in-guest mount(2) → file read/write across the boundary.

## Why this is hardware-bound

The smoke fixture exercises code paths that don't run on a developer's macOS box:
- `libc::mount(2)` — Linux-only syscall, gated `cfg(target_os = "linux")`.
- `virtiofsd` — host-side daemon (Linux-only binary).
- Firecracker — needs `/dev/kvm`.
- Real virtio-fs device emulation in the guest kernel — confirmed only inside an actual microVM.

Lima VM on macOS provides a Linux + KVM environment, but spinning up nested Firecracker inside Lima for CI doubles boot time and complicates the diagnostics. Plan 25 §W3 (verified-boot smoke) settled on a dedicated KVM-capable host for this class of test; Plan 58 follows the same pattern.

## Fixture design

### Setup

- Build target: `crates/mvm-runtime/src/vm/template/lifecycle.rs` extended with a `volume_smoke_lifecycle` test fn, gated `#[cfg(feature = "live-kvm-smoke")]` so it doesn't pollute the normal `cargo test` run.
- Reuse the existing W3 verity fixture's microVM image (`nix/images/default-tenant`) with `mkGuest.volumeMounts` declaring two mount points:
  ```nix
  volumeMounts = {
    "/mnt/scratch" = { volume = "smoke-scratch"; readOnly = false; };
    "/mnt/inputs"  = { volume = "smoke-inputs";  readOnly = true;  };
  };
  ```

### Scenarios

1. **Single-VM round-trip**:
   - Create volume `smoke-scratch` (host directory at `~/.mvm/volumes/smoke-scratch/`).
   - Boot VM with `--volume smoke-scratch:/mnt/scratch:rw`.
   - From inside guest: write `/mnt/scratch/payload.txt` with a known string.
   - Tear down VM.
   - Assert: file exists in `~/.mvm/volumes/smoke-scratch/payload.txt` with the expected content.

2. **Persistence across reboot**:
   - Reboot a fresh VM mounting `smoke-scratch`.
   - From inside guest: read `/mnt/scratch/payload.txt`; assert content matches.

3. **Multi-attach proof**:
   - Boot a *second* VM mounting `smoke-scratch` at `/mnt/scratch` while the first is still running.
   - Both VMs see the same file.
   - One VM writes; the other VM (after a brief cache-flush wait) sees the update.
   - Note: virtiofs cache coherence is the real determining factor here; the test asserts the *eventual* shape, not strict-ordering, to match e2b's documented "best-effort consistency" stance.

4. **Read-only enforcement**:
   - Boot VM with `--volume smoke-inputs:/mnt/inputs:ro`.
   - From inside guest: attempt to write `/mnt/inputs/foo` → assert EROFS.
   - Confirms the read-only flag propagates through both the mount-flags layer (`-o ro`) and the trait dispatch.

5. **Scope-isolation regression**:
   - Create volumes `scratch` in `(local, default)` and `(local, ws-other)`.
   - Confirm they have distinct host directories.
   - Verify HKDF derivation produces unrelated AEAD keys (already covered by `key_derivation` unit tests; this just sanity-checks end-to-end).

6. **Nix path denial regression**:
   - Boot VM with `--volume scratch:/nix/store/xxx` → asserts `MountPathPolicy::Denied` before any boot work happens.

### Tear-down

Each scenario is hermetic: setup creates fresh volumes in a tempdir, tears them down on completion. Failures emit virtiofsd stderr + the agent's `tracing` output for triage.

## Required hardware

- A KVM-capable Linux host (bare-metal or cloud — Hetzner CCX22, GCP n2-standard-2 with nested-virt, etc.).
- mvmctl + the dev-microvm rootfs on that host.
- virtiofsd ≥ 1.7 (matches what the W3 verified-boot lane already exercises).

## Coupling with W3 fixture

Plan 27's W3 fixture (`runbooks/w3-verified-boot.md`) already has the host-setup boilerplate: kernel build, dev image fetch, virtiofsd binary install. Plan 58 reuses that runbook with one added prerequisite (`mvmctl volume create smoke-scratch` before running the smoke).

## Exit criteria

- All six scenarios pass on the W3 fixture host (CI lane: `live-volume-smoke`).
- `cargo test --features live-kvm-smoke -p mvm-runtime --test volume_smoke` is green.
- Output is captured into `specs/runbooks/plan-58-volume-smoke.md` with the actual virtiofsd / agent log excerpts so future debugging has a known-good baseline.

## Out of scope (Plan 58 itself)

- Performance benchmarking — call as a follow-up sprint once the correctness fixture lands.
- mvmd-side workspace-scoped attach (Sprint 137 W3) — that has its own integration tests against MinIO; the live-KVM lane only covers `LocalBackend`.
- Multi-host (NFS / CephFs) backends — Plan 45 §B2, separate sprint.
