---
title: "ADR-002: microVM security posture — explicit guarantees, layered defenses"
status: Accepted
date: 2026-04-30
revised: 2026-05-07
supersedes: none
related: ADR-001 (multi-backend execution); plan 25-microvm-hardening; plan 53-cross-platform-roadmap
---

## Status

Accepted. Implementation tracked in `specs/plans/25-microvm-hardening.md`. Workstreams W1–W6 shipped 2026-04-30.

The 2026-05-07 revision adds the **Trust layers (Matryoshka model)** section, names the seven CI-enforced claims explicitly, and adds a **per-backend tier matrix** showing which claims hold for each backend in `AnyBackend`. None of the original decisions or surfaces change — the revision is a re-framing for legibility, motivated by plan 53 (cross-platform roadmap) where multiple backends with different tier coverage now coexist.

## Context

mvm runs untrusted-shaped Linux workloads in microVMs. Through Sprint
14 the project's stated security model was a single claim: "no SSH in
microVMs, ever — vsock-only communication, with the dev `Exec` handler
gated at compile time by the `dev-shell` Cargo feature." That claim is
true and load-bearing, but it is the *only* hardened layer. Everything
underneath it — the guest's own privilege model, the rootfs's integrity,
the host-side proxy socket, the supply chain by which the dev image
arrives, the deserializer that parses every host-to-guest message — is
soft. A failure in any one of those defeats the whole stack regardless
of the vsock claim.

The project's value proposition is that a developer can run third-party
or AI-generated code in a microVM and trust the isolation. That promise
demands that the protections be technical, verifiable, and stated
explicitly.

This ADR captures the decisions; the implementation sequence is in
`specs/plans/25-microvm-hardening.md`.

## Threat model

Adversaries, in priority order:

1. **A malicious guest workload.** Code running inside a microVM. Must
   not be able to read the host filesystem outside explicit shares,
   talk to the host network, escape the hypervisor, read another
   guest service's secrets, or tamper with the rootfs's baked closure.

2. **A same-host hostile process.** Another local user, or another
   process running as the host user, must not be able to talk to the
   dev VM's guest agent, read its console log, write to its rootfs
   cache, or tamper with launchd plists / GC roots.

3. **A compromised supply chain.** A malicious nixpkgs commit, a
   compromised GitHub account hosting prebuilt artifacts, or a
   typo-squatted Cargo dep, must not silently land code in a microVM
   without producing a verifiable signature failure.

A *malicious host* (the macOS or Linux machine running mvmctl itself)
is **explicitly out of scope**. mvmctl trusts the host with the
hypervisor, the GC roots, the launchd plists, the user's secrets in
`/mnt/secrets`, and the private build keys.

## Trust layers (Matryoshka model)

mvm's defense-in-depth is structured as five trust layers nested like a
matryoshka doll. Each layer trusts the layer *below* it and nothing
else; an attacker has to break through every boundary above to reach
the host. A failure in any one layer is bounded — the layer below
still enforces its own contract.

```
┌───────────────────────────────────────────────────────────────┐
│ L5 — Workload (untrusted code, AI-generated, user scripts)    │
│      enforced by: per-service uid (W2.1), bounding-set drop   │
│                   (W2.3), seccomp tier `standard` (W1.1, W2.4)│
├───────────────────────────────────────────────────────────────┤
│ L4 — Guest agent (parses host messages, launches services)    │
│      enforced by: uid 901 setpriv (W4.5), no_new_privs,       │
│                   `do_exec` absent in prod (W4.3),            │
│                   fuzzed deser + deny_unknown_fields (W4.1-2) │
├───────────────────────────────────────────────────────────────┤
│ L3 — Guest kernel (Linux from Nix, ephemeral, isolated)       │
│      enforced by: dm-verity rootfs + roothash on cmdline +    │
│                   mvm-verity-init initramfs (W3)              │
├───────────────────────────────────────────────────────────────┤
│ L2 — VMM (userspace, Rust, seccomp-jailed, unprivileged)      │
│      enforced by: minimal device set (Firecracker), seccomp   │
│                   default-on, host-side proxy socket 0700     │
│                   (W1.2), port allowlist (W1.3)               │
├───────────────────────────────────────────────────────────────┤
│ L1 — Host + hypervisor (KVM on Linux, Apple VZ on macOS,      │
│                          Hypervisor.framework via libkrun)    │
│      enforced by: hardware (CPU rings, EPT, IOMMU); host      │
│                   hardening is the user's responsibility      │
└───────────────────────────────────────────────────────────────┘
```

