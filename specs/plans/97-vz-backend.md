# Plan 97 — `Virtualization.framework` backend (`vz`)

> **Status:** Phase A — supervisor binary scaffolded and building;
> end-to-end boot acceptance + Rust-side fuzz target are the remaining
> Phase A items. ADR-056 deferred until Phase D.
>
> Pick-up command for fresh sessions: read this file top to bottom, then
> jump to the next unchecked item in the **Progress checklist** below.

## Progress checklist

Top-level phases:

- [ ] **Phase A** — `mvm-vz-supervisor` Swift binary (smallest tracer)
- [ ] **Phase B** — `VzBackend` impl in `crates/mvm-backend/src/vz.rs`
- [ ] **Phase C** — Vz as a builder-VM backend
- [ ] **Phase D** — ADR-056 lands + ADR-002 backend table update
- [ ] **Phase E** — Snapshot / save-restore (macOS 14+)

Phase A sub-tasks:

- [x] Worktree on `worktree-vz-backend-phase-a` set up off `origin/main`
- [x] `crates/mvm-vz-supervisor/Package.swift` Swift package skeleton
- [x] `Sources/mvm-vz-supervisor/Config.swift` — Codable mirror of
      the libkrun supervisor JSON schema (`#[serde(deny_unknown_fields)]`
      equivalent on the Swift side via strict `JSONDecoder` —
      `StrictKeys` protocol + `checkStrictKeys` helper)
- [x] `Sources/mvm-vz-supervisor/Supervisor.swift` — VZ machine config
      + start + SIGTERM forwarding
- [x] `Sources/mvm-vz-supervisor/VsockProxy.swift` — bidirectional
      unix-socket ↔ vsock proxy under `<socketDir>/vsock-<port>.sock`,
      mode 0700, via POSIX `accept()` + `DispatchIO` splice
