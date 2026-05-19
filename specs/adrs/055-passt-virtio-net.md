# ADR-055 — libkrun networking via passt + virtio-net

**Status:** proposed 2026-05-18, implements Plan 87.

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
  default to `Passt`; `Tsi` stays available behind an env var for
  debugging.
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

## References

- Plan 87 — `specs/plans/87-passt-virtio-net.md`
- Plan 86 — `specs/plans/86-ur-seed-stage0-bootstrap.md`
  (end-to-end smoke that exposed TSI's edge cases)
- Plan 72 W5.D — the prior round of libkrun debugging notes
- libkrun upstream: https://github.com/containers/libkrun
- passt upstream: https://passt.top/
- libkrun's `krun_set_passt_fd` API documented in `libkrun.h`