The "matryoshka" framing comes from the 2026 microVM ecosystem
discourse (notably <https://emirb.github.io/blog/microvm-2026/>).
It is the same pattern used by Fly.io Sprites, AWS Lambda's
SnapStart, E2B, Vercel Sandbox, and Kata Containers. The mvm
adaptation is that L5 is enforced *inside* the guest by uid/seccomp
(plan 26), so even a guest-kernel compromise (L3 fall) doesn't grant
arbitrary access to other in-guest services.

## The seven CI-enforced claims

Each claim is backed by a CI gate that fails the build if the claim
ceases to hold. Claims are mapped to the trust layer they *primarily*
defend; many claims have ripple effects across multiple layers, but
the primary defended layer is the one the gate fails first.

| # | Claim | Primary layer | Workstream | CI gate |
|---|---|---|---|---|
| 1 | No host-fs access from a guest beyond explicit shares | L2/L5 | W2.1, W1.1, W2.3, W2.4 | seccomp regression; per-service-uid bind audit |
| 2 | No guest binary can elevate to uid 0 | L2/L4 | W2.2, W2.3 | bind-mount RO assertion; setpriv `--no-new-privs` regression |
| 3 | A tampered rootfs ext4 fails to boot | L3 | W3.1–W3.4 | `verified-boot-artifacts` + live-KVM tamper test in `security.yml` |
| 4 | The guest agent does not contain `do_exec` in production builds | L4 | W4.3 | `prod-agent-no-exec` symbol-grep job in `ci.yml` |
| 5 | Vsock framing is fuzzed | L2/L4 | W4.1, W4.2 | `cargo-fuzz` targets in `crates/mvm-guest/fuzz/`; `deny_unknown_fields` audit |
| 6 | Pre-built dev image is hash-verified | cross-cutting (supply chain) | W5.1 | `download_dev_image` SHA-256 check; `MVM_SKIP_HASH_VERIFY` only documented escape |
| 7 | Cargo deps are audited on every PR | cross-cutting (supply chain) | W5.2 | `cargo-deny` + `cargo-audit` jobs; reproducibility double-build (W5.3) |

L1 (host + hypervisor) has no claim of its own — the host is trusted
by definition (see Threat model). L1 *enables* claim 3 (verified boot
needs a hypervisor that respects the kernel cmdline). If the host is
compromised, every layer falls; that case is explicitly out of scope.

## Surfaces

A complete enumeration of every surface that bears on these adversaries.
Each is addressed in the corresponding workstream of plan 25.

### Host → guest

| Surface | Today | Hardened |
|---|---|---|
| Vsock framing in `mvm-guest-agent` | `serde_json::from_slice`, no fuzzing, parses any `GuestRequest` | `deny_unknown_fields`, depth/size caps, fuzzed in CI (W4.1, W4.2) |
| `Exec` handler | Compile-gated by `dev-shell` feature, but no CI gate | CI greps the prod binary for `do_exec`; absence is enforced (W4.3) |
| `ConsoleOpen` | PTY data port multiplexed over vsock | Same; mitigated by per-service uid (W2.1) and proxy-socket lockdown (W1.2, W1.3) |
| `StartPortForward` bind address | Not audited | Asserted `127.0.0.1`-only by regression test (W4.4) |
| Guest agent's own privileges | Runs as PID 1 = uid 0 | Runs as uid 901 `mvm-agent` user under `setpriv` (W4.5) |

### Guest → host

| Surface | Today | Hardened |
|---|---|---|
| VirtioFS workdir share | Writable, scoped to project dir | Unchanged shape, but per-service uid means no service can write there without explicit user grant (W2.1) |
| VirtioFS datadir share | Writable, scoped to `~/.mvm` | Same; mode-locked containment via uid + `nosuid,nodev` mount opts (W2.3) |
| Host-side proxy socket | Mode inherits umask (typ. 0755) | Mode `0700` post-bind (W1.2) |
| Vsock proxy port-forward | Any port allowed | Allowlist: `GUEST_AGENT_PORT` (5252) + `PORT_FORWARD_BASE..+65535` (W1.3) |
| Console log + daemon log | Mode inherits umask | Mode `0600` (W1.4) |
| Block device passthrough | `nix-store.img` attached as `/dev/vdb`; host doesn't mount it | Documented invariant: host shall never `mount` this file. Static-check in code review. |

### Inside the guest

| Surface | Today | Hardened |
|---|---|---|
| Service privilege model | All services run as uid 900 in shared `serviceGroup` | Per-service uid, per-service group, mode-0400 secrets (W2.1) |
| `/etc/{passwd,group,nsswitch}` | Tmpfs-writable at runtime | Bind-mounted read-only after init (W2.2) |
| Service launch privileges | busybox `su -s sh -c …` | `setpriv --no-new-privs --bounding-set=-all --groups=<gid>,900` (W2.3) |
| Per-service syscall filtering | None (default tier `unrestricted`) | Default tier `standard`; per-service overrideable (W1.1, W2.4) |
| Rootfs integrity | None | dm-verity over the read-only ext4 lower layer; root hash on cmdline (W3.1-W3.4) |
| Capabilities | Inherited bounding set | Empty bounding set per service (W2.3) |

### Supply chain

| Surface | Today | Hardened |
|---|---|---|
| Pre-built dev image | HTTPS download, no integrity check beyond TLS | SHA-256 verified against const compiled into mvmctl (W5.1) |
| Cargo deps | No audit | `cargo-deny` + `cargo-audit` in CI; pre-commit local check (W5.2) |
| mvmctl binary reproducibility | Not verified | Double-build hash check in CI (W5.3) |
| SBOM | Not emitted | CycloneDX SBOM attached to releases (W5.4) |
| nixpkgs trust | `cache.nixos.org` trusted via `trusted-public-keys` | Inherited assumption; documented but not changed |
| Linux builder SSH | `sudo cp` writes `/etc/ssh/ssh_config.d/200-linux-builder.conf` | Documented; user-level prompt before sudo |

## Decisions

The following are decided and committed for v1 of this hardening:

1. **Defaults must be safe.** Every option whose value affects security
   defaults to the safer choice, and users opt *out* with documentation.
   No more `seccomp = unrestricted` defaults; no more `0755` socket
   defaults.

2. **Defense in depth, not a single chokepoint.** The vsock-only claim
   stays load-bearing, but every layer beneath it is also tightened.
   A failure in any one layer must not be catastrophic.

3. **Verified boot is mandatory for production microVMs.** The dev VM
   is exempt because its overlay-upper write layer can't compose with
   dm-verity; that exemption is named explicitly so the dev VM is
   never used as a "production microVM" by accident.

4. **The guest agent does not run as root in production.** Period. It
   doesn't need to, and the day-zero exploit cost of "uid 0 + buggy
   deser" is too high to keep paying.

5. **CI gates the security claims.** Every claim made in this ADR is
   backed by a CI check that fails the build if the claim is no
   longer true. Specifically: `cargo-deny`, `cargo-audit`, the `do_exec`
   symbol grep, the seccomp regression test, the proxy-socket perm
   test, the verity round-trip test, the bind-address test. Listed in
   plan 25 §W6.

6. **The threat model is documented and lived-with.** A malicious host
   is out of scope. Multi-tenant guests are out of scope. Hardware-
   backed key attestation is out of scope. These limits are in the
   ADR so we don't accidentally commit to defending against them.

## Per-backend tier matrix

Plan 53 (cross-platform roadmap) introduces multiple backends —
Firecracker, Apple Container, libkrun, Docker, microvm.nix — each
with different layer coverage. A given user run carries the tier of
its active backend, not the strongest tier the project supports. The
following matrix is what `mvmctl doctor` reports and what the
mvm-cli startup banner surfaces (loudly, when the active backend
falls below Tier 1).

| Backend | L1 | L2 | L3 | L4 | L5 | Notes |
|---|---|---|---|---|---|---|
| Firecracker (Linux + KVM) | ✅ | ✅ | ✅ | ✅ | ✅ | **Tier 1** — full ADR-002. All seven claims hold. |
| Apple Container (macOS 26+ Apple Silicon) | ✅ VZ | ✅ Containerization | ⚠️ no verified boot yet | ✅ | ✅ | Tier 2 — claim 3 partial; claims 1, 2, 4, 5, 6, 7 hold. |
| libkrun (Linux KVM, macOS HVF) | ✅ | ✅ | ⚠️ no verified boot yet | ✅ | ✅ | Tier 2 — claim 3 partial; comparable VMM TCB to Firecracker. |
| Docker | ❌ shared host kernel | ❌ container runtime is L2=host kernel | ❌ shared with host | ✅ | ✅ | **Tier 3** — claims 1, 2, 3 do *not* hold; 4, 6, 7 hold; 5 N/A (unix socket). |
| microvm.nix (QEMU) | ✅ KVM | ⚠️ QEMU TCB much larger | ⚠️ partial verified boot | ✅ | ✅ | Tier 2 — claims 3 partial; QEMU's larger device model raises L2 audit cost. |

**Tier discipline**: Tier 1 is the production default and the only
tier that carries the *full* ADR-002 promise. Tier 2 carries six of
the seven claims with claim 3 (verified boot) tracked as a follow-up
once verified-boot lands for VZ/HVF. Tier 3 (Docker) carries only the
supply-chain and guest-agent claims; the L1–L3 isolation collapses to
the host kernel. Plan 53 §"Security posture decision" documents *why*
we keep Docker available but unpromoted — the convenience is real,
but we refuse to launder a container as a microVM in marketing or in
auto-selected defaults.

`mvmctl doctor` (plan 40 folded the standalone `security` verb into
doctor) renders this matrix per-host with the active backend
highlighted and prints a loud `MVM_ACK_DOCKER_TIER`-suppressible
warning banner whenever Tier 3 is auto-selected.

## Consequences

### Positive

- The vsock-only claim becomes one of seven enforced claims, each with
  CI evidence.
- The dev VM's "trust mvmctl entirely" model is now an *explicit choice*
  the codebase makes, not a side-effect of missing layers.
- New contributors get a clear story: "here's what mvm protects against,
  here's what it doesn't, here's how each protection is enforced."

### Negative / accepted costs

- The production guest closure grows by ~1.5 MB to include
  `pkgs.util-linux` (for `setpriv`/`runuser`).
- dm-verity adds a second VirtioBlk device per VM and a few hundred
  ms to first-boot setup.
- `cargo-deny`/`cargo-audit` in CI will occasionally block merges on
  upstream advisories. This is the *point*; we accept the friction.
- Per-service uid means existing example flakes need a one-line audit
  to confirm they don't rely on the shared `serviceGroup` for cross-
  service file sharing. (None observed today.)

### Explicit non-goals

- **Malicious host defense.** Out of scope. Documented.
- **Multi-tenant guests.** Out of scope.
- **TPM/SEV/attestation.** Out of scope for v1.
- **Network policy enforcement at hypervisor level.** The
  `network_policy` field exists in `mvm-core` and the seccomp tier
  filters network syscalls, but the hypervisor itself doesn't enforce
  guest egress destinations beyond NAT vs. tap. Noted, not addressed
  in this ADR; potential follow-up.

## Reversal cost

If a later decision wants to undo a layer (e.g. roll back per-service
uid because of a use case we didn't foresee):

- W1 items are one-line patches; trivially reversible.
- W2 items change the init contract; reversal requires a flake-API
  version bump because user flakes can become uid-aware.
- W3 (verity) is the biggest commitment; reversing means dropping the
  "rootfs integrity" claim from the security posture, which would
  warrant its own superseding ADR.
- W4-W5 items are CI/test additions; trivially reversible if they
  prove too noisy.

## References

- Plan: `specs/plans/25-microvm-hardening.md`
- Plan: `specs/plans/53-cross-platform-roadmap.md` (per-backend tier discipline)
- Related ADRs: `001-multi-backend.md`, `public/.../adr/001-firecracker-only.md`
- User-facing version of the layer model: `public/src/content/docs/security/matryoshka.md`
- Surface enumeration came from this session's audit; the seven
  numbered "additional surfaces" beyond the eight in the existing
  posture document are folded into the table above.
- The "matryoshka" framing draws on the 2026 microVM ecosystem
  discourse (e.g. <https://emirb.github.io/blog/microvm-2026/>);
  the same defense-in-depth pattern is used by Fly.io Sprites,
  AWS Lambda SnapStart, E2B, Vercel Sandbox, and Kata Containers.
