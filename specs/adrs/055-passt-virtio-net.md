# ADR-055 — libkrun networking via passt + virtio-net

**Status:** accepted 2026-05-19, implements Plan 87. Default flipped from TSI → Passt by Plan 87 W5 / PR3. **Amended 2026-05-19 by Plan 88** to add gvproxy as the macOS backend (passt is Linux-only — see §"Cross-platform backends" below). **Amended 2026-05-26 by [Plan 102 W6.A](../plans/102-gateway-audit-substrate-impl.md) / [ADR-058](058-claim-10-bytes-leaving-trust-boundary.md):** TSI removed entirely — it bypassed virtio-net (no host fd to splice), which violates the claim-10 no-bypass invariant. `MVM_NETWORKING=tsi` is no longer accepted; only `passt` and `gvproxy` resolve. The historical TSI context below is retained for archaeology.

## Context

Since Plan 72 W5 (libkrun cutover) the libkrun-backed VMs mvm boots
have relied on libkrun's TSI (Transparent Socket Impersonation)
networking mode. TSI hijacks the guest's `AF_INET` socket calls at
the syscall layer and forwards them to a host-side proxy, so the
guest kernel doesn't need a network stack and there's no virtio-net
device or DHCP dance. ADR-046 §"Two artifact layers" treats this as
an internal libkrun detail.

Plan 86's end-to-end smoke proved TSI doesn't actually support the
HTTP behavior nix relies on:

