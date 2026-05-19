# Plan 88 — macOS gvproxy backend (Plan 87 cross-platform parity)

**Status:** drafted 2026-05-19, awaiting review.
**Amends:** ADR-055 (`specs/adrs/055-passt-virtio-net.md` — original framing assumed passt was cross-platform; Plan 88 corrects).

## Problem

Plan 87 / ADR-055 shipped `passt`-backed virtio-net as the
production-ready libkrun networking backend across PRs #354 + #356 +
#360. The design was tested for correctness on a macOS host but the
**install path** was never exercised end-to-end. After PR #360
merged:

```
$ brew install passt
…
passt: Linux is required for this software.
Error: passt: An unsatisfied requirement failed this build.
```

`passt` is Linux-only — it uses Linux-specific syscalls
(`vmsplice`, namespace primitives) that don't have macOS equivalents.
The Homebrew formula refuses to build it. Plan 87's default flip
(`MVM_NETWORKING=passt`) therefore fail-closes on every macOS
contributor host — the exact platform mvm targets as a Tier 1
development host.

libkrun's C API anticipates this: `libkrun.h` ships **two** virtio-net
backend functions:

- `krun_add_net_unixstream(ctx, path, fd, mac, features, flags)` — for
  `passt` (Linux) **or `socket_vmnet`** (macOS).
- `krun_add_net_unixgram(ctx, path, fd, mac, features, flags)` — for
  `gvproxy` (cross-platform but the slp/krun maintainers ship a
  macOS-specific Homebrew bottle) or `vmnet-helper`.

The slp/krun Homebrew tap (`brew install slp/krun/{libkrun, libkrunfw,
gvproxy}`) is the canonical macOS install path; gvproxy is what their
documentation expects libkrun consumers to use.

## Goal

Restore Plan 87's user-visible promise (default `dev up` works on a
fresh `brew install`'d macOS host) by adding gvproxy as a second
networking backend, picked automatically per-OS.

## Design

### Backend selection

```
                   ┌──────────────┐
  cargo run -- dev up   →   resolve_networking_mode()
                          │
                          ▼
   MVM_NETWORKING unset / empty / unrecognised
                          │
                          ▼
                target_os = "linux"?  → NetworkingPreference::Passt
                target_os = "macos"?  → NetworkingPreference::Gvproxy
                                        (NEW)
                else / TSI override  → NetworkingPreference::Tsi
                MVM_NETWORKING=passt → Passt (works only on Linux;
                                       fail-closed on macOS)
                MVM_NETWORKING=gvproxy → Gvproxy (works on both;
                                         macOS-preferred)
```

The user-facing flag stays `MVM_NETWORKING={tsi, passt, gvproxy}`;
unset implies "the OS-best default."

### Rust surface

```rust
// crates/mvm-libkrun/src/lib.rs
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum NetworkingMode {
    #[default]
    Tsi,
    Passt { mac: [u8; 6], scratch_dir: String },
    Gvproxy { mac: [u8; 6], scratch_dir: String },  // NEW
}

impl KrunContext {
    pub fn with_passt(self, mac: [u8; 6], scratch_dir: impl Into<String>) -> Self { … }
    pub fn with_gvproxy(self, mac: [u8; 6], scratch_dir: impl Into<String>) -> Self { … }  // NEW
}
```

### FFI surface

