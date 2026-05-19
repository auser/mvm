# Plan 87 — Replace libkrun TSI with passt-backed virtio-net

**Status:** drafted 2026-05-18, awaiting review.
**Pairs with:** ADR-055 (`specs/adrs/055-passt-virtio-net.md`, lands with this plan).

## Problem

libkrun's TSI (Transparent Socket Impersonation) mode is the default
"no network stack in the guest" path the project has relied on since
Plan 72 W5 (the libkrun cutover). TSI hijacks the guest's AF_INET
socket calls at the syscall layer and forwards them to a host-side
proxy, so the guest kernel doesn't need a network stack and there's
no virtio-net device or DHCP dance.

Plan 86 verified TSI works for the simple HTTP fetch (the initial
nixpkgs flake tarball downloads cleanly from github.com), but breaks
on the patterns nix actually relies on for substituter and source
fetches:

- nix's internet-availability pre-check fails → `warning: you don't
  have Internet access; disabling some network-dependent features`
- `cache.nixos.org` is never even queried by nix in the guest
- HTTPS-with-redirect destinations bail with `HTTP error 302 (curl
  SSL connect error)`
- Tarball mirrors with HTTP/2 hit `Server returned nothing (no
  headers, no data) (52) Empty reply from server`

The result: every in-VM `nix build` falls back to source builds for
2800+ derivations, and most of those source builds fail to fetch
their tarballs. Stage 0 cannot finish.

This isn't an mvm bug in the conventional sense — it's TSI's edge
cases. TSI is an experimental libkrun mode; it works well enough for
"open one socket, read one response" but does not transparently
proxy modern HTTP behavior (HTTP/2 multiplexing, HTTPS handshake
sequencing, connection reuse, redirect chains). The same TSI mode is
why the steady-state builder VM (downstream of Stage 0) fails the
same way — this is not a Stage-0-specific issue.

## Goal

Migrate every libkrun-backed VM mvm boots — Stage 0 ur-seed, the
steady-state builder VM, and the runtime microVMs — from TSI to
`passt`-backed virtio-net. Passt is a userspace network gateway
(Red Hat project, single binary, no kernel/setuid required) that
translates between virtio-net frames in the guest and AF_INET sockets
on the host. libkrun has first-class passt support via
`krun_set_passt_fd()`.

After this plan: the guest sees a normal `eth0`, gets a DHCP lease
from passt's built-in DHCP server, resolves DNS through its own
resolver, and reaches the host's network the same way any normal
Linux VM would. Every HTTP/HTTPS pattern that works on the host
works in the guest.

## Design

### Components

```
+------------------------------------------------------------+
| Host (macOS or Linux)                                      |
|                                                            |
|   +---------+  AF_UNIX (fd handoff)  +---------+           |
|   | libkrun |------------------------| passt   |           |
|   | ctx     |                        | child   |           |
|   +----+----+                        +----+----+           |
|        | vmnet/Hypervisor.framework       | AF_INET socks  |
|        v                                  v                |
|   +-----------------------------------+                    |
|   | Linux guest (libkrunfw kernel)    |                    |
|   |   virtio-net -> eth0              |                    |
|   |   udhcpc -> 192.168.0.2/24        |                    |
|   |   gateway 192.168.0.1 (passt)     |                    |
|   |   DNS via passt's resolver        |                    |
|   +-----------------------------------+                    |
+------------------------------------------------------------+
```

`PasstSupervisor` (new) owns the passt child process lifetime,
exposes the fd-pair, and tears down on Drop.

### Workstreams

**W1 — `mvm-libkrun` FFI surface (~½ day).**

- Add `krun_set_passt_fd` to the bindgen allowlist (already in
  libkrun.h on hosts with libkrun-1.17+).
- New `sys::set_passt_fd(ctx, fd) -> Result<(), Error>`.
- New `KrunContext::networking: NetworkingMode` enum:
  ```
  enum NetworkingMode {
      Tsi,                    // legacy, libkrun's default when no NIC
      Passt { socket_fd: i32 } // virtio-net via passt
  }
  ```
- `configure()` dispatches: TSI → no extra FFI; Passt → call
  `set_passt_fd` before `start_enter`.
- Unit tests: `KrunContext::with_passt_socket(fd)` accessor; round-trip
  through `KrunContext`'s serde to preserve the fd handle.

**W2 — `PasstSupervisor` host-side child (~1 day).**

- New crate-internal module `mvm-libkrun::passt`.
- `PasstSupervisor::spawn(scratch_dir) -> Result<PasstHandle>`:
  - `socketpair(AF_UNIX, SOCK_STREAM, 0)`
  - spawn `passt` with args `["--fd=N", "--no-pid",
    "--no-resolv-conf", "--no-tcp-init", "--mtu=65520"]` — flags
    pinned per ADR-055 discussion of which TCP/UDP profile we want
  - Parent keeps one socket end → returned as `socket_fd`
  - Child inherits the other end via the `--fd=N` arg
- `Drop` impl: SIGTERM the child, wait with timeout, SIGKILL on
  timeout. Same pattern as `mvm-libkrun-supervisor`'s own shutdown
  path.
- `PasstHandle::socket_fd() -> RawFd` — fed to `KrunContext::Passt`.
- Host-side install probe: `which passt` + version pin → friendly
  error on missing dep with `brew install passt` / distro hint.
- Unit tests: spawn + immediate-shutdown roundtrip; spawn + simulate
  hang + Drop-kill timeout.

**W3 — Builder VM and runtime defaults (~½ day).**

- `mvm-build::libkrun_builder::LibkrunBuilderVm` defaults to
  `NetworkingMode::Passt`. TSI remains available as an opt-in
  (`with_networking(NetworkingMode::Tsi)`) for debugging.
- `mvm-backend::libkrun::LibkrunBackend` (runtime path) ditto.
- Builder VM flake's `cmd.sh` drops the `substituters = …` /
  `trusted-public-keys = …` lines — defaults pull from nix's
  built-in config once network actually works.
- Stage 0 ur-seed path threads through unchanged — the change is
  transparent to mvm-builder-init.

**W4 — In-VM `udhcpc` + resolv.conf wiring (~½ day).**

- `mvm-builder-init::network::setup_network()` already calls
  `udhcpc` and treats the failure as non-fatal. With virtio-net
  present, the call succeeds and produces `/etc/resolv.conf` from
  the DHCP option.
- Verify: `udhcpc -i eth0 -s /etc/udhcpc/default.script` writes
  the right resolv.conf. Add an explicit script under
  `nix/lib/udhcpc-default.script` so the busybox-default doesn't
  surprise us (it tries to call `/etc/resolv.conf.head` etc.).
- ur-seed flake's `cmd.sh` drops its manual cacert symlink —
  resolv.conf comes from DHCP, ca-bundle.crt remains for HTTPS.
- Smoke target: `mvmctl dev up` from clean cache reaches
  `Built builder VM image at ...`.

**W5 — Host install + bootstrap (~½ day).**

- macOS: `passt` is in homebrew (`brew install passt`). Add to the
  `mvmctl doctor` host-dep probe alongside libkrun + libkrunfw.
- Linux: standard distro packages (`apt install passt`,
  `dnf install passt`).
- New `mvm-libkrun::passt::install_hint()` mirroring the existing
  libkrun install-hint pattern.
- `mvmctl doctor` reports `passt: <version>` / `passt: not found
  (run brew install passt)` so contributors hit the missing-dep
  error before `dev up` fails.

**W6 — TSI removal + ADR (~½ day).**

- ADR-055 lands describing the TSI→passt migration: why TSI fails,
  why passt is the production-ready substitute, why TSI stays
  available as an opt-in.
- Memory note `reference_libkrun_gotchas.md` updated to record the
  passt path as the canonical one and TSI's edge-case failures.
- Remove the in-repo TSI kernel patches under
  `nix/images/builder-vm/kernel/patches/` — they're no longer used
  (libkrunfw's bundled kernel still has TSI, which is fine, we just
  don't enable it from the host side anymore). Or keep them under a
  `legacy/` subdir with a README pointing at ADR-055 for context.
- Update SPRINT.md.

### Migration path

The existing `mvmctl dev up` invariants stay intact:

- `ensure_dev_image` / `bootstrap_builder_vm_image` paths unchanged.
- Stage 0 ur-seed cache layout unchanged.
- The only user-visible change: an extra `passt` install on first
  host setup. Documented in CLAUDE.md + the install runbook.

### Why not virtio-net via vmnet directly

Apple Virtualization.framework's vmnet API is closed-source and tied
to the macOS host's network stack. Using vmnet directly means
shelling out the network plumbing into Apple's hands — fine for the
Apple Container backend (Plan 72 W5.C / Plan 75) but mvm runs the
same code on Linux KVM via Firecracker, where vmnet doesn't exist.
Passt is a single binary, cross-platform, and decoupled from any
hypervisor — the same code path works on macOS libkrun, Linux
libkrun, and (with the obvious wiring) Linux Firecracker.

### Why now

TSI works for the trivial case (one socket, one response). The
moment mvm tries to do anything realistic — nix builds, mvm-oci
image pulls, dev-shell `curl`, deps-install fetches — TSI's edge
cases surface. Every workstream downstream of `dev up` either
inherits TSI's flakiness or adds workarounds. Replacing the
network substrate once removes the workarounds from every
downstream feature.

## Non-goals

- **No re-design of libkrun's host integration.** This plan uses
  libkrun's existing `krun_set_passt_fd` API; the Rust wrapper +
  PasstSupervisor are additive.
- **No virtio-net for Apple Container or Firecracker backends.**
  Apple Container has its own network model (the Apple
  Virtualization.framework vmnet path) and Firecracker on Linux has
  TAP + bridge — both already work. The TSI problem is specific to
  libkrun.
- **No DNS or HTTP/firewall policy changes in the guest.** The
  Plan 73 deps-install egress proxy (mvm-egress-proxy) remains
  load-bearing for production microVMs running untrusted
  workloads. This plan is about the Stage 0 / builder-VM / dev-VM
  trust boundary where the contributor is the only principal.
- **No fork of passt.** Bumping the pinned version is a one-line
  change in `nix/ur-seed/flake.nix` (or wherever the doctor probe
  lives); upstream passt is well-maintained.

## Success criteria

1. `cargo run -- dev up` from a clean cache (no
   `~/.cache/mvm/builder-vm/aarch64/` and a fresh
   `~/.cache/mvm/ur-seed/aarch64/`) completes end-to-end and
   produces a usable builder VM image, with the in-VM nix build
   using `cache.nixos.org` substitutes (visible "copying path" log
   lines).
2. `cargo test --workspace` clean; `cargo clippy --workspace --
   -D warnings` clean.
3. `mvmctl doctor` reports `passt: <version>` when installed,
   `passt: not found` with the install hint when missing.
4. The steady-state builder VM (post-Stage-0) does in-VM Nix builds
   that hit cache.nixos.org without the "no Internet" warning.
5. Memory `reference_libkrun_gotchas.md` updated.

## Order of operations

W1 + W2 ship together (small, mechanical FFI + child-process
plumbing). W3 + W4 are dependent on W1/W2 (need the FFI live).
W5 + W6 are docs/install plumbing and land alongside or after.

Suggested PR sequence:

- PR1: W1 + W2 (passt FFI + supervisor, no consumers yet)
- PR2: W3 + W4 (consumers, behind opt-in flag —
  `MVM_NETWORKING=passt` env var)
- PR3: flip default; ADR-055; W6 cleanup
- PR4: W5 doctor probe + install docs

This sequence keeps every PR independently revertible. PR3 (the
default flip) is the one to gate on end-to-end smoke against the
contributor's `cargo run -- dev up`.