| Behavior                            | TSI result                                  |
| ----------------------------------- | ------------------------------------------- |
| Single HTTP GET (e.g. flake tarball)| works                                       |
| nix's internet-availability probe   | fails → `warning: you don't have Internet…` |
| HTTPS with 302 redirect             | `HTTP error 302 (curl SSL connect error)`   |
| HTTP/2 multiplexed connection       | `Server returned nothing (curl 52)`         |
| Substituter chatter to cache.nixos.org | never even attempted                     |

The result is that `nix build` falls back to source builds for 2800+
derivations, most of which then fail to fetch their tarballs. Stage 0
cannot complete. The same TSI mode is used by the steady-state
builder VM (downstream of Stage 0) and the runtime microVMs, so the
failure pattern is universal across every libkrun-backed VM.

This is not an mvm bug. TSI is an experimental libkrun mode designed
for "this guest only ever opens one socket and reads a response";
modern HTTP — connection reuse, HTTP/2 multiplexing, HTTPS handshake
sequencing, redirect chains — is outside its design envelope. Plan 86
W3 v3 implemented `extract_bundled_kernel()` to source the kernel
patches from libkrunfw's own bundled kernel (the patches are
validated against libkrunfw's specific kernel revision), which fixes
a kernel-oops class but not the TSI proxy behavior.

## Decision

Migrate every libkrun-backed VM from TSI to **passt + virtio-net**.

Passt is a userspace network gateway (Red Hat project, single-binary,
no kernel patches or `CAP_NET_ADMIN` required) that translates
between virtio-net frames in the guest and `AF_INET` sockets on the
host. libkrun has first-class passt support via `krun_set_passt_fd()`
(libkrun 1.17+).

The guest sees a normal `eth0`, gets a DHCP lease from passt's
built-in DHCP server, resolves DNS through its own resolver, and
reaches the host's network the same way any normal Linux VM would.
HTTP/HTTPS patterns that work on the host work in the guest.

Implementation lives in Plan 87:

- New `mvm-libkrun::sys::set_passt_fd()` FFI wrapper.
- New `mvm-libkrun::passt::PasstSupervisor` host-side child that
  socketpair's with libkrun, spawns passt, owns its lifecycle.
- `KrunContext::networking: NetworkingMode { Tsi, Passt {..} }`.
- libkrun-backed VMs (builder-VM + dev-VM + runtime microVMs)
  default to `Passt`. (Plan 102 W6.A: `Tsi` removed entirely —
  the env-var escape is gone. No-bypass invariant, ADR-058.)
- `mvmctl doctor` probes for `passt` and emits an install hint when
  missing.

## Alternatives considered

- **Stay on TSI, work around the edge cases.** Rejected: the
  workarounds (force-substituters, retry-on-redirect, alternative
  HTTP clients) would have to live in every workstream that touches
  the guest's network. Replacing the substrate once eliminates the
  workaround surface area.
- **Use Apple Virtualization.framework's vmnet directly.** Rejected:
  vmnet is closed-source, macOS-only, and tied to the host's network
  stack. mvm runs the same code on Linux KVM via Firecracker where
  vmnet doesn't exist. Passt is cross-platform and decoupled from
  the hypervisor.
- **Implement a TSI-aware HTTP shim in the guest.** Rejected:
  building a working subset of HTTP/2 + redirect chains + HTTPS
  handshake on top of TSI's existing semantics is a multi-quarter
  effort with no upside vs. just using a real network stack.
- **Pin libkrunfw to a version where TSI is more complete.** Rejected
  as a non-fix — TSI's design constraints don't disappear with
  libkrunfw bumps. The libkrun upstream itself has been moving
  toward passt as the recommended default.
- **Switch to gvproxy** (the rootless-podman gateway). Considered.
  gvproxy and passt occupy the same niche; passt is simpler, faster,
  and the libkrun integration is documented. Bumping or migrating to
  gvproxy later is a one-line change in the supervisor.

## Consequences

- **All libkrun VMs gain a virtio-net interface.** mvm-builder-init
  already handles `udhcpc` (currently a no-op because there's no
  interface); the change is transparent to the in-guest init.
- **New host-side dependency: `passt`.** `brew install passt` on
  macOS, distro package on Linux. Doctor probe + install hint added.
- **TSI patches in the kernel become dead code from mvm's
  perspective.** libkrunfw's bundled kernel still carries them, which
  is fine — we just don't enable that path from the host side. The
  in-repo TSI patch port under `nix/images/builder-vm/kernel/`
  becomes legacy; Plan 87 W6 moves or removes it.
- **`mvm-egress-proxy` (Plan 73 Followup B.2.y / ADR-047)** remains
  load-bearing for production microVMs running untrusted workloads.
  This ADR is about the network substrate, not the policy layer; the
  egress allowlist runs on top of passt-virtio-net the same way it
  ran on top of TSI.
- **The contributor onboarding sequence gains one step** (install
  passt). Documented in the Plan 87 W5 doctor probe + CLAUDE.md
  update.

## Security model

The host-side passt process runs as the contributor's user (not
root). It cannot bind privileged ports or modify the host's
firewall — its entire job is to relay packets between an `AF_UNIX`
socket and the host's TCP/UDP stack via standard userspace sockets.
The host kernel is the final policy layer for outbound traffic.

In production microVM contexts (running untrusted workloads), the
guest's egress allowlist is enforced by `mvm-egress-proxy` inside
the VM (Plan 73 Followup B.2.x). passt is the transport; the policy
is independent.

CLAUDE.md security claim 1 ("no host-fs access from a guest beyond
explicit shares") is unaffected: passt doesn't see the guest's
filesystem, only its virtio-net frames. Claim 9 (deps-volume
hash-lock + audit) is unaffected for the same reason.

### New untrusted-input surfaces introduced by Plan 87 (Plan 88 W6 amendment)

Moving from TSI to virtio-net opens three new host-side parsing
boundaries that didn't exist under TSI's syscall-hijack model. All
three run as the contributor's user (not root), so a successful
exploit is a code-execution-as-user boundary — not a host-kernel
boundary. None has filesystem visibility into the guest. But each is
a new fuzzing target:

1. **libkrun's virtio-net device emulator.** Parses virtio
   descriptors the guest writes to the virtqueue. Same class of risk
   QEMU / Firecracker / Cloud Hypervisor virtio implementations have
   carried for years.
2. **passt's frame parser** (Linux). C code dealing with raw
   Ethernet/IP/TCP/UDP/ICMP frames the guest sends. Well-audited by
   Red Hat security; not invulnerable.
3. **gvproxy's frame parser** (macOS / cross-platform). Go code, so
   memory-safety bugs are rare, but logic bugs in its DHCP server,
   TCP state machine, and ICMP responder remain possible.

Fuzz coverage by surface — only one of the three is genuine
first-party Rust we can put under cargo-fuzz. The supervisor's JSON
parser is the fourth boundary that the network-backend dispatch
opened (the supervisor's pipe semantics didn't change vs TSI, but
its config now carries `NetworkingMode::{Passt, Gvproxy}` variants
the parser has to handle).

| Surface | Where it lives | mvm's local fuzz coverage |
| ------- | -------------- | ------------------------- |
| `SupervisorConfig` JSON (stdin → `mvm-libkrun-supervisor`) | First-party Rust | **In tree.** Plan 88 W6 — `crates/mvm-libkrun/fuzz/fuzz_supervisor_config.rs`, wired into `security.yml::fuzz`. |
| libkrun virtio-net device emulator | C, inside `libkrun.dylib` | **Upstream.** Fuzzing requires a running guest per iteration; mvm trusts the libkrun project's own fuzz harness. |
| passt frame parser | C, external process | **Upstream.** Red-Hat-maintained; mvm runs passt as the contributor's user. |
| gvproxy frame parser | Go, external process | **Upstream.** Memory-safety bugs are rare in Go; logic bugs in the DHCP / TCP / ICMP responder are tracked by the gvproxy project. |

CLAUDE.md security claim 5 ("vsock framing is fuzzed") is extended
to "vsock framing + supervisor-config JSON are fuzzed" — explicit
about the in-tree / upstream split so a future reader doesn't take
it as a stronger claim than the harness actually backs.

A separate follow-up plan covers the aspirational external-process
gateway-frame fuzz harness (persistent gvproxy/passt subprocess
driven by a unix-socket fuzzer, plus a mocked libkrun virtqueue
harness). That work is out of Plan 88's scope — multi-week effort
with substantial dependency on upstream libkrun maintainers exposing
sanitizer-friendly entry points.

### `mvm-egress-proxy` becomes load-bearing

ADR-055 v1 already noted this, but it's worth flagging again as part
of the Plan 88 amendment:

- Under TSI, AF_INET socket calls were hijacked at the syscall
  layer. A workload couldn't bypass the egress allowlist because
  there was no Linux network stack in the guest to bypass through.
  `mvm-egress-proxy` (Plan 73 Followup B.2.x / ADR-047) was
  defense-in-depth.
- Under virtio-net (passt or gvproxy), the guest's real Linux
  network stack is the path. A workload that ignores `HTTPS_PROXY`
  / `HTTP_PROXY` env vars can open raw sockets directly to any
  destination passt/gvproxy will forward to. The in-VM iptables
  uid-owner rules `mvm-builder-init::install_egress_lockdown`
  installs are the only thing preventing bypass.

The policy layer (`mvm-egress-proxy` + iptables uid-owner) is
unchanged; what changed is its load-bearing status. ADR-047's
threat model still applies; production microVMs running untrusted
workloads still need the egress proxy active.

## Cross-platform backends (Plan 88 amendment, 2026-05-19)

ADR-055 v1 (above) assumed `passt` was cross-platform. End-to-end
smoke after Plan 87 PR3 merged surfaced the gap:

```
$ brew install passt
passt: Linux is required for this software.
```

`passt` uses Linux-specific syscalls (`vmsplice`, namespace
primitives, `splice`) that have no macOS equivalents — the Homebrew
formula refuses to build it. Since macOS is mvm's Tier 1
contributor host, this fail-closes every fresh `dev up` on the
platform the work was meant to fix.

libkrun's C API anticipates the asymmetry: `libkrun.h` ships **two**
virtio-net backend functions in parallel:

| libkrun call                  | Userspace backend(s)              | Socket type | Cross-platform? |
| ----------------------------- | --------------------------------- | ----------- | --------------- |
| `krun_add_net_unixstream`     | `passt` (Linux), `socket_vmnet` (macOS) | unixstream | No, per-backend |
| `krun_add_net_unixgram`       | `gvproxy`, `vmnet-helper`         | unixgram   | gvproxy: yes; vmnet-helper: macOS |

The slp/krun Homebrew tap (`brew install slp/krun/{libkrun, libkrunfw,
gvproxy}`) is the canonical macOS install path. gvproxy is the
libkrun maintainers' documented macOS backend — same project that
ships libkrun + libkrunfw.

**Resolution (Plan 88):** mvm dispatches the network backend per OS:

- Linux → `passt` via `krun_add_net_unixstream`
- macOS → `gvproxy` via `krun_add_net_unixgram` (path-based listener
  instead of fd-passed)

`MVM_NETWORKING={passt, gvproxy}` remains the explicit override (Plan 102 W6.A removed `tsi`).
Unset → the per-OS default. `passt` on macOS still fail-closes (the
binary doesn't exist), but the user gets a clear error rather than a
silent regression — `mvmctl doctor` flags the missing dep with the
right install hint per platform.

**Vz backend (Plan 98).** The Apple Virtualization.framework builder
also wires gvproxy on macOS — `mvm_vz::NetworkConfig::Gvproxy { socket_path, mac }`
piped through `mvm-vz-supervisor`'s `Network.swift` attaches a
`VZFileHandleNetworkDeviceAttachment` to the same gvproxy listener
libkrun uses via `krun_add_net_unixgram`. The host-side gateway
(gvproxy) is one process per dev session; the Vz and libkrun paths
just attach to its socket differently. Full Plan 98 selection-policy
context lives in **ADR-046 §"Vz as a second builder backend"**.

Both backends share the same threat model: a userspace process
running as the contributor's user, no privileged sockets, no host-fs
visibility into the guest. The libkrun-end of the virtio-net frame
transport is identical at the guest kernel layer; only the host-side
plumbing differs.

## References

- Plan 87 — `specs/plans/87-passt-virtio-net.md`
- Plan 88 — `specs/plans/88-gvproxy-macos-backend.md` (the
  cross-platform amendment above)
- Plan 86 — `specs/plans/86-ur-seed-stage0-bootstrap.md`
  (end-to-end smoke that exposed TSI's edge cases)
- Plan 72 W5.D — the prior round of libkrun debugging notes
- libkrun upstream: https://github.com/containers/libkrun
- passt upstream: https://passt.top/
- gvproxy upstream: https://github.com/containers/gvisor-tap-vsock
- libkrun's `krun_add_net_unixstream` / `krun_add_net_unixgram`
  APIs documented in `libkrun.h`
