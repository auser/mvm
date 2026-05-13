# Plan 57 — libkrun spike (turn scaffolding into a working backend)

> Status: pending (spike — start when bandwidth permits)
> Owner: TBD
> Started: —
> Depends on: plan 53 §"Plan E" (Sprint 48 scaffolding — landed 2026-05-07)
> Tracking breadcrumb: `mvm_libkrun::Error::NotYetWired { tracking: "plan 53 §\"Plan E\" / Sprint 48 spike phase" }`

## Why

Sprint 48 shipped libkrun **scaffolding**: the public Rust API (`KrunContext`, `start`, `stop`, `is_available`), platform detection (`Platform::has_libkrun`), `LibkrunBackend` wired into `AnyBackend` with the `--hypervisor libkrun` flag, the right `auto_select` priority order, a Tier 2 `BackendSecurityProfile`, doctor visibility, and bootstrap install hints. What's missing is the part that actually boots VMs: real C-library bindings, codesigning, and end-to-end kernel + rootfs validation on at least one platform.

This plan is the spike to close that gap. It is **explicitly scoped to one-platform proof of viability** — boot a Nix-built ext4 rootfs on macOS Apple Silicon, confirm vsock + console work, confirm the Nix kernel is compatible. Once that's done, follow-up work expands to Linux + macOS Intel and lands the daemonization / state-tracking story.

When this plan ships, Intel Mac users get a real Tier 2 microVM tier with no Lima dependency, and Linux users get a single-binary alternative to firecracker-on-PATH.

## Prerequisites

A macOS Apple Silicon dev machine with:

