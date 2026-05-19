# Plan 89 — external-process gateway frame fuzz harness

**Status:** drafted 2026-05-19, awaiting prioritization (no scheduled sprint).
**Follows:** Plan 88 W6 (`specs/plans/88-gvproxy-macos-backend.md`) — Plan 88 W6 shipped the in-tree `fuzz_supervisor_config` target; Plan 89 covers the aspirational external-process frame fuzz harnesses that W6 deliberately scoped out.
**Amends:** ADR-055 §"New untrusted-input surfaces" — fills in the upstream-fuzz-by-mvm slot.

## Problem

After Plan 87 + Plan 88 land, virtio-net brings three new
untrusted-input parsers online on the host side:

1. **libkrun's virtio-net device emulator** — C, inside
   `libkrun.dylib`. Parses virtio descriptors the guest writes to the
   virtqueue.
2. **passt frame parser** (Linux) — C, external process. Parses raw
   Ethernet/IP/TCP/UDP/ICMP frames the guest sends.
3. **gvproxy frame parser** (macOS / cross-platform) — Go, external
   process. Same shape as passt but unixgram-flavored.

CLAUDE.md security claim 5 (after Plan 88 W6: "vsock framing +
supervisor-config JSON are fuzzed") explicitly leaves these three
surfaces to upstream-project fuzz coverage. Plan 89 closes that gap
locally, so a regression in any of the three parsers is caught by
**mvm's own** CI rather than only by upstream maintainers' harnesses.

This is not on the critical path for shipping a working `dev up` —
all three parsers are already shipped in stable upstream releases
with their own fuzz coverage. Plan 89 is hardening, not unblocking.

## Why this is non-trivial

The natural cargo-fuzz pattern (`fuzz_target!(|data: &[u8]| { … })`)
assumes a Rust function that consumes bytes and produces a value.
None of the three targets fit:

- **libkrun's virtio-net emulator** runs *inside* a guest. Reaching
  it requires either (a) a running libkrun VM per fuzz iteration
  (seconds per iter — impractical), or (b) an in-process harness
  that links libkrun and pumps frames through a mocked virtqueue
  without booting a guest. Option (b) requires libkrun to expose
  sanitizer-friendly entry points it doesn't currently have. Sergio
  Lopez (the upstream maintainer) would likely take a contribution
  but it's a meaningful upstream PR.
- **passt + gvproxy** run as external processes. The natural
  harness is "spawn the binary, write fuzzer bytes to its socket,
  verify no crash." That works but has three drawbacks:
  - Spawning per iteration is too slow (~100 ms minimum).
  - A persistent subprocess + per-iteration write is faster but
    needs careful state reset between iterations.
  - "No crash" is the only observable signal. Logic bugs that
    cause silent malfunction (DHCP misroute, TCP state corruption)
    are invisible to libFuzzer.

## Scope

Three workstreams, each multi-day:

**W1 — passt subprocess fuzz harness (~3 days).**

- New `crates/mvm-libkrun/fuzz/fuzz_targets/fuzz_passt_frame.rs`.
  Linux-only (`#[cfg(target_os = "linux")]` gate on the binary; the
  `[[bin]]` entry in `Cargo.toml` is conditional via the
  `target_family` cfg).
- Harness: on first iteration, spawn one persistent `passt --fd=N`
  subprocess wired to a `socketpair(AF_UNIX, SOCK_STREAM)`. The
  fuzzer end of the pair is kept by the harness; the passt end is
  passed via `--fd=N`. Per iteration, write the fuzzer's bytes to
  the harness end and check `passt.try_wait()` to detect crashes.
- Reset state between iterations is best-effort: passt is stateful
  (TCP connection table, ARP table) and we can't easily reset it
  without re-spawning. Accept the state-leak; the fuzzer is still
  more likely to surface a crash than no fuzzer is. Document this.
- Wire into `.github/workflows/security.yml::fuzz` with a Linux-only
  conditional step (`if: runner.os == 'Linux'`).
- Corpus seed: a handful of valid Ethernet/IP/TCP/UDP/ICMP frames
  (captured via `tcpdump` in a Stage 0 builder VM).

**W2 — gvproxy subprocess fuzz harness (~3 days).**

- New `crates/mvm-libkrun/fuzz/fuzz_targets/fuzz_gvproxy_frame.rs`.
  macOS-only (`#[cfg(target_os = "macos")]`) — gvproxy builds on
  Linux too but `gvproxy -listen-vfkit unixgram://…` is the canonical
  macOS entry point and we already exercise the Linux side via
  passt.
- Same shape as W1, but `gvproxy -listen-vfkit unixgram://<path>`
  doesn't take a fd — it creates the listener itself. The harness
  connects a unixgram socket to the listener path and sends fuzzer
  bytes via `sendto()`.
- Wire into `security.yml::fuzz` as a macOS conditional step. mvm's
  GitHub-hosted macOS runners can't currently run libkrun guests
  (no Hypervisor.framework access), but they CAN run gvproxy as a
  user process. Confirm `brew install slp/krun/gvproxy` works on
  the macOS runner image before relying on it.
- Corpus seed: same Ethernet/IP captures as W1, plus a handful of
  DHCP request frames since gvproxy's DHCP responder is a parser
  surface passt doesn't expose the same way.

**W3 — libkrun virtio-net mocked virtqueue harness (~2 weeks).**

This is the upstream-coupled piece. Two steps:

1. Upstream contribution to libkrun: a `krun_fuzz_virtio_net_frame`
   entry point (or similar — name TBD with upstream) that takes a
   buffer of bytes representing a virtio descriptor + frame payload
   and walks it through the emulator's parser without booting a
   guest. Coordinate with @slp on the libkrun discussion board.
2. New `crates/mvm-libkrun/fuzz/fuzz_targets/fuzz_virtio_net_frame.rs`
   that links libkrun (`features = ["libkrun-sys"]`) and calls the
   new entry point per iteration.

W3 is the most valuable target (libkrun's emulator is the most
attractive escape target — a successful exploit crosses out of the
guest sandbox) but also the highest-effort. It can ship independently
of W1+W2.

## Non-goals

- **Replacing upstream fuzz coverage.** Each upstream project
  maintains its own fuzz harness. Plan 89 is *additional* coverage
  bound to mvm's CI and audit chain, not a substitute for upstream.
- **Frame-mutation policies.** libFuzzer's mutator is sufficient;
  no custom byte-mutation strategy needed for v1.
- **Fuzzing through a running libkrun guest.** Even with W3's mocked
  harness, no fuzz target runs an actual guest — too slow, no CI
  hardware support.

## Success criteria

1. `fuzz_passt_frame` (Linux) and `fuzz_gvproxy_frame` (macOS) build
   under their respective host OSes, run for ≥5 minutes in PR CI
   and 30 minutes in nightly cron without crashing the gateway
   subprocess or the harness itself.
2. ADR-055 §"New untrusted-input surfaces" coverage table updates
   to show the upstream rows now have in-tree fuzz coverage.
3. CLAUDE.md security claim 5 widens to "vsock framing +
   supervisor-config JSON + virtio-net gateway frames are fuzzed"
   once W1 + W2 land.
4. (W3 only) `fuzz_virtio_net_frame` builds against a libkrun
   release that exposes the new entry point, with a CI lane that
   skips when running against older libkrun. Claim 5 widens again
   to mention the libkrun emulator.

## Order of operations

W1 and W2 are independent (different OSes, different harness
shapes). Either can ship first. W3 is gated on upstream libkrun
work and should not block W1/W2.

Suggested PR sequence:

- **PR1 (W1):** Linux passt subprocess fuzz harness.
- **PR2 (W2):** macOS gvproxy subprocess fuzz harness.
- **PR3 (W3, separate sprint):** libkrun mocked-virtqueue harness,
  contingent on the upstream entry-point landing.

Each PR is independently revertible. None is on the critical path
for any user-visible feature; this plan can be deprioritized if a
higher-value workstream needs the time.

## References

- Plan 88 — `specs/plans/88-gvproxy-macos-backend.md` (W6 shipped
  the in-tree `fuzz_supervisor_config` target; W6 also scoped this
  follow-up explicitly).
- ADR-055 — `specs/adrs/055-passt-virtio-net.md` §"New
  untrusted-input surfaces" (coverage-by-surface table that Plan 89
  updates).
- ADR-002 — `specs/adrs/002-microvm-security-posture.md` claim 5.
- libkrun upstream — https://github.com/containers/libkrun (target
  for the W3 entry-point contribution).
- passt upstream — https://passt.top/
- gvproxy upstream — https://github.com/containers/gvisor-tap-vsock