`mvm-libkrun::sys` gains `add_net_unixgram_path(c_path, mac, features, flags)`.
gvproxy listens on a unix-domain socket (path-based, not fd-based —
that's the unixgram vs unixstream difference). The Rust wrapper
mirrors the passt one but passes `c_path != NULL` + `fd = -1`.

`build.rs` already pulls `krun_add_net_unixgram` from the existing
bindgen allowlist (`allowlist_function("krun_.*")`).

### Host-side supervisor

```rust
// crates/mvm-libkrun/src/gvproxy.rs   (NEW, mirrors passt.rs)
pub fn spawn(scratch_dir: &Path) -> Result<GvproxyHandle, GvproxyError> {
    let bin = locate_gvproxy().ok_or(GvproxyError::NotInstalled { … })?;
    let socket_path = scratch_dir.join("gvproxy.sock");
    let log_path    = scratch_dir.join("gvproxy.log");
    let child = Command::new(bin)
        .arg("-listen-vfkit").arg(&socket_path)
        .arg("-log-file").arg(&log_path)
        .spawn()?;
    wait_for_socket(&socket_path, Duration::from_millis(500))?;
    Ok(GvproxyHandle { child: Some(child), socket_path })
}

pub struct GvproxyHandle {
    child: Option<Child>,
    socket_path: PathBuf,
}

impl GvproxyHandle {
    pub fn socket_path(&self) -> &Path { … }
}

impl Drop for GvproxyHandle { /* SIGTERM → grace → SIGKILL */ }
```

The key difference from `passt::spawn`: gvproxy listens on a unix
socket the caller creates, rather than receiving a pre-opened fd.
libkrun connects to that socket itself via `krun_add_net_unixgram(ctx,
c_path = socket_path, fd = -1, …)`.

`wait_for_socket` polls for the socket file to appear (gvproxy creates
it ~tens of ms after spawn). 500 ms timeout is generous.

### Supervisor dispatch

`run_supervisor()` already handles `NetworkingMode::Passt`. We add a
parallel `NetworkingMode::Gvproxy` arm:

```rust
let (krun, _gateway) = match &cfg.krun.networking {
    NetworkingMode::Tsi => (configure_pre_net(&cfg.krun)?, GatewayHandle::None),
    NetworkingMode::Passt { mac, scratch_dir } => {
        let h = passt::spawn(Path::new(scratch_dir))?;
        let k = configure_pre_net(&cfg.krun)?;
        k.add_net_unixstream_fd(h.socket_fd(), mac, PASST_NET_FEATURES, 0)?;
        (k, GatewayHandle::Passt(h))
    }
    NetworkingMode::Gvproxy { mac, scratch_dir } => {
        let h = gvproxy::spawn(Path::new(scratch_dir))?;
        let k = configure_pre_net(&cfg.krun)?;
        k.add_net_unixgram_path(h.socket_path(), mac, PASST_NET_FEATURES, 0)?;
        (k, GatewayHandle::Gvproxy(h))
    }
};
```

`GatewayHandle` is an internal enum that holds either backend's
handle so its Drop runs at the end of `run_supervisor`.

### Default-resolution platform pivot

```rust
// crates/mvm-build/src/libkrun_builder.rs
pub fn resolve_networking_mode() -> NetworkingPreference {
    match parsed_env() {
        Some("tsi")     => NetworkingPreference::Tsi,
        Some("passt")   => NetworkingPreference::Passt,
        Some("gvproxy") => NetworkingPreference::Gvproxy,
        Some("")  | None => default_for_host(),
        Some(other) => {
            tracing::warn!(value = other, "MVM_NETWORKING unrecognised; …");
            default_for_host()
        }
    }
}

fn default_for_host() -> NetworkingPreference {
    if cfg!(target_os = "linux") {
        NetworkingPreference::Passt
    } else if cfg!(target_os = "macos") {
        NetworkingPreference::Gvproxy
    } else {
        NetworkingPreference::Tsi
    }
}
```

### `mvmctl doctor`

Today probes `passt` regardless of OS. Plan 88 makes it probe the
right binary per host:

- Linux → probe `passt`; warn if missing.
- macOS → probe `gvproxy`; warn if missing.
- Both probes also run when the active `MVM_NETWORKING` explicitly
  requests the off-OS backend (so the user sees the precise hint).

## Workstreams

**W1 — FFI + Rust surface (~½ day).**

- New `sys::add_net_unixgram_path()` wrapper.
- New `NetworkingMode::Gvproxy` variant + `KrunContext::with_gvproxy`.
- Unit tests: serde roundtrip for the new variant; `add_net_unixgram_path`
  rejects empty paths.

**W2 — Host-side gvproxy supervisor (~1 day).**

- New `mvm-libkrun::gvproxy` module mirroring `passt`.
- `spawn`, `GvproxyHandle`, `Drop` (SIGTERM → 2 s grace → SIGKILL).
- `locate_gvproxy()`, `install_hint()` (macOS → `brew install
  slp/krun/gvproxy`; Linux → `apt install gvproxy` / build from
  source).
- `wait_for_socket(path, timeout)` helper shared between backends.
- Unit tests: spawn-and-reap; `NotInstalled` when binary hidden;
  socket appears within timeout.

**W3 — Dispatch + default per-OS (~½ day).**

- `run_supervisor()` arm for Gvproxy.
- `resolve_networking_mode()` `default_for_host()` pivot.
- Updated `resolve_networking_mode_parses_env` test:
  - Linux: default = Passt; macOS: default = Gvproxy.
  - `MVM_NETWORKING=gvproxy` resolves to Gvproxy on both.
  - `MVM_NETWORKING=passt` resolves to Passt on both (user accepts
    consequences on macOS).

**W4 — Doctor + docs (~½ day).**

- `mvm-cli::doctor::passt_check` becomes `network_backend_check` that
  picks the right binary per OS.
- CLAUDE.md "Host dependencies" section:
  - macOS: `brew install slp/krun/{libkrun,libkrunfw,gvproxy}` (the
    current `brew install libkrun libkrunfw passt` line is wrong on
    macOS — `passt` is Linux-only).
  - Linux: `apt install passt libkrun-dev` (no libkrunfw — Linux
    distros ship a real kernel).
- ADR-055 amendment: a §"Cross-platform backends" section noting the
  passt/gvproxy split, why both exist (Linux vs macOS sycall
  expectations), and the per-OS default.

**W5 — End-to-end smoke (~½ day).**

- `cargo run -- dev up` on a fresh macOS host with `brew install
  slp/krun/{libkrun,libkrunfw,gvproxy}` succeeds through Stage 0 and
  produces a builder VM image. The in-VM nix build reaches
  cache.nixos.org via gvproxy + virtio-net.
- Capture the timing for `specs/plans/87-passt-virtio-net.md` follow-up
  notes.

## Non-goals

- **socket_vmnet** support. socket_vmnet is the
  `krun_add_net_unixstream` cousin on macOS; gvproxy
  (`krun_add_net_unixgram`) is the slp/krun-recommended path and what
  libkrun's macOS Homebrew install ships. socket_vmnet remains a
  future option if gvproxy hits a wall.
- **vmnet-helper** support. Same reasoning — gvproxy is the
  documented macOS path.
- **vfkit-mode for cross-hypervisor compat.** gvproxy supports
  `-listen-vfkit` (what we use here) and other modes; we don't
  expose the others.
- **Removing passt.** Linux contributors still use it. Both backends
  coexist; the dispatcher picks per OS.

## Success criteria

1. `cargo run -- dev up` on macOS with `brew install
   slp/krun/{libkrun,libkrunfw,gvproxy}` boots a Stage 0 VM and
   completes the builder VM image build. No `MVM_NETWORKING` override
   needed.
2. `cargo run -- dev up` on Linux with `apt install passt libkrun-dev`
   still works (same code path, different backend).
3. `cargo test --workspace --lib` clean; `cargo clippy --workspace --
   -D warnings` clean.
4. `mvmctl doctor` reports `gvproxy: available — <version>` on macOS,
   `passt: available — <version>` on Linux. Missing-binary case
   surfaces the correct OS-specific install hint.
5. Memory `reference_libkrun_gotchas.md` updated with the
   passt-Linux-only / gvproxy-macOS split.

## ADR-055 amendment

Append a §"Cross-platform backends — Plan 88 amendment":

> Plan 87 / ADR-055 v1 assumed passt was cross-platform. Plan 88
> corrects this: passt uses Linux-specific syscalls (`vmsplice`,
> namespace primitives) and is not available on macOS. libkrun
> anticipates this asymmetry — its C API ships
> `krun_add_net_unixstream` (passt / socket_vmnet) and
> `krun_add_net_unixgram` (gvproxy / vmnet-helper) as parallel
> entry points. mvm's networking dispatch picks the right backend
> per OS: passt on Linux, gvproxy on macOS. The
> `MVM_NETWORKING={tsi,passt,gvproxy}` flag remains the explicit
> override.

## Order of operations

W1 + W2 ship together (FFI + supervisor — small, mechanical,
independently mergeable). W3 + W4 land next; W5 is the smoke gate
before declaring Plan 88 closed.

Suggested PR sequence:

- **PR1 (W1 + W2):** `krun_add_net_unixgram` FFI + gvproxy
  supervisor. No consumers; doesn't change behavior.
- **PR2 (W3 + W4):** Dispatcher + default pivot + doctor + docs.
  Flips macOS default from `Passt` (fail-closed) to `Gvproxy`
  (working). This is the user-visible fix.
- **PR3 (W5):** smoke test results captured in
  `specs/plans/87-passt-virtio-net.md`'s "Completion" section +
  closes Plan 88.

Each PR is independently revertible. PR2 is the load-bearing one for
unsticking macOS contributors.