- macOS 14+ (Sonoma or later — Hypervisor.framework features libkrun uses are all available there).
- Homebrew installed.
- Xcode command line tools (for codesigning).
- An Apple developer cert for codesigning (the dev binary path uses ad-hoc signing; the spike doesn't need a paid developer account).
- libkrun installed: `brew install libkrun`.

The spike does not require a paid Apple developer account, but the
follow-up "ship a notarized release" work does. That's tracked outside
this plan.

## Workstreams

Each item is independently shippable. Numbering is execution order.

### W1 — Real C bindings (1–2 days)

Goal: replace the hand-written stub API in `mvm-libkrun` with `bindgen`-generated FFI plus a thin safe wrapper.

- **W1.1** Add `bindgen` to `crates/mvm-libkrun/Cargo.toml` as a `[build-dependencies]` entry.
- **W1.2** Write `crates/mvm-libkrun/build.rs`. The build script:
  - Probes for `libkrun.h` at `/opt/homebrew/include/libkrun.h` (Apple Silicon Homebrew), `/usr/local/include/libkrun.h` (Intel/manual), `/usr/include/libkrun.h` (Linux distro).
  - Calls `bindgen` to generate Rust bindings into `OUT_DIR/libkrun_sys.rs`.
  - Emits `cargo:rustc-link-lib=krun` so the linker pulls in the shared library.
  - Falls back to "no-FFI mode" (the current Sprint 48 stub behavior) if `libkrun.h` isn't present, so the workspace still builds on hosts without libkrun installed. Use a feature flag like `libkrun-sys` (default off) or a `cfg(any(libkrun_h_found))` set by the build script.
- **W1.3** Wrap the generated bindings in `crates/mvm-libkrun/src/sys.rs` (private module), exposing safe Rust functions to the existing `lib.rs` API surface. The C calls to wrap are roughly:
  - `krun_create_ctx() -> i32` (negative = error)
  - `krun_set_log_level(level: u32)`
  - `krun_set_vm_config(ctx, num_vcpus: u8, ram_mib: u32) -> i32`
  - `krun_set_root_disk(ctx, path: *const c_char) -> i32`
  - `krun_set_kernel(ctx, path: *const c_char, type_: u32, cmdline: *const c_char) -> i32`
  - `krun_add_vsock_port(ctx, port: u32, filepath: *const c_char) -> i32`
  - `krun_set_workdir(ctx, path: *const c_char) -> i32`
  - `krun_set_env(ctx, envp: *const *const c_char) -> i32`
  - `krun_start_enter(ctx) -> i32` (blocks until guest exits)
  - The exact set varies by libkrun version; pin to the one Homebrew ships on the spike day and document the version in `Cargo.toml`.
- **W1.4** Replace `Error::NotYetWired` returns in `lib.rs::start` and `lib.rs::stop` with calls into the safe wrapper. Keep `Error::NotInstalled` as the front-stop guard. Add a new `Error::Krun(i32)` (already declared) for non-zero return codes from libkrun.

### W2 — macOS codesigning entitlement (0.5–1 day)

Goal: `mvmctl` + libkrun + `Hypervisor.framework` boot a guest without the macOS kernel rejecting the syscall.

- **W2.1** Add `com.apple.security.hypervisor` to the existing entitlements plist (the same one that gates `mvm-apple-container`'s VZ usage). Confirm the dev binary builds and codesigns (ad-hoc) cleanly. Path: probably `crates/mvm-apple-container/macos/Entitlements.plist` or wherever `ensure_signed()` reads from.
- **W2.2** Test the `ensure_signed()` machinery in `crates/mvm-apple-container/src/macos.rs` covers the new libkrun consumer. The hypervisor entitlement is shared between VZ and libkrun, so `mvmctl.app` only needs to be signed once.
- **W2.3** Document the first-run UX in `public/.../guides/troubleshooting.md`: which Gatekeeper / SIP prompts appear, and whether the user has to right-click → Open the first time. (For ad-hoc signed binaries on macOS 14+, the prompt is one click.)

### W3 — End-to-end boot validation (the actual spike, 2–3 days)

Goal: prove a real Nix-built kernel + ext4 rootfs boots in libkrun on macOS Apple Silicon and the guest agent comes up on vsock.

- **W3.1** Author a smallest-thing-that-boots flake at
  `examples/minimal/flake.nix`. The dev-image flake under
  `nix/images/builder/` is unusable here: its closure (rustc + ~480
  cargo crates baked into the guest agent) overflows microsandbox's
  4 GiB overlay during evaluation, so the build never completes.
  The minimal flake instead pairs `pkgs.linuxPackages.kernel` with a
  hand-rolled ext4 staging — `pkgs.runCommand` + `mke2fs -d` over a
  directory containing `pkgs.pkgsStatic.busybox` and a single static
  C binary (`vsock_ok.c`) compiled via `pkgs.pkgsStatic.stdenv`.
  `/init` mounts `/proc /sys /dev`, runs `vsock_ok` (which connects
  vsock `CID_HOST:1234` and writes `"ok\n"`), then powers off via
  `/proc/sysrq-trigger`.
- **W3.2** Author `examples/libkrun-smoke.rs` at the workspace root,
  gated by a new `libkrun-sys` feature on the root package. The
  example handles libkrun's process-ownership semantics
  (`krun_start_enter` `exit()`s the calling process on success) via
  spawn-self: the parent binds a Unix listener at
  `$TMPDIR/mvm-libkrun-smoke.sock`, spawns itself with
  `MVM_LIBKRUN_SMOKE_CHILD=1`, the child calls `mvm_libkrun::boot`,
  and the parent reads `"ok\n"` off the vsock bridge before reaping
  the child.
- **W3.3** Wire `ensure_signed()`: the example calls
  `mvm_providers::apple_container::ensure_signed` before any libkrun
  call so the macOS `com.apple.security.hypervisor` entitlement (PR
  #151) is applied and inherited by the spawned child. We did not
  embed the call inside `mvm_libkrun::start`/`boot` because
  `mvm-libkrun` sits one layer below `mvm-providers` in the
  dependency graph — having libkrun depend on providers would
  invert the layering. The example wiring is the "less invasive"
  option named in the W3 spec.
- **W3.4** `mvm_libkrun::boot(&ctx) -> Result<Infallible, Error>`
  lands as the real-boot entry point alongside the example: it
  configures the context via the existing `sys::Context` wrappers
  then calls `krun_start_enter`. The `Infallible` Ok arm encodes
  the process-suicide semantics — only `Err` is reachable
  (configuration rejected before the VMM loop). `KrunContext`
  grew a `VsockListener { guest_port, host_socket }` shape and a
  `with_kernel_cmdline` builder; `add_vsock_port(port)` rotated
  into `add_vsock_listener(port, host_socket)` because
  `krun_add_vsock_port` requires the caller to name the host-side
  Unix socket (the host process must be `bind`-listening on it
  *before* the guest boots). `mvm-backend`'s `LibkrunBackend`
  updated in lockstep — it generates a per-VM tmpdir socket path
  until W4 lands the `~/.mvm/vms/<name>/` registry.
- **W3.5** Build path: `cargo xtask build-libkrun-smoke-image`
  drives the minimal flake through the existing microsandbox
  builder (`MicrosandboxBuilderVm`) and drops `vmlinux + rootfs.ext4`
  at `examples/minimal/result/` — the same path `nix build`
  creates via symlink, so the smoke example's default discovery
  works for either build path. On a Linux host with Nix installed,
  `(cd examples/minimal && nix build .#default)` is the supported
  alternative. Host Nix on macOS is explicitly NOT required
  (CLAUDE.md invariant); the xtask is the macOS path.
- **W3.6** **Risks to validate explicitly**:
  - **Kernel cmdline.** Firecracker boots with `console=ttyS0`;
    libkrun's implicit console is virtio-console (hvc0). The example
    pins `console=hvc0 root=/dev/vda rw panic=1 loglevel=4` via
    `KrunContext::with_kernel_cmdline`. If the kernel logs end up
    invisible on the host, swap to `krun_set_console_output` (a
    file) or `krun_add_virtio_console_default` (explicit fd) — both
    already wrapped in `sys::Context`.
  - **Console device.** No serial → console output may not appear.
    libkrun's stdout is the implicit virtio-console; confirm it
    routes to the child process's stdout/stderr. If not, wire
    `krun_set_console_output` at `examples/minimal/result/console.log`
    or similar.
  - **vsock device naming.** Firecracker uses `/dev/vhost-vsock`;
    libkrun's macOS path uses an internal abstraction. The host-side
    `krun_add_vsock_port(1234, <socket>)` bridges to the guest's
    `AF_VSOCK` listener on the standard guest CID. The
    `examples/minimal` `vsock_ok.c` targets `VMADDR_CID_HOST` (CID 2),
    which is what libkrun routes the bridge to. If the connect fails
    inside the guest, capture `dmesg` to confirm `vsock` and
    `vmw_vsock_virtio_transport` are present.
  - **Verified boot (claim 3) is out of scope for the spike.** The
    minimal flake does not produce dm-verity sidecars; Plan 25 §W6.4
    promotes claim 3 to "Holds" for libkrun once verity wires
    through `KrunContext`.

### W4 — State tracking decision (1–2 days)

Goal: decide and implement how mvmctl tracks running libkrun VMs.

libkrun is a library, not a daemon. `krun_start_enter` blocks the calling thread until the guest exits. Two options:

- **Option A — In-process registry.** mvmctl spawns a background thread per VM; the thread holds the libkrun handle and waits on `krun_start_enter`. State lives in a shared `OnceLock<Mutex<HashMap<VmId, KrunHandle>>>`. Closing the mvmctl process kills the VM (libkrun's threads exit when the process dies).
  - Pro: simple. Pro: matches the dev workflow ("`mvmctl run` keeps the VM alive while you work, ^C to stop").
  - Con: doesn't support `mvmctl start` + come-back-later flows.

- **Option B — Daemonize via launchd / systemd.** Same pattern as `mvm-apple-container`'s `install_launchd_direct`. mvmctl writes a launchd plist (or systemd unit on Linux), launchd starts a background `mvmctl libkrun-supervise <vm-name>` process, that process holds the libkrun handle. State persists across mvmctl invocations.
  - Pro: matches Firecracker / Apple Container UX (`mvmctl start` returns immediately).
  - Con: more code, more failure modes.

**Recommendation**: ship Option A first (covers the dev workflow), file a follow-up issue for Option B once the spike validates the rest of the stack. Most users in Sprint 48 are dev-mode users; the persistent-VM use case is rarer for libkrun specifically.

- **W4.1** Implement Option A. Files: `crates/mvm/src/vm/libkrun.rs` gains a `LIBKRUN_VMS: OnceLock<Mutex<HashMap<VmId, KrunHandle>>>` static. `start()` spawns the supervisor thread; `stop()` signals it to exit; `list()` returns keys; `status()` checks presence.
- **W4.2** `stop_all()` becomes a real implementation (today it's an empty `Ok(())` placeholder).

### W5 — CI lanes (0.5–1 day)

Goal: regression coverage on every PR.

- **W5.1** Extend `.github/workflows/ci.yml` with a `libkrun-macos-arm64` job that runs on `macos-latest` (which is Apple Silicon as of late 2025), installs libkrun via Homebrew, runs `cargo test -p mvm-libkrun -p mvm --test libkrun_smoke`. The smoke test boots `examples/minimal` and asserts the guest agent responds.
- **W5.2** Once W3 lands, **the smoke test is the gate** — it'll catch regressions in libkrun's API, the Nix kernel cmdline, our wrapper code, and the codesigning path simultaneously.

### W6 — Cross-platform expansion (1 sprint follow-up — out of scope here)

Out of scope for plan 57 itself. Tracked as W6 so the path is named:

- **W6.1** macOS Intel — same steps as Apple Silicon but x86_64 build. The libkrun bindings should "just work" since they're identical at the C level.
- **W6.2** Linux x86_64 — KVM path. Different libkrun internals (no Hypervisor.framework, no entitlements), but the C API is identical. Codesigning concerns evaporate on Linux.
- **W6.3** Linux aarch64 — same as x86_64.
- **W6.4** Promote claim 3 (verified boot) for libkrun to `Holds` once dm-verity is wired through `KrunContext`. Ties into plan 25 §W3.

These each merit their own one-day sub-sprint *after* plan 57 ships.

## Files to create or modify

| File | Workstream | Action |
|---|---|---|
| `crates/mvm-libkrun/Cargo.toml` | W1.1 | add `bindgen` build-dep |
| `crates/mvm-libkrun/build.rs` | W1.2 | new — generate bindings, set link flags |
| `crates/mvm-libkrun/src/sys.rs` | W1.3 | new — safe wrapper over generated bindings |
| `crates/mvm-libkrun/src/lib.rs` | W1.4 | replace `NotYetWired` returns with real calls |
| `crates/mvm-apple-container/macos/Entitlements.plist` (or equivalent) | W2.1 | add `com.apple.security.hypervisor` if not already present |
| `crates/mvm/src/vm/libkrun.rs` | W4.1 | wire `LIBKRUN_VMS` registry, real lifecycle |
| `examples/libkrun-smoke.rs` | W5.1 | new — minimal end-to-end test binary |
| `.github/workflows/ci.yml` | W5.1 | new `libkrun-macos-arm64` job |
| `public/.../guides/troubleshooting.md` | W2.3 | first-run codesigning prompt FAQ |

## Risks

1. **libkrun's macOS Apple Silicon support is younger than its Linux story.** If the spike hits a libkrun upstream bug, file it and either (a) wait, (b) patch via a pinned fork, or (c) descope to Linux-first and revisit Apple Silicon when upstream catches up.
2. **The Nix kernel may need a libkrun-specific cmdline variant.** Plan 25 §W3 already adds a kernel-cmdline knob for verified boot; libkrun would consume the same plumbing. Likely a small addition; flag if it grows.
3. **Codesigning entitlement may not compose with the existing VZ entitlement.** Apple's docs say `com.apple.security.hypervisor` covers both VZ and direct Hypervisor.framework usage, but verify on a real signed build before committing.
4. **The `bindgen` build dep adds a non-trivial build-time cost** (libclang). Mitigate by gating the bindings generation behind a `libkrun-sys` cargo feature so the default workspace build (CI fast lane, hosts without libkrun installed) skips it.
5. **In-process registry (W4 Option A) means mvmctl can't be terminal-detached.** Document this clearly. The follow-up daemonization (W6) addresses it for users who need `mvmctl start` + walk-away.

## Verification

When the spike is done:

- `cargo test -p mvm-libkrun --features libkrun-sys` passes on a host with libkrun installed.
- `cargo test --workspace` continues to pass on hosts *without* libkrun installed (fallback build path).
- `mvmctl run --hypervisor libkrun --flake examples/minimal` boots, the guest agent responds to vsock health checks, and `Ctrl-C` stops the VM cleanly.
- `mvmctl doctor` shows `libkrun: available` on the dev host and reports the active backend as `libkrun` when KVM/Apple Container are both unavailable.
- `mvmctl run --hypervisor libkrun` on a host **without** libkrun installed errors with the exact `install_hint()` message — not a generic "command failed."
- The CI lane (W5) is green on PRs that touch `crates/mvm-libkrun/`, `crates/mvm/src/vm/libkrun.rs`, or any file `examples/libkrun-smoke.rs` depends on.

## Non-goals (named explicitly)

- **Daemonized libkrun VMs across mvmctl invocations.** W4 Option A (in-process) is the spike scope; Option B (launchd / systemd) is W6.
- **macOS Intel + Linux x86_64 + Linux aarch64.** All deferred to W6 sub-sprints.
- **Verified boot (ADR-002 claim 3) for libkrun.** The dev-VM exemption from plan 25 §W3.4 applies; promoting claim 3 is W6.4.
- **Notarization / paid Apple developer cert.** Ad-hoc signing is sufficient for the spike. Notarization is part of the release-engineering follow-up.
- **GPU paravirt** (libkrun's `paravirtualized GPU` feature). Not needed for mvm's workloads; revisit if a real user asks.

## Reversal cost

If the spike fails (e.g., libkrun's macOS path turns out to be too immature), the reversal is cheap:

- **W1 changes**: roll back the bindgen + sys.rs work; the crate falls back to its current Sprint 48 scaffolding shape (already a working state).
- **W2 changes**: remove the `com.apple.security.hypervisor` entitlement addition (no other tier depends on it being present except VZ, which already had it).
- **W3–W5**: pure additions; deletion is the rollback.

The `LibkrunBackend` and `AnyBackend::Libkrun` variant stay in place either way — the scaffolding shipped in Sprint 48 is independently useful as a "documented placeholder" even if the spike never lands.

## References

- Plan 53 §"Plan E" — the design context and fork test that qualified libkrun.
- Plan 25 §W3 — verified boot (claim 3); libkrun support tracked in W6.4 here.
- Sprint 48 commit `ac8c9b2` — scaffolding that this plan upgrades.
- ADR-002 §"Per-backend tier matrix" — Tier 2 expectations for libkrun.
- libkrun upstream: <https://github.com/containers/libkrun>
- `mvm_libkrun::Error::NotYetWired` — the runtime breadcrumb that points readers at this plan.
