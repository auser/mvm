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

> Status update 2026-05-13: **W3.1, W3.2, W3.4 (cmdline + console) shipped in PR #154.** W3.3 vsock plumbing decision shipped in the follow-on PR; the host-side listener + full guest-agent ping still wait on W4's process-lifecycle work. See "Findings" + "W3.3 follow-up" below.

Goal: prove a real Nix-built kernel + ext4 rootfs boots in libkrun on macOS Apple Silicon and the guest agent comes up on vsock.

- **W3.1** Pick the validation flake. `examples/minimal` is the canonical "smallest-thing-that-boots" target across mvm. Build it: `mvmctl build --flake examples/minimal`. Outputs are `vmlinux`, `rootfs.ext4`, and possibly `initrd`.
- **W3.2** Construct a `KrunContext` pointing at those artifacts and invoke `mvm_libkrun::start()` directly (bypass the `LibkrunBackend` for the spike — the backend's job is to plumb config, not validate boot). Run from a unit test or a small `examples/libkrun-smoke.rs` binary.
- **W3.3** Confirm the guest agent's vsock listener responds. Use `mvm_guest::vsock::GUEST_AGENT_PORT` and the existing health-check protocol that Firecracker / Apple Container already use.
- **W3.4** **Risks to validate explicitly**:
  - **Kernel cmdline.** Firecracker boots with `console=ttyS0`; libkrun expects `console=hvc0` (virtio-console). Likely fix: pass a libkrun-specific cmdline via `KrunContext.kernel_cmdline`. The Nix build may need a `mkGuest` `cmdlineFor = "libkrun" | "firecracker"` parameter, or just override at runtime via the existing cmdline plumbing in `VmStartConfig`.
  - **Console device.** No serial → console output may not appear. Confirm libkrun routes hvc0 to stdout in the calling process, or wire it through a host-side log file.
  - **vsock device naming.** Firecracker uses `/dev/vhost-vsock`; libkrun's macOS path uses an internal abstraction. Confirm the guest sees vsock on cid 3 (standard guest CID) and the agent's listening port matches.
  - **Verified boot (claim 3) is out of scope for the spike.** The Nix flake's `verifiedBoot = false` exemption (the dev VM path) covers this. Plan 25 §W3 follow-up promotes claim 3 from "DoesNotHold" to "Holds" for libkrun once dm-verity is wired through.

#### Findings — first end-to-end boot, 2026-05-13

Run on: macOS 15.6 (Apple Silicon, M-series), libkrun 1.17.4 via Homebrew, kernel + rootfs from the pre-Plan-72 `~/.mvm/dev/current/` dev-VM artifacts (built 2026-05-12).

**Boot path shipped**: `cargo run --example libkrun-smoke -p mvm-libkrun --features libkrun-sys`. Source: `crates/mvm-libkrun/examples/libkrun-smoke.rs`.

What worked first-try on the W2 codesigned binary:

1. **`console=hvc0 root=/dev/vda rw init=/init` is correct.** The plan's prediction held: libkrun's Hypervisor.framework path drops the Firecracker `console=ttyS0` form and uses virtio-console (`hvc0`) — the same cmdline Apple Container already produces in `crates/mvm-providers/src/apple_container/macos.rs`. Without this swap the kernel boots but emits no console output.
2. **ARM64 "Image" kernel format = libkrun `KRUN_KERNEL_FORMAT_RAW`.** Nix's `nixpkgs.linuxPackages` cross-built `vmlinux` is a flat ARM64 boot Image, not ELF. `KernelFormat::Raw` consumes it directly. W1's wrapper had defaulted to `Elf`; the W3 smoke flipped that, and the W1 unit test doesn't exercise the kernel-format value so the change is safe.
3. **Multiple virtio-blk devices work.** Rootfs at `/dev/vda` (736 MiB ext4), `~/.mvm/dev/nix-store.img` at `/dev/vdb` (64 GiB sparse). The kernel boot log enumerates both; `EXT4-fs (vda): mounted filesystem … r/w with ordered data mode` is the success signal.
4. **`krun_set_console_output(path)` routes hvc0 to a host file** — confirmed by checking `/tmp/mvm-libkrun-smoke-console.log` for the full kernel boot log post-shutdown. Useful for both manual smoke tests and the W5 CI lane.
5. **Plan 57 W2's codesigning gate is load-bearing.** The first run without `ensure_signed()` failed at `krun_start_enter` with `Internal(Vm(VmSetup(VmCreate)))` / rc `-22`. After `ensure_signed()` self-signs the binary with both entitlements and re-spawns, boot succeeds. The smoke binary now calls `ensure_signed()` first so subsequent reruns are silent (`MVM_SIGNED=1`).

Risks the plan named that **did not materialize**:

- The Nix `mkGuest` build did not need a `cmdlineFor = "libkrun" | "firecracker"` switch. Overriding `KrunContext.kernel_cmdline` at runtime is sufficient — the same kernel binary boots under Firecracker (`ttyS0`) and libkrun (`hvc0`) depending on what the host passes.
- The codesigning entitlement composed cleanly with the existing VZ entitlement (plan 57 W2 + PR #151 already validated this for the codesign step; W3 confirmed it at runtime).
- No `mkGuest` change was needed for libkrun-specific kernel features.

What's **deferred to a follow-on PR (W3.3)**:

- **`krun_add_vsock` vs `krun_add_vsock_port` are mutually exclusive in libkrun's current API.** Calling `add_vsock(0)` after `add_vsock_port(...)` returns `-EEXIST`; the reverse order has the same result. The W3 smoke ran with only `add_vsock_port` (which is enough for TSI-mode pseudo-vsock) and the guest booted to userspace, but the dev rootfs's guest-agent listener uses true virtio-vsock and needs `add_vsock`. Picking the right mode (real virtio-vsock for the guest agent, TSI for ipc-style port forwards) and the per-VM host socket path is W3.3.
- The dev rootfs `/init` itself hits a BusyBox-vs-util-linux incompatibility (`setpriv: unrecognized option: reuid=990`) — orthogonal to libkrun. Tracked separately under the dev-VM owners; the libkrun layer did its job by getting `/init` running.
- A vsock health-check ping against `mvm_guest::vsock::GUEST_AGENT_PORT` from the host. `krun_start_enter` consumes the calling process (calls `exit()` on success), so the ping has to come from a sibling process or fork. The W4 supervisor-thread / launchd lane resolves the process-lifecycle side of this naturally.

Decision recorded: **the libkrun macOS Apple Silicon path is viable.** Plan 72 (builder-VM-via-libkrun) is unblocked on the boot side; the remaining wiring is state-tracking (W4) and CI (W5).

#### W3.3 follow-up — vsock plumbing decision, 2026-05-13

After reading the libkrun upstream README (Homebrew ships it as `/opt/homebrew/Cellar/libkrun/1.17.4/README.md`) and an empty-listener experiment on this host:

1. **TSI (Transparent Socket Impersonation) is auto-enabled** when no virtio-net device is added. The README is explicit: "TSI for AF_INET and AF_INET6 is automatically enabled when no network interface is added to the VM." We never call `krun_add_net_*`, so TSI is on and the virtio-vsock device is created implicitly. The earlier "`add_vsock` vs `add_vsock_port` are mutually exclusive" finding was the symptom — `add_vsock` is documented as *adding a second independent virtio-vsock device*, which collides with the TSI-provided one. The right call for our use case is `add_vsock_port` alone; `add_vsock` is the wrong API for mvm and stays out of the `configure()` path.
2. **libkrun does not create the host-side unix socket file.** Verified by running the smoke binary with `~/.mvm/vms/<name>/` set as the per-VM dir and inspecting the dir at +10s — empty. The host is responsible for binding a `UnixListener` at the path before `start_enter`; libkrun then proxies traffic from each guest-side `AF_VSOCK port` to a *client* connection at that listener. Apple Container's `start_vsock_proxy_listener` (`crates/mvm-providers/src/apple_container/macos.rs`) is the analogue; the W4 supervisor adopts the same pattern.
3. **Per-VM socket dir is now configurable.** `KrunContext::with_vsock_socket_dir(...)` + the `vsock_socket_path(port) -> PathBuf` helper. Default fallback (used by the smoke binary) is `/tmp/mvm-libkrun-<name>-vsock-<port>.sock`; W4 + Plan 72 consumers always supply `~/.mvm/vms/<name>/` so cross-process clients (e.g. `mvmctl console`) can find the socket without scanning `/tmp`.

Full guest-agent health-check ping is still W4-gated: the smoke binary becomes the guest, so it can't simultaneously be the host that connects to the unix listener. Once W4 lands a sibling supervisor process (or launchd unit), the same wiring + the existing `mvm_guest::vsock` framing closes the loop.

### W4 — State tracking decision (1–2 days)

Goal: decide and implement how mvmctl tracks running libkrun VMs.

libkrun is a library, not a daemon. `krun_start_enter` blocks the calling thread until the guest exits. Two options:

- **Option A — In-process registry.** mvmctl spawns a background thread per VM; the thread holds the libkrun handle and waits on `krun_start_enter`. State lives in a shared `OnceLock<Mutex<HashMap<VmId, KrunHandle>>>`. Closing the mvmctl process kills the VM (libkrun's threads exit when the process dies).
  - Pro: simple. Pro: matches the dev workflow ("`mvmctl run` keeps the VM alive while you work, ^C to stop").
  - Con: doesn't support `mvmctl start` + come-back-later flows.

- **Option B — Daemonize via launchd / systemd.** Same pattern as `mvm-apple-container`'s `install_launchd_direct`. mvmctl writes a launchd plist (or systemd unit on Linux), launchd starts a background `mvmctl libkrun-supervise <vm-name>` process, that process holds the libkrun handle. State persists across mvmctl invocations.
  - Pro: matches Firecracker / Apple Container UX (`mvmctl start` returns immediately).
  - Con: more code, more failure modes.

**Recommendation (revised 2026-05-13 after the W3 spike): ship Option B.** Option A's "background thread holds the libkrun handle" pattern collapses on close: `krun_start_enter` calls `exit()` on the *whole process* when the guest powers off cleanly (libkrun documents this; reproduced in the W3 smoke as the `start_enter` return type becoming `Result<Infallible, _>`). That means stopping any one VM in a multi-VM registry would tear down the mvmctl process and every other libkrun guest it was supervising. Option A *would* work for the single-VM dev shell, but the moment the design grows to plan 72's builder VM + a user-facing `mvmctl up --hypervisor libkrun`, Option A's blast radius is unacceptable. Option B (subprocess per VM) sidesteps the `exit()` problem entirely — each supervisor process owns exactly one libkrun guest, and the parent mvmctl returns immediately after spawning.

- **W4.1** Add a `mvm-libkrun-supervisor` binary in `crates/mvm-libkrun/src/bin/`. It reads a `SupervisorConfig` JSON document from stdin, runs `ensure_signed()` (W2 codesigning gate), creates the per-VM directory under `~/.mvm/vms/<name>/`, writes its own PID to `~/.mvm/vms/<name>/libkrun.pid`, registers each vsock port via `krun_add_vsock_port2(listen=true)` (libkrun then creates the unix socket file as a listener when the guest binds to the matching `AF_VSOCK` port), then calls `mvm_libkrun::start_enter`. Process lifetime equals VM lifetime: `exit()` from libkrun terminates only this supervisor, not its parent.

  *Finding from the W4 spike run (2026-05-13):* the supervisor writes its PID and reaches `start_enter` cleanly. With the dev-VM artifacts the guest boots through the kernel but the rootfs's init crashes at a BusyBox `setpriv: unrecognized option: reuid=990`, so the guest never gets to `AF_VSOCK::bind(GUEST_AGENT_PORT)`. libkrun creates the unix socket file lazily on first guest bind, so the per-VM dir stays empty until a working guest agent runs. The supervisor infrastructure is correct; full vsock proof needs the dev-VM init fix (orthogonal — tracked under the dev-VM owners) or a minimal test rootfs that just opens a vsock listener.
- **W4.2** Wire `LibkrunBackend::start()` to spawn the supervisor via `std::process::Command::new(supervisor_path())`, pipe the config JSON to stdin, and return. `LibkrunBackend::stop()` reads the PID file and signals (SIGTERM, optionally promoted via shutdown_eventfd). `list()` walks `~/.mvm/vms/*/libkrun.pid`. `stop_all()` iterates the same walk.
- **W4.3** Add an integration test that spawns the supervisor on the W3 dev artifacts, connects a `UnixStream` to the bound vsock listener, and waits for libkrun to proxy a single byte. This is the missing W3.3 health-check that the spike couldn't run in-process.

### W5 — CI lanes (0.5–1 day)

Goal: regression coverage on every PR.

- **W5.1** Extend `.github/workflows/ci.yml` with a `libkrun-macos-arm64` job that runs on `macos-latest` (which is Apple Silicon as of late 2025), installs libkrun via Homebrew, runs `cargo test -p mvm-libkrun -p mvm --test libkrun_smoke`. The smoke test boots `examples/minimal` and asserts the guest agent responds.
- **W5.2** Once W3 lands, **the smoke test is the gate** — it'll catch regressions in libkrun's API, the Nix kernel cmdline, our wrapper code, and the codesigning path simultaneously.

#### W5 progress (2026-05-13)

The `libkrun-macos` lane shipped with a `dorny/paths-filter@v3` gate so the macOS runner (~10× per-minute cost vs ubuntu-latest, per ADR-038) only spins up the heavy steps when a PR touches:

- `crates/mvm-libkrun/**`
- `crates/mvm-backend/src/libkrun.rs`
- `crates/mvm-backend/Cargo.toml`
- `crates/mvm-providers/src/apple_container/**` (W2 codesigning gate)
- `specs/plans/57-libkrun-spike.md`
- `.github/workflows/ci.yml`

Or on any push to `main` regardless of path, so post-merge drift is still surfaced.

What the lane validates that the Linux lanes cannot:

1. `bindgen` + `libclang` consume the Homebrew `libkrun.h` cleanly — the `libkrun-sys` feature builds and links against `libkrun.dylib`.
2. The `mvm-libkrun-supervisor` binary compiles (`required-features = ["libkrun-sys"]`).
3. `mvm_libkrun::sys::Context::*` round-trips against the real `libkrun.dylib` — the 8 unit tests in `mvm-libkrun` covering `create_ctx`, `set_log_level`, `set_vm_config`.
4. `LibkrunBackend`'s spawn / PID-file / signal logic in `mvm-backend` builds + its 12 non-FFI tests pass (path resolver branches, status / list with no VMs, etc.).

**Out of scope for W5** (folded into W4.3 instead): end-to-end VM boot. GitHub macOS runners don't expose `Hypervisor.framework` to user-mode processes, so `krun_start_enter` can't actually create a guest there. The lane catches everything *before* that boundary; full boot validation needs a self-hosted runner or a working minimal-rootfs harness, which is W4.3's job.

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
| `crates/mvm-libkrun/src/bin/mvm-libkrun-supervisor.rs` | W4.1 | new — one process per libkrun guest, owns the host-side vsock listeners + PID file |
| `crates/mvm-backend/src/libkrun.rs` | W4.2 | wire `LibkrunBackend::start/stop/list` to spawn / signal / walk the supervisor processes |
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