- [x] `Sources/mvm-vz-supervisor/Network.swift` — gvproxy file
      handle attachment via `VZFileHandleNetworkDeviceAttachment`
      (SOCK_DGRAM unix connect to gvproxy's `--listen-vfkit` socket)
- [x] Ad-hoc code-signing with `com.apple.security.virtualization`
      entitlement (`Entitlements.plist` + `tools/build.sh`)
- [ ] Phase A acceptance: `mvm-vz-supervisor < config.json` boots the
      dev-shell image and host-side `vsock-connect 3:5252` succeeds
      *(deferred — needs Phase B's `VzBackend` to produce the JSON
      against real artifact paths)*
- [ ] Rust fuzz target `crates/mvm-vz/fuzz/fuzz_supervisor_config.rs`
      generating the corpus; Swift-side equivalence test reads the
      same corpus and asserts equivalent rejections (ADR-002 claim 5)
      *(deferred — Rust `mvm-vz` crate is a Phase B item)*

Phase B sub-tasks:

- [x] `crates/mvm-core/src/platform/platform.rs::has_vz()` detector
- [x] `crates/mvm-backend/src/vz.rs` — `VzBackend` impl of `VmBackend`
      with real start/stop/status/list/logs/install via supervisor
      subprocess + PID file (mirrors `LibkrunBackend`). `pause`/`resume`
      bail with capability-honest messages because the supervisor
      exposes only stdin-driven start/stop today; flips on when the
      control-socket follow-up lands.
- [x] `BackendKind::Vz` in `crates/mvm-backend/src/backend.rs`
- [x] `MVM_BACKEND=vz` / `--backend vz` opt-in plumbed; `auto_select()`
      **unchanged**
- [ ] Resource-cap parity check (vCPU / memory / disk size); fail-closed
      test asserts over-allocation refused (Security §8)
- [ ] Kernel cmdline allow-list enforcement; fail-closed test asserts
      `init=/bin/sh` injection refused (Security §7)
- [ ] `mvm_supervisor::admit_for_run` integration; fail-closed test
      asserts bypass refuses launch (ADR-002 claim 8)
- [ ] Console mode lockdown — capture-only on workload microVMs,
      PTY-over-vsock for dev mode only (Security §9)
- [ ] Phase B acceptance: `MVM_BACKEND=vz mvmctl run dev-shell` boots
      workload microVM directly on macOS without nested libkrun
- [ ] Hypervisor.framework concurrent-VM cap probe + clear error class
- [x] `mvmctl doctor` Vz availability check (entitlement / MDM-policy
      sub-probes pending — current check reports framework
      availability + supervisor-binary presence across the
      env-override / source-checkout / installed paths)

Phase C sub-tasks:

- [ ] `StartMode::BlockingWithIO` (or equivalent) added if not already
      in the trait
- [ ] Builder runtime selection in `crates/mvm/src/vm/` branches on
      `MVM_BUILDER_BACKEND=vz`
- [ ] Stage 0 audit emit + cache-prune contract participation
      (`project_stage0_audit_and_cache_prune_contract` memory)
- [ ] Phase C acceptance: `MVM_BUILDER_BACKEND=vz mvmctl build --flake
      .` produces byte-identical rootfs to libkrun-hosted equivalent

Phase D sub-tasks:

- [ ] `specs/adrs/056-vz-backend.md` — Why Vz, security tier (Tier 2
      proposed), relationship to ADR-013 / ADR-055, ADR-002 backend
      table update
- [ ] Performance numbers from CI lane (cold-boot, idle memory, build
      wall time) referenced in the ADR
- [ ] ADR-002 backend table updated with Vz row and claim-coverage
      markers
- [ ] macOS minor-version compatibility matrix wired into CI (min 13.x,
      current latest, one macOS-26+ build)

Phase E sub-tasks (macOS 14+):

- [ ] `snapshot.save_path` / `snapshot.restore_path` modes added to
      supervisor JSON schema
- [ ] Swift `saveMachineStateTo` / `restoreMachineStateFrom` wiring
- [ ] Rust `VmBackend::pause` / `resume` / snapshot verbs routed to
      supervisor IPC
- [ ] Snapshot file SHA-256 hash-pinned in audit chain;
      `verify_audit_chain` rejects tampered snapshots (Security §4)
- [ ] `VZGenericMachineIdentifier` persisted with snapshots and
      verified on restore (Security §10)
- [ ] `VmCapabilities::snapshots = true` on macOS 14+
- [ ] Phase E acceptance: `mvmctl snapshot save/restore` round-trips
      a dev-shell workload VM, restored VM preserves in-guest state
      and vsock agent sessions

Cross-cutting (any phase):

- [ ] Build, distribution, versioning (Swift toolchain in CI,
      `Package.resolved` pinned, distribution signing + notarization
      runbook entry, lockstep version pinning with `mvmctl`,
      source-checkout determinism)
- [ ] License & Swift package conventions (Apache-2.0 + MIT dual)
- [ ] mvmd integration follow-up (separate repo, separate PR)
- [x] Tracking issue filed for **future work: Windows host via WHP** — [#428](https://github.com/tinylabscom/mvm/issues/428)

## Context

Today on macOS, `mvm` always goes through **two layers** of virtualization to
run a workload microVM:

```
macOS host  →  libkrun Linux VM  →  Firecracker microVM (/dev/kvm)
```

That nesting exists because Firecracker requires `/dev/kvm`, which only exists
inside a Linux guest. libkrun (via `Hypervisor.framework`) hosts that Linux
guest. The whole pipeline assumes the macOS host can't run Linux directly,
*even though it can* — `Virtualization.framework` (Vz) has shipped Linux-guest
support since macOS 11 and exposes virtio-blk, virtio-net, virtio-vsock,
virtio-console, virtio-rng, and virtio-fs natively. Those are exactly the
device classes our guests already use (`crates/mvm-guest/src/vsock.rs:14`,
`DEFAULT_CMDLINE` at `crates/mvm-backend/src/libkrun.rs:62`).

Concretely the repo today exposes four backends behind a single trait
(`crates/mvm-core/src/protocol/vm_backend.rs:520`):

| Backend          | Hypervisor surface                    | Status     | Tier |
|------------------|---------------------------------------|------------|------|
| Firecracker      | Linux `/dev/kvm`                      | shipping   | 1    |
| libkrun          | `Hypervisor.framework` (C library)    | shipping   | 2    |
| Apple Container  | Apple **Containerization** framework  | **stub**, macOS 26+ Apple Silicon only | 3 |
| Docker           | OCI runtime                           | shipping fallback | 3 |

Neither shipping macOS backend uses Vz directly. Apple Container *does*, but
only macOS 26+ on Apple Silicon, and through the higher-level Containerization
framework. That leaves a real gap: **macOS 11–25 hosts have only the nested
libkrun→Firecracker path**, even though Vz is right there.

Adding a `vz` backend closes that gap and lets us run *both* the builder VM
and workload microVMs on Vz directly — collapsing the nested-VM pipeline on
macOS into a single layer for the hosts that want it.

Explicit user constraints driving this plan:

- A Vz backend usable for **both** the builder VM and workload microVMs.
- libkrun left in place — Vz is **additive**, not a replacement on macOS.
- **Firecracker stays the Linux default**, including for production deploys.
  This plan does not touch the Linux path; it only adds a macOS option.
- Balloon and (where available) snapshotting included from the start.
- Vz on Linux is **not wanted** — the existing Firecracker-direct path on
  Linux is better in every dimension that matters.

## Why it works (the virtio observation)

Every host↔guest channel we rely on maps 1:1 onto Vz's Swift classes:

| Our use                                | Vz class                                        |
|----------------------------------------|-------------------------------------------------|
| rootfs at `/dev/vda`, overlay `/dev/vdc`, verity sidecar `/dev/vdd` | `VZVirtioBlockDeviceConfiguration` |
| guest agent vsock (CID 3, port 5252)   | `VZVirtioSocketDeviceConfiguration` + `VZVirtioSocketConnection` |
| `console=hvc0`                         | `VZVirtioConsoleDeviceSerialPortConfiguration` |
| host-side `passt`/`gvproxy` socket     | `VZVirtioNetworkDeviceConfiguration` + `VZFileHandleNetworkDeviceAttachment` |
| entropy                                | `VZVirtioEntropyDeviceConfiguration` |
| balloon                                | `VZVirtioTraditionalMemoryBalloonDeviceConfiguration` |

Direct-kernel boot via `VZLinuxBootLoader(kernelURL:initialRamdiskURL:commandLine:)`
takes the same `(vmlinuz, initrd?, cmdline)` shape Firecracker takes, so the
artifacts the builder VM produces today can boot under Vz with minor cmdline
adjustments (no `i8042` quirks needed, console name changes from `ttyS0` to
`hvc0` — which we already use, see `DEFAULT_CMDLINE`).

`gvproxy` already terminates the host end of our virtio-net path on macOS
(ADR-055), and Vz's `VZFileHandleNetworkDeviceAttachment` is exactly the
"hand me a unix datagram socket and I'll bridge it" interface gvproxy expects.
The pieces line up.

## What Vz can and can't do

Can do, used in this design:

- Boot uncompressed Linux kernel + cmdline + optional initrd
  (`VZLinuxBootLoader`)
- Multiple virtio-blk devices (rootfs RO, overlay RW, dm-verity sidecar)
- virtio-vsock CID 3 with arbitrary listen/connect ports
- File-handle network attachments (gvproxy bridge)
  (`VZVirtioNetworkDeviceConfiguration` + `VZFileHandleNetworkDeviceAttachment`)
- virtio-console on serial port for captured logs
- **Memory balloon** via `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`
  — exposes `targetVirtualMachineMemorySize`, mapping cleanly onto
  `VmBackend::balloon_target_mib()`. macOS 11+; on from day one.
- **Snapshot / save-restore** via
  `VZVirtualMachine.saveMachineStateTo(url:completionHandler:)` and
  `restoreMachineStateFrom(url:completionHandler:)`. **macOS 14+ only.**
  File format is opaque (Apple-controlled). Phase E gates on macOS 14+;
  `VmCapabilities { snapshots: true, .. }` only when detected.

Can't do, and we don't need:

- Nested virtualization (would only matter if we still wanted to run
  Firecracker inside the Vz guest — we don't; that's the whole point)
- PCI passthrough
- Live migration across hosts

Constraints to plan around:

- The supervisor binary must carry `com.apple.security.virtualization`
  entitlement and be code-signed (parallel to libkrun's
  `com.apple.security.hypervisor` requirement).
- Vz is a **Swift framework**; we bridge to Rust via a separate
  supervisor subprocess, same pattern as `mvm-libkrun-supervisor`
  (`crates/mvm-backend/src/libkrun.rs:18-24`).

### Volumes and host-path mounts

Both block-device volumes and host-path shares are supported, but they
go through different Vz classes with different security implications.

**Block volumes** — `VZVirtioBlockDeviceConfiguration` with one of:

- `VZDiskImageStorageDeviceAttachment(url:readOnly:)` — backed by a disk
  image file on the host. macOS 11+. Covers our rootfs RO, overlay RW,
  and verity sidecar slots (`/dev/vda`, `/dev/vdc`, `/dev/vdd`) one-for-one.
- `VZDiskBlockDeviceStorageDeviceAttachment(fileHandle:readOnly:)` —
  backed directly by a host block device handle. macOS 13+.

The **app-deps sealed volume** (`~/.mvm/volumes/deps/<volume_hash>/content/`
plus sidecars):

- *Preferred:* pack `content/` into an immutable ext4 image at seal time
  and mount it RO as another virtio-blk. Matches the dm-verity model and
  ADR-002 claim 9's "hash-locked, attestation-checked" contract.
- *Alternative:* expose `content/` as a virtio-fs share. Simpler but
  expands the share-audit surface; default is the block-image approach.

The decision is **not Vz-specific** — whatever libkrun does, Vz does.

**Host-path mounts (virtio-fs shares)** —
`VZVirtioFileSystemDeviceConfiguration` + `VZSharedDirectory` /
`VZMultipleDirectoryShare`:

- Builder VM: one explicit share for the Nix store output extraction
  point, same contract libkrun uses today.
- Workload microVMs: **no virtio-fs shares by default.** The supervisor
  JSON config refuses to attach `VZVirtioFileSystemDeviceConfiguration`
  unless the admitted ExecutionPlan names that share. Fail-closed test in
  Phase B verification.

### Guest communication is still vsock — nothing changes

`VZVirtioSocketDeviceConfiguration` is the same virtio-vsock device the
guest kernel already drives. From the guest's perspective there is no
difference: `/dev/vsock`, CID 3 (we keep `GUEST_CID = 3` from
`crates/mvm-guest/src/vsock.rs:14`), and the same port allocation:

- Port 5252: control protocol (JSON + Ed25519 signing) — unchanged
- Ports 10000+: TCP port forwarding — unchanged
- Ports 20000+: interactive console PTY sessions — unchanged

Host-side, Vz exposes:

- `VZVirtioSocketDevice.setSocketListener(_:forPort:)` — supervisor
  listens; guest dials in
- `VZVirtioSocketDevice.connect(toPort:completionHandler:)` — host dials
  a port the guest listens on

The Vz supervisor forwards both onto unix sockets under
`~/.mvm/run/<vm_id>/vsock/` with mode 0700. `mvmctl` doesn't know which
hypervisor sits on the other end of those sockets — **no guest agent
code, no protocol code, no key handling code changes** when switching
backends.

### Networking detail

The macOS host path today (per ADR-055) is:

```
guest virtio-net  ↔  unix-datagram socket  ↔  gvproxy  ↔  host NAT  ↔  internet
```

Vz fits in without changing the plumbing. The supervisor:

1. Opens a `socketpair(AF_UNIX, SOCK_DGRAM)` (or talks to the gvproxy
   control socket already running for the host).
2. Hands one end to gvproxy via the existing dispatch in
   `crates/mvm-backend/src/libkrun.rs` (`MVM_NETWORKING` selection
   stays as-is).
3. Wraps the other end in `VZFileHandleNetworkDeviceAttachment(fileHandle:)`
   and attaches it to a `VZVirtioNetworkDeviceConfiguration`.

No new virtio-net frame parser inside the Vz supervisor; parsing stays
in gvproxy (Go) where ADR-055's threat model already accounts for it.
Outbound-only builder VMs reuse the same gvproxy path; the JSON config
just toggles `network.policy = "nat-outbound"`.

## Security considerations

Items that must be settled before the backend launches a *production
workload* VM. Pre-prod / dev-shell launches are held to a lower bar per
the `feedback_dev_vm_vs_prod_security_tiers` memory.

1. **ADR-002 claim coverage** (full per-claim audit below in
   "Can we still make all nine ADR-002 security claims?").

2. **New trust surface: the Vz supervisor binary.** Runs with
   `com.apple.security.virtualization` entitlement, ad-hoc / Dev ID
   code-signed. Treat like `mvm-libkrun-supervisor`: mode 0700 on IPC
   socket (W1.2), one supervisor per VM (per `reference_libkrun_gotchas`),
   binary under `~/.mvm/bin/` not on `$PATH`.

3. **Closed-source framework.** Vz is closed-source — same posture as
   libkrun-on-`Hypervisor.framework` and Apple Container-on-Containerization.
   Doesn't move the ADR-002 "host is trusted" boundary.

4. **Snapshot file integrity (Phase E).** Snapshots live under mode-0700
   directories; SHA-256 pinned in the audit chain via `vm.snapshot_saved`
   / `vm.snapshot_restored` events. `VzBackend::restore` rejects on
   mismatch.

5. **Security tier.** Proposed initial tier: **Tier 2** (matches
   libkrun, same `Hypervisor.framework` primitive). Tier 3 considered
   and rejected. ADR-056 captures the reasoning.

6. **Dev vs prod tier.** Dev builder VM does not require dm-verity or
   claim-1/2/3 enforcement (per `feedback_dev_vm_vs_prod_security_tiers`).
   Workload `VzBackend::start_with_mode(Workload)` held to prod-tier.

7. **Kernel command-line lockdown.** Supervisor refuses unrecognized
   cmdline tokens; only ExecutionPlan-allowed tokens admitted. Without
   this, `init=/bin/sh` or `mvm.verity_disable=1` could neuter claim 3.
   Fail-closed test in Phase B.

8. **Resource-cap enforcement parity.** Admitted plan caps vCPU /
   memory / per-disk size; supervisor refuses over-allocation.

9. **Console mode lockdown.** `VZVirtioConsoleDevice` is **capture-only**
   on workload microVMs. Interactive console (PTY-over-vsock) is dev
   mode only, routed via `crates/mvm-guest/src/console.rs` on vsock
   ports 20000+ — not via virtio-console serial port. Supervisor
   config separates "console log path" from "PTY console."

10. **VM identifier handling.** `VZGenericMachineIdentifier` generated
    fresh per launch (ephemeral). For Phase E snapshots, identifier
    persists with the snapshot and is verified on restore so snapshots
    can't be "swapped" between unrelated workloads.

11. **Supervisor binary is a security boundary.** Memory disclosure
    leaks guest memory. Mitigations: Swift bounds-checked types,
    hardened runtime, library validation, no JIT entitlement,
    restricted entitlement set, no plugin loading.

12. **Crash diagnostics.** Capture
    `~/Library/Logs/DiagnosticReports/mvm-vz-supervisor-*.crash` into
    `mvmctl logs <vm_id> --hypervisor`.

13. **Enterprise / MDM policy.** `mvmctl doctor` detects MDM-disabled
    virtualization (via `VZVirtualMachineConfiguration.validate()`
    error class) and reports clearly.

## Implementation phases

Tracer-bullet ordering: get a single Linux VM booting end-to-end first,
then layer on builder-mode, workload-mode, and security parity. Each
phase ends in something the user can run.

### Phase A — `mvm-vz-supervisor` Swift binary (smallest viable end-to-end)

New crate / Xcode target at `crates/mvm-vz-supervisor/` containing a Swift
package that builds a single binary:

- Reads a `SupervisorConfig` JSON on stdin (mirrors
  `mvm-libkrun-supervisor`'s shape — `crates/mvm-libkrun/` is the reference)
- Constructs a `VZVirtualMachineConfiguration` with:
  - `VZLinuxBootLoader` from `kernel_path` + `cmdline` + optional `initrd_path`
  - One `VZVirtioBlockDeviceConfiguration` per disk
  - One `VZVirtioSocketDeviceConfiguration`
  - One `VZVirtioConsoleDeviceConfiguration` writing to a file we pass in
  - One `VZVirtioNetworkDeviceConfiguration` if `network.socket_path` set
- Starts the VM, writes its PID to `pid_path`, blocks until exit
- Forwards SIGTERM → `VZVirtualMachine.stop(completionHandler:)`
- Returns the exit code from the guest as its own exit code

**Acceptance:** boot a known-good kernel+rootfs (the dev-shell image)
under Vz and `vsock-connect 3:5252` succeeds (`mvmctl console`).

### Phase B — `VzBackend` in `crates/mvm-backend/src/vz.rs`

New module parallel to `libkrun.rs`. Implements `VmBackend`
(`crates/mvm-core/src/protocol/vm_backend.rs:520`) by spawning the Phase
A binary, writing config JSON to stdin, managing PID + lifecycle the
same way `LibkrunBackend` does (`crates/mvm-backend/src/libkrun.rs:64`).

Capabilities (runtime-feature-detected):

```rust
VmCapabilities {
    vsock: true,
    snapshots: macos_at_least(14, 0),
    balloon: true,
    tap_networking: false,
}
```

Wire into `BackendKind` enum and `auto_select()`
(`crates/mvm-backend/src/backend.rs:372-403`). **`auto_select()` stays
unchanged.** Vz only ranks above libkrun when `MVM_BACKEND=vz` (env)
or `--backend vz` (flag) opts in. Linux hosts never see Vz in their
selection chain. `crates/mvm-core/src/platform/platform.rs:30` gets
`has_vz()` (returns `false` on Linux).

**Acceptance:** `MVM_BACKEND=vz mvmctl run dev-shell` boots a workload
microVM on macOS **without** going through libkrun first.

### Phase C — Vz as a builder-VM backend

Builder VM lifecycle today calls in-process `start_enter()` in
`crates/mvm-libkrun/src/lib.rs` (libkrun is a C library linked into
`mvmctl`). For Vz, use the supervisor-subprocess model from Phase A/B:
`StartMode::BlockingWithIO` (or equivalent) routes through
`VmBackend::start_with_mode()`, captures stdin/stdout/stderr from the
guest's virtio-console, returns the guest's exit code.

Builder runtime selection in `crates/mvm/src/vm/` gets a parallel branch:
when `MVM_BUILDER_BACKEND=vz`, the builder VM is constructed via
`VzBackend::start_with_mode(BlockingWithIO)` instead of
`LibkrunBuilderVm::run_build`.

**Acceptance:** `MVM_BUILDER_BACKEND=vz mvmctl build --flake .` produces
a byte-identical rootfs to the libkrun-hosted equivalent.

### Phase D — ADR-056 & security-tier landing (no default reshuffle)

Vz stays opt-in. `auto_select()` is **unchanged**. libkrun remains the
macOS default; Firecracker remains the Linux default and the production
deploy default.

`specs/adrs/056-vz-backend.md` covers:

- Why Vz given libkrun + Apple Container already exist (Vz fills the
  macOS 11–25 / Intel coverage gap *and* unlocks direct workload microVM
  hosting without nested Firecracker).
- Security tier (**Tier 2** proposed; Tier 3 considered and rejected
  because Vz sits on the same `Hypervisor.framework` primitive as libkrun).
- Relationship to ADR-013 (adds, doesn't retract).
- Relationship to ADR-055 (gvproxy networking unchanged).
- ADR-002 update: add Vz row to backend table; mark claim coverage.

### Phase E — Snapshot / save-restore (macOS 14+)

- Extend supervisor JSON config with `snapshot.save_path` and
  `snapshot.restore_path` modes
- Swift: call `saveMachineStateTo` on stop when `snapshot.save_path` is
  set; `restoreMachineStateFrom` on start when `snapshot.restore_path`
  is set
- Rust: implement `VmBackend::pause` / `resume` / snapshot verbs;
  expose via `mvmctl snapshot <id> save <path>` / `restore <path>`
- Hash-pin snapshot file in the audit chain before restore (Security §4)
- `VmCapabilities::snapshots = true` for Vz on macOS 14+; ADR-002
  claim 1 / 3 verification re-run against restored VM in CI

**Acceptance:** `mvmctl snapshot save / restore` round-trips a dev-shell
workload VM, restored VM has same PID 1 state and preserves vsock
agent sessions.

## Critical files

Modify:

- `crates/mvm-backend/src/backend.rs` — add `BackendKind::Vz`, slot into
  `auto_select()`
- `crates/mvm-core/src/platform/platform.rs` — `has_vz()` detector
- `crates/mvm-core/src/protocol/vm_backend.rs` — possibly extend
  `StartMode` for blocking-with-IO builder-VM mode if not already there

Add:

- `crates/mvm-vz-supervisor/` — Swift package, `mvm-vz-supervisor` binary
- `crates/mvm-backend/src/vz.rs` — `VzBackend` impl of `VmBackend`
- `crates/mvm-vz/` (optional) — thin Rust crate for supervisor-binary
  path resolution + JSON config types, parallel to `crates/mvm-libkrun/`
- `specs/adrs/056-vz-backend.md`

Reuse, don't duplicate:

- `SupervisorConfig` JSON shape from `crates/mvm-libkrun/` — reuse where
  overlapping; vz-specific fields go in a `vz: Option<...>` block
- `auto_select()` ordering machinery
- `GUEST_CID = 3` + agent port allocation
  (`crates/mvm-guest/src/vsock.rs:14`)

## Verification

End-to-end, on a macOS 13+ host (covers both Intel Hypervisor.framework
and Apple Silicon):

1. **Phase A:** `mvm-vz-supervisor < example-config.json` boots the
   dev-shell-image VM, prints boot logs to the configured console file,
   host-side `vsock-connect 3:5252` succeeds.

2. **Phase B:** `MVM_BACKEND=vz mvmctl run dev-shell` and
   `mvmctl console <id>` give an interactive shell. `mvmctl status <id>`
   reports Vz. `time mvmctl run ...` on Vz vs. nested
   libkrun→Firecracker; expect ≥30% wall-time win on cold-boot.

3. **Phase C:** `MVM_BUILDER_BACKEND=vz mvmctl build --flake . --profile
   minimal --role worker` produces rootfs hash identical to the
   libkrun-built rootfs hash for the same flake input.

4. **Regression net:** `cargo test --workspace` + `cargo clippy
   --workspace -- -D warnings` pass with both backends compiled in. CI
   adds a macOS-only `vz-smoke` job that runs the Phase A acceptance.

5. **Security-claim parity:** `mvmctl doctor` against a Vz-backed
   workload microVM reports claims 1, 2, 3 green.

## Platform coverage summary

| Host          | Builder VM backends                | Workload microVM backends         | Production deploy default |
|---------------|------------------------------------|-----------------------------------|---------------------------|
| Linux + KVM   | (unchanged) libkrun / Firecracker  | (unchanged) **Firecracker**       | **Firecracker** (unchanged) |
| macOS 11–12   | libkrun, Vz (NEW)                  | libkrun (Firecracker nested), Vz (NEW) | n/a (dev only)       |
| macOS 13–25   | libkrun, Vz (NEW, full virtio)     | libkrun (nested), Vz (NEW)        | n/a (dev only)            |
| macOS 14+     | libkrun, Vz (NEW, **+ snapshots**) | libkrun (nested), Vz (NEW, **+ snapshots**) | n/a (dev only)   |
| macOS 26+ ASi | libkrun, Vz, Apple Container       | libkrun (nested), Vz, Apple Container | n/a (dev only)        |

`auto_select()` defaults are unchanged. Vz is opt-in only and never
appears on Linux hosts.

## Can we still make all nine ADR-002 security claims?

| Claim | Status under Vz                                                          |
|------:|--------------------------------------------------------------------------|
| 1     | **Inherits** — supervisor refuses non-admitted virtio-fs shares          |
| 2     | **Inherits** — guest-side, hypervisor-independent                        |
| 3     | **Inherits** — dm-verity is kernel-side; `VZLinuxBootLoader` carries cmdline + roothash unchanged |
| 4     | **Inherits** — guest-side                                                |
| 5     | **NEW WORK** — Swift `JSONDecoder` strict struct + Rust-driven fuzz corpus equivalence test |
| 6     | **Inherits** — host-side download path                                   |
| 7     | **EXTENDS** — Swift binary reproducibly built, SPM `Package.resolved` pinned, no prebuilt download on contributor path |
| 8     | **NEW WORK** — `VzBackend::start_with_mode` through `admit_for_run`; fail-closed bypass test |
| 9     | **Inherits** — `verify_sealed_volume` is hypervisor-agnostic             |

Claims 5 and 8 are the "new code, new tests" items. Claim 7 extends an
existing pipeline. Others come free with the backend abstraction.

## Additional considerations

### Build, distribution, versioning

- Swift toolchain on macOS CI lanes
- Reproducible builds; SPM `Package.resolved` pinned; W5.3 double-build
  parallel for Swift
- Code signing: ad-hoc for dev, Developer ID + notarization for release
- Versioning: `~/.mvm/bin/mvm-vz-supervisor-<mvmctl_version>` lockstep
- Source-checkout determinism — no prebuilt download

### Minimum macOS version

- **macOS 13 (Ventura)** as the floor — full virtio surface
- macOS 11–12 hosts fall back to libkrun (status quo, no regression)
- macOS 14+ unlocks snapshots (Phase E)
- macOS 26+ ASi gets Apple Container parallel

### Multi-architecture coverage

Vz works on both arm64 and x86_64. The existing artifact pipeline
already produces per-arch kernels and rootfs images (ADR-046); Vz
inherits multi-arch without new artifact work. CI smoke runs on both
arches.

### Inactive device classes (smaller attack surface)

The Vz supervisor explicitly **does not** configure:

- `VZVirtioSoundDeviceConfiguration`
- `VZUSBKeyboardConfiguration` / `VZUSBScreenCoordinatePointingDeviceConfiguration`
- `VZGraphicsDeviceConfiguration`
- `VZUSBControllerConfiguration`
- `VZGenericMachineIdentifier` mutability beyond what snapshot needs

### `mvmctl doctor` and `mvmctl init` integration

- `doctor` gains a Vz availability check (entitlement, macOS version,
  ADR-002 claim status, MDM policy)
- `init` wizard on macOS offers Vz as a backend choice; default stays
  libkrun

### Builder VM Stage 0 contract

The Vz builder-VM mode (Phase C) participates in the existing Stage 0
audit + cache-prune contract (`project_stage0_audit_and_cache_prune_contract`):

- `stage0_*` events to the shared audit log
- Pre-`Stage0Boot` failures are not audited
- `mvmctl cache prune` respects the Stage 0 lock

### Host sleep / wake

Vz pauses VMs when the host sleeps (Apple behavior, can't override).
On wake, supervisor inherits libkrun's current auto-resume / paused
status behavior — consistency over novelty.

### Performance baseline (numbers, not vibes)

Phase B verification commits to a measured comparison:

- Cold boot wall time (kernel → guest-agent ready): Vz vs.
  libkrun-direct vs. nested-libkrun-then-Firecracker
- Memory footprint at idle
- Build wall time for a fixed Nix derivation through a builder VM

ADR-056 carries actual numbers from a CI lane, not estimates.

### mvmd integration (separate work)

mvmd selects backends per pool. Adding `vz` to mvmd's backend enum is
a follow-up in that repo, **explicitly out of scope here**.

### Plan / ADR numbering

Per `project_spec_numbering_chaos`: this plan claimed **97**, ADR
claims **056**. Plan 96 was already in flight (PR #420 referenced it
as "Plan 96 dev-up followups") when this plan was filed
(2026-05-22), so this plan stepped to the next free slot.

### Concurrent VM limits and capacity planning

`Hypervisor.framework` caps concurrent VMs (~16 older Intel,
~32 Apple Silicon; varies). Phase B adds:

- Capability probe at startup
- `auto_select`-time warning when host near the ceiling
- Clear error class for "concurrent VM limit reached"

### Boot loader locked to `VZLinuxBootLoader`

Vz also offers `VZEFIBootLoader`; we never use it. Faster boot, less
attack surface. No EFI field in the supervisor config schema.

### Disk image format

Workload microVM disks are **raw ext4 image files** with sparse
allocation. Vz honors guest-issued `DISCARD` / `TRIM` ops via
virtio-blk discard — overlay disks stay thin. No qcow2.

### CI environments

GitHub Actions macOS runners support Vz. Self-hosted Apple Silicon
runners need `com.apple.developer.security.virtualization` provisioning;
flag in contributor docs.

### Notarization & Gatekeeper

Distribution-signed releases of `mvm-vz-supervisor` go through Apple
notarization (`xcrun notarytool`). Dev / source-checkout builds use
ad-hoc signing.

### macOS minor-version compatibility matrix

CI matrix runs Phase B's smoke test against minimum (13.x), current
latest, and one macOS-26+ build. Catches Apple mid-version regressions
before users do.

### CPU scheduling and resource control

macOS has no cgroups. Vz exposes only `cpuCount` and `memorySize`. We
accept Apple's scheduler — same as libkrun today.

### Memory balloon floor

`balloon_target_mib` refuses to shrink below
`min(plan.memory_floor, 128 MiB)`. Without the floor, an aggressive
control loop could OOM the guest.

### License & repository conventions

The Swift package carries dual Apache-2.0 + MIT, matching the Rust
workspace.

### Implementation hygiene

- Work in git worktrees (`feedback_always_use_git_worktrees`)
- No prebuilt download on contributor source-checkout path
- No external build-cache providers (`feedback_no_external_cache_providers`)
- Vz-related host tools route through the builder VM where the
  builder-tools rule applies (`feedback_builder_tools_on_host`)
- The `mvm-vz-supervisor` Swift package is the **only** Swift code we
  own; resist scope creep into broader Swift surface

## Out of scope

- Replacing libkrun as the macOS default
- Touching the Linux Firecracker path
- Removing the nested Firecracker-in-libkrun path on macOS
- Vz on Linux
- Live VM migration across hosts

## Future work (cataloged, not in this plan)

### Windows host support

Separate deferred initiative analogous to this Vz work but for the
Windows hypervisor surface:

- **Primitive:** Windows Hypervisor Platform (WHP)
- **Shape:** parallel crate `crates/mvm-whp-supervisor/` mirroring
  the Vz / libkrun supervisor pattern
- **Open questions:** Linux-on-WHP boot loader (cloud-hypervisor /
  QEMU adopt), virtio device exposure on WHP (`WinHvPlatform` is bare;
  userspace virtio layer needed), signing posture (Authenticode,
  code-integrity)
- **Magnitude:** comparable to this Vz plan plus userspace virtio
- **Tracking issue:** [#428](https://github.com/tinylabscom/mvm/issues/428) — references this plan + ADR-056; gated on Phases A–C merging

## Implementation log

Each session that touches this plan appends an entry below.

- 2026-05-22 — Plan filed. ADR-056 reserved. Worktree
  `worktree-vz-backend-phase-a` created off `origin/main` for Phase A
  work. SPRINT.md Sprint 55 section added.
- 2026-05-22 — `mvmctl doctor` now reports Vz availability +
  supervisor-binary presence (env / source-checkout / installed
  paths). Two unit tests + live smoke against a macOS 26 / arm64
  contributor host. Entitlement and MDM-policy sub-probes remain
  follow-ups.
- 2026-05-22 — `crates/mvm-vz/build.rs` auto-builds the Swift
  supervisor during `cargo build` on macOS by invoking
  `crates/mvm-vz-supervisor/tools/build.sh`. No-op on non-macOS
  hosts and when Swift is unavailable; the warning path keeps
  Linux contributors unblocked. End-to-end:
  `cargo clean -p mvm-vz && cargo build -p mvm-vz` produces the
  ad-hoc-signed supervisor at the source-checkout path the
  resolver consults first. `MVM_VZ_SKIP_SUPERVISOR_BUILD` opts out.
- 2026-05-22 — VzBackend lifecycle wired end-to-end: real
  `start`/`stop`/`status`/`list`/`logs`/`install` in
  `crates/mvm-backend/src/vz.rs`, mirroring `LibkrunBackend`'s
  PID-file lifecycle. `start` resolves the supervisor binary via
  `MVM_VZ_SUPERVISOR_PATH` → adjacent-to-exe →
  `crates/mvm-vz-supervisor/.build/<arch>/debug/` (source checkout)
  → `~/.mvm/bin/mvm-vz-supervisor-<version>` (release-installed),
  builds the `mvm_vz::SupervisorConfig` from `VmStartConfig`,
  spawns the supervisor with JSON on stdin, waits up to 5 s for
  the PID file. `stop` reads the PID, sends `SIGTERM`, escalates
  to `SIGKILL` after 2 s. `pause`/`resume` bail with capability-
  honest messages (supervisor exposes only stdin-driven start/stop
  today — pause/resume + balloon adjustment + snapshots need a
  control socket, follow-up). Eleven VzBackend tests green;
  workspace clippy clean. Replaces the earlier stub-bail
  implementations under the same NOT_YET_WIRED sentinel.
- 2026-05-22 — Phase B trait wiring landed:
  `Platform::has_vz()` in `crates/mvm-core/src/platform/platform.rs`
  (macOS-only, ≥13.0); `crates/mvm-backend/src/vz.rs` with `VzBackend`
  implementing `VmBackend` (skeleton: name/capabilities/security
  profile/install/guest_channel_info real, lifecycle methods bail with
  NOT_YET_WIRED constant pending supervisor-spawn slice); `BackendKind::Vz`
  added to `AnyBackend` enum + `inner()` dispatch + `from_hypervisor`
  (aliases `vz` / `virtualization`) + `tier()`. `auto_select()`
  **unchanged** per user constraint. Six new VzBackend unit tests + one
  AnyBackend dispatch test (`test_any_backend_from_hypervisor_vz`) green;
  `cargo test -p mvm-backend --lib` 148/148; workspace clippy clean.
  Remaining Phase B: supervisor-spawn `start`, resource-cap parity,
  cmdline allow-list, `admit_for_run` integration, console mode
  lockdown, HVF concurrent-VM cap probe, doctor wiring.
- 2026-05-22 — Phase B foundation landed: `crates/mvm-vz/` Rust
  crate with `SupervisorConfig` (+ nested) types whose JSON shape
  matches the Swift `Config.swift` schema byte-for-byte;
  `#[serde(deny_unknown_fields)]` on every struct mirrors the Swift
  `StrictKeys` contract. Also includes `MacAddress::parse` with
  locally-administered bit enforcement, and
  `supervisor_binary_path` / `source_tree_binary_path` for the
  release vs. source-checkout resolution split. Seven unit tests
  green; `cargo check --workspace` clean; `clippy -- -D warnings`
  clean. This is the "Add: crates/mvm-vz/ (optional)" entry from
  Plan 97 §"Critical files"; the actual `VzBackend` impl that
  consumes it is the next slice.
- 2026-05-22 — Phase A first slice landed: `crates/mvm-vz-supervisor/`
  Swift package builds clean with macOS 13 deployment target. All five
  source files in place (`main.swift`, `Config.swift`, `Supervisor.swift`,
  `VsockProxy.swift`, `Network.swift`); strict deny-unknown-fields
  decoder smoke-tested (rejects unknown field with exit 2, empty stdin
  with documented message); ad-hoc codesigning helper `tools/build.sh`
  injects `com.apple.security.virtualization` from `Entitlements.plist`
  and `codesign -d --entitlements -` confirms it's on the binary.
  Remaining Phase A: end-to-end boot acceptance (gated on Phase B's
  Rust JSON producer) and the Rust-side fuzz corpus (gated on the
  Phase B `mvm-vz` crate).
