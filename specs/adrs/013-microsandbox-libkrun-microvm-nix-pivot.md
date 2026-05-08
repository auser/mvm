---
title: "ADR-013: Pivot to microsandbox + libkrun + microvm.nix; drop Lima"
status: Proposed
date: 2026-05-07
related: ADR-002 (security posture), ADR-014 (VmBackend trait), plan 60-mvm-microsandbox-migration
---

## Status

Proposed. Implementation tracked in `specs/plans/60-mvm-microsandbox-migration.md`. Phase 0 + Phase 1 deliver the build/exec pivot; subsequent phases compose on top.

## Context

The previous iteration of `mvm` (at `../mvm`) used Lima as the macOS dev-VM hop and Firecracker as the production hypervisor on Linux. Two pain points motivated the pivot:

1. **macOS dev experience was indirect**: every guest action traversed `host → Lima Ubuntu → Firecracker microVM`. Boot times were dominated by Lima warm-up; first-launch UX was brittle.
2. **Build pipeline lacked portability**: Nix builds ran inside ephemeral Firecracker builder VMs, gated by KVM availability. macOS and Windows hosts had no clean path.

The new direction:

- **microsandbox** (Apache-2.0, libkrun-backed) becomes the **builder** and the macOS/Windows execution path. libkrun gives us native Hypervisor.framework on macOS and KVM on Linux without a wrapping Lima VM.
- **Firecracker** stays as the preferred Linux production execution path because of its smaller attack surface, faster cold boot, and existing security work (jailer, dm-verity, seccomp tier).
- **microvm.nix** (MIT) becomes the Nix-flake foundation for microVM image generation. It abstracts Firecracker / Cloud Hypervisor / QEMU / crosvm / kvmtool / stratovirt as a NixOS module — adding a new backend later is a config change, not a kernel rewrite. **Fallback path**: if the per-bump audit (`xtask audit-flake`) of microvm.nix surfaces a security regression we can't accept, we fall back to the previous iteration's hand-rolled NixOS modules in `../mvm/nix/`. The fallback is a **named, ready-to-execute escape hatch**, not just an ADR sentence.
- **Lima is dropped entirely.** The macOS path is microsandbox-direct; no intermediate Linux VM.

## Decision

1. **Builder**: microsandbox-backed Nix builds (`mvm-build/src/pipeline/microsandbox.rs`); persistent warm-pool per tenant (ADR-015).
2. **Execution backend selection** at runtime:
   - Linux + `/dev/kvm` available → Firecracker
   - macOS / Windows / Linux without KVM → microsandbox (libkrun)
3. **Image generation**: extend microvm.nix's NixOS module with our security overlay (W2.1 per-service uids, W2.4 seccomp tiers, W3 dm-verity, W2.2 read-only `/etc`).
4. **Drop Lima** from the codebase; no fallback path.

## Consequences

**Positive**:
- Single fewer hop on macOS (host → microsandbox → guest) — faster boot, cleaner UX.
- microvm.nix gives multi-hypervisor portability for free.
- Builder pipeline runs on every host class.
- Reduced surface: no more Lima-specific code paths.

**Negative**:
- Adds a third-party dep (microvm.nix) to the build trust boundary — pinned by hash and CI-audited (`xtask audit-flake`).
- Some Linux-specific guarantees (dm-verity at boot, seccomp tier "strict") only hold on the Firecracker path. The microsandbox path uses image-hash-on-load + HMAC chain instead. Documented in the per-backend tier matrix in ADR-002.
- Loss of the Lima dev-VM means macOS users without microsandbox installed get a clearer error instead of a working but slow path.

**Neutral**:
- mvmd's facade contract (`mvmctl::core`, `mvmctl::runtime::shell`, etc.) is unaffected — this is a backend swap, not a contract change.

## Boot-time budget — busybox-as-PID-1, NOT NixOS+systemd

The project's value prop includes "as fast as possible" boot — concretely **sub-200ms to userspace on Firecracker / libkrun**, sub-1s on Apple Virtualization framework. Neither NixOS+systemd nor Alpine+OpenRC reaches that:

| init system | Firecracker p50 | Why |
|---|---|---|
| NixOS + systemd | 1–3 s | systemd unit graph, generators, dbus, locale-loader |
| Alpine + OpenRC | 300–500 ms | OpenRC runlevel + service supervision |
| **busybox-as-PID-1** (custom init) | **~50–150 ms** | One static binary, one script, exec the entrypoint |

microvm.nix's NixOS module is a convenient way to *describe* a microVM, but it produces a NixOS-systemd rootfs that's structurally too heavy for our boot budget. We therefore:

1. **Use microvm.nix only for the hypervisor abstractions** it exposes (runner-script generation, hypervisor-specific config knobs). Pinning microvm.nix as a flake input is still an ADR-013 commitment.
2. **Build the rootfs ourselves** as busybox-as-PID-1, the way the previous iteration did. The mkGuest implementation (`nix/lib/mk-guest.nix`) emits an ext4 image whose `/init` is a tiny script that mounts `/proc`, `/sys`, `/dev`, sets up vsock, and execs the user's entrypoint.
3. **No initrd in the default path** — kernel modules required at root mount (virtio-blk, virtio-vsock, ext4) are built into the kernel image, so `init=/init` runs without a stage-1 initramfs detour. Saves ~30-50ms vs the microvm.nix initramfs path.
4. **NixOS+systemd remains available as an opt-in** for users who explicitly want it (`init = "nixos"` parameter on mkGuest). Boot will be ~1-3s; we surface that warning in mkGuest's module docs.

The previous iteration shipped this exact strategy and was approaching the upstream Firecracker reference (~125ms). We replicate that, then tighten further per Phase 9's perf gate (`tests/perf.rs::cold_boot_p50_within_budget`).

### Per-backend boot budgets (CI gate, Phase 9)

**Floor: every backend must boot in ≤ 300 ms cold p50.** The number is intentionally aggressive — busybox-as-PID-1 + a trimmed kernel + direct-`vmlinux` boot all exist precisely so we can hit it. A backend that can't reach the floor is a backend we don't ship.

| Backend | Cold p50 | Snapshot-cloned p50 | Notes |
|---|---|---|---|
| Firecracker (Linux/KVM) | ≤ 300 ms | ≤ 30 ms | Trivially achievable with custom init; aim for ~150 ms in practice. |
| microsandbox / libkrun (Linux/KVM) | ≤ 300 ms | ≤ 30 ms | libkrunfw bundles kernel; matches Firecracker on Linux. |
| microsandbox / libkrun (macOS HVF) | ≤ 300 ms | ≤ 60 ms | HVF init overhead is real; reaching the floor needs the kernel + initramfs trim from §"Boot-time budget" to be tight. |
| Apple Virtualization framework | ≤ 300 ms | ≤ 200 ms | Apple's hypervisor overhead. If we can't hit 300 ms here we drop the backend (see ADR-031 — macOS path is microsandbox-direct anyway). |
| Cloud Hypervisor (post-Phase-10) | ≤ 300 ms | ≤ 50 ms | Same target; slightly heavier device model. |

CI perf gate: `xtask perf --backend <name> --p50-ms 300 --runs 100` (Phase 9). The smoke at `tests/smoke_e2e_boot.rs` (Phase 1 W6) runs a single boot and asserts the floor on every PR that touches the boot path.

## Guest agent supervision

`/init` (PID 1) forks **two** processes after staging the filesystem:

1. The **guest agent** in the background, under `setpriv` to uid 990. The agent listens on vsock for host-mediated tool RPCs (web_search, code_eval, file transfer, etc.), reports system metrics, and handles lifecycle events (sleep/wake, stop). Without it the host can boot the VM but can't talk to it for anything beyond hypervisor-level control.

2. The **entrypoint** in the foreground, under `setpriv` to the resolved entrypoint uid (root in dev, 1000 in prod by default).

PID 1 stays uid 0 (kernel mandate) but exec's nothing as root after the supervision fork.

**Implementation status (Phase 1 W6.1.1 — partial):**
- The supervision pattern is in place: `/init` forks the agent in the background under uid 990 before setpriv-exec'ing the entrypoint.
- The agent **binary** at `/usr/local/bin/mvm-guest-agent` is currently a **placeholder stub** — a sh script that logs its startup uid to `/dev/console` and sleeps in a loop. It demonstrates the supervision shape but doesn't implement the vsock RPC surface.
- Every `mkGuest`-built derivation surfaces `passthru.mvm.agentBinary = "stub"` so consumers can detect this. Production deployments will refuse to boot a `"stub"` image once the policy lint lands.
- W6.1.2 swaps in the real Rust binary (`crates/mvm-guest/src/bin/mvm-guest-agent.rs` — ~2400 LOC of vsock RPC). That swap needs cross-compile infrastructure (a Linux builder) and is its own focused wave.

The supervision wiring matters even with the stub because: (a) the dev/prod uid split is real today, (b) `/etc/passwd` + `/etc/group` are baked correctly today, (c) the host-side `mvmctl status` cross-check against `/proc/<pid>/status` works today, and (d) swapping the binary path in the rootfs population step is a one-line change.

## Linux builder via microsandbox (no Lima)

macOS hosts can't `nix build` Linux derivations natively — `nix build` emits a "no Linux builder available" error and stops. The previous iteration solved this by running a Lima VM as a Linux builder; this iteration drops Lima entirely (per the body of this ADR), so the question becomes: how does a macOS user `mvmctl build .` without configuring host-side Nix infrastructure?

**Design: bootstrap a Linux builder inside microsandbox itself.**

Microsandbox supports OCI images, and Nix-bearing OCI images are widely available (`nixos/nix`, `nixpkgs/nix-flakes`, our own pinned image). On a macOS host without a Linux builder configured, `mvmctl build` can:

1. Detect the gap — `Platform::has_host_nix()` returns true but the Nix instance can't build Linux derivations (`nix-store --eval` against a Linux derivation fails, or `nix.conf` lacks a configured builder).
2. Pull a small, pinned Nix-bearing OCI image — once, cached in `~/.cache/mvm/builder-image/`.
3. Spawn a microsandbox sandbox from that image with the user's flake source bind-mounted as `/work`, the host's Nix store mount-shared as `/nix`, and a sane PATH.
4. Run `nix build .#default` inside the sandbox.
5. Extract the resulting rootfs (the runtime artifact) back to the host.
6. Hand the rootfs off to the runtime path (which uses microsandbox + `RootfsSource::DiskImage` per the OCI non-goal — the runtime never pulls OCI).

**Why this is consistent with the OCI non-goal.** The non-goal banned OCI from the **runtime/boot path** — the place where user workloads run, where reproducibility + offline-by-default + no-registry-trust matter. The **builder** lives in a different trust zone: it has to fetch from caches, talk to the network, run arbitrary `nix build` derivations. Builder VMs and runtime VMs are governed by different policies; using OCI for the builder doesn't compromise the runtime's invariants.

**Cache reuse.** The Nix store on the macOS host is bind-mounted into the builder sandbox as `/nix`. Builds populate the host store; subsequent builds (Linux or otherwise) reuse the same cached derivations. This is the same trick `nix-darwin`'s `linux-builder` uses — the difference is mvm doesn't require the user to have configured `nix-darwin`.

**Fallbacks.** If the user has already configured a host-side Linux builder (`nix-darwin`'s `linux-builder`, or a remote `nix-daemon` URL), mvm uses that — the microsandbox-builder path is the *zero-config* default, not a forced override. Detection: probe `nix-store --add-fixed sha256 /dev/null --realize` against a Linux derivation; success → the host can build; failure → fall through to the microsandbox builder.

**Implementation status.** Phase 1 W6.x ships the design as documented; the actual builder bootstrap is its own focused wave (needs the OCI image pinned + cached, the bind-mount semantics worked through, the artifact extraction path written). Tracked in Sprint 50 as a follow-up.

**This replaces every previous reference to "configure `nix-darwin`'s `linux-builder`" in the docs.** Users with an existing builder keep using it; everyone else gets the microsandbox-bootstrapped path with no host-side configuration.

## Privilege model — rootless workloads on busybox PID 1

PID 1 must be uid 0 (Linux kernel requirement; user-namespace tricks bring their own risk surface and are out of scope). `setpriv` drops privileges before exec'ing the workload, so the user-visible process tree is non-root by default in production.

| Process | Uid | Why |
|---|---|---|
| `/init` (PID 1) | 0 | Kernel mandates. Mounts `/proc`/`/sys`/`/dev`, sets up the world, then exec's the entrypoint via `setpriv`. |
| `mvm-guest-agent` | 990 | Vsock RPC handler. Never needs root. Always non-root regardless of mode. |
| Entrypoint (workload) | 0 (dev) / 1000 (prod) | Root by default in dev for debug ergonomics (`apt`, `mount`, etc.); non-root by default in prod for defense in depth. Override via `uids = { entrypoint = … }`. |

`setpriv` invocation uses `--reuid + --regid + --clear-groups + --no-new-privs` (matches ADR-002 W2.3). `--no-new-privs` blocks `setuid` re-elevation in the workload — a compromise of the entrypoint can't reach uid 0 even if it finds a SUID binary.

**Why dev defaults to root:** dev shells are interactive debug surfaces. `apt install`, `mount /dev/sdX`, `tcpdump -i any` — all expect root. Defaulting dev to non-root would break those flows on first try and push users to flip the override, which is friction without payoff. Dev is *already* a less-secure mode (the `accessible` distinction in ADR-013 §"Sealed vs accessible"); rootful entrypoint is consistent with that posture.

**Why prod defaults to non-root:** the ADR-002 W2.1 commitment — "no guest binary can elevate to uid 0." Defending against this requires the workload not *being* uid 0 to begin with. The rootless default lands a meaningful slice of W2.1 ahead of Phase 6's full security overlay; the rest of W2 (per-service uids, read-only `/etc`, dm-verity) layers on top without breaking the surface.

**Override knob:** `uids = { agent = N; entrypoint = M; }` on the `mkGuest` call. Valid permutations:
- `{ entrypoint = 1000 }` — rootless dev shell (forces non-root in dev mode)
- `{ entrypoint = 0 }` — rootful prod workload (rare; usually a misconfiguration; blocked at policy level once the lint lands)
- `{ agent = 5000 }` — non-default agent uid (e.g. to avoid collisions with a host-side range)

Values surface on the resulting derivation as `passthru.mvm.uids = { agent; entrypoint; }` and `passthru.mvm.rootlessEntrypoint :: bool`. `mvmctl status` reads them and cross-checks against `/proc/<pid>/status` in the guest at runtime.

## Non-goal: OCI / container images

**mvm is microVMs, not containers.** Even though microsandbox's API
exposes both — `RootfsSource::Oci(reference)` for OCI image pulls and
`RootfsSource::DiskImage { path, format, fstype }` for raw disk
images — we deliberately use **only the `DiskImage` path**.

Why this is a stated invariant, not a default:

1. **Architectural commitment.** The project's value prop is microVM
   isolation backed by Nix-built rootfs images. OCI brings registry
   pulls, layered images, image index resolution, and a different
   trust model — none of which we want in the trust boundary.
2. **Reproducibility.** Nix-built rootfs images are byte-reproducible
   given the same flake inputs (we gate this in CI). OCI images
   resolve through a registry, can be re-tagged, and don't carry the
   same guarantees by construction.
3. **Trust boundary minimalism.** Pulling from an OCI registry adds
   an external network dependency to the boot path. The microVM
   path is offline-by-default once the rootfs is built.
4. **Runtime path consistency.** The bridge between our `.ext4`
   rootfs files and microsandbox's `.disk()` builder (a sibling
   `.raw` hard-link with explicit `fstype("ext4")`) keeps the disk
   path entirely host-local. No registry, no auth, no pull cache.

**What this means for code review:** any PR that introduces
`RootfsSource::Oci`, `microsandbox::RegistryAuth`, OCI image
references, or related types is reviewed against this invariant.
The exception is the future `mvm-cve` crate (plan 60 §"Roadmap
support") which may parse OCI artifact metadata as input to the
CVE roller — that's a metadata path, not a runtime path.

## Alternatives considered

- **Keep Lima as a fallback**: rejected. Maintains a code path that doesn't get exercised in the pivot's primary use case. Either Lima is good enough to be the macOS path (it isn't, per UX measurements) or it's dead code.
- **Cloud Hypervisor as primary**: rejected for now. CH is heavier than Firecracker and lacks the existing security work; revisit when GPU passthrough (VFIO) is needed (ADR-030).
- **Hand-rolled Nix flake (no microvm.nix)**: rejected. The previous iteration's hand-rolled flake was ~5000 LOC of NixOS module work; microvm.nix replaces most of that and is actively maintained.

## Threat model impact

- **microvm.nix** as a third-party dep widens the supply-chain surface. Mitigated by hash-pinning in `flake.lock`, CI re-audit on every bump, and reproducibility double-build.
- **microsandbox 0.4.5** is itself a third-party dep. Same mitigation.
- The per-backend tier matrix from ADR-002 is updated: Firecracker tier remains "strict"; microsandbox tier is "standard" until parity work lands (post-Phase 6).

## Compliance impact

- SOC 2: positive — narrower scope (one fewer trust boundary on macOS).
- PCI: neutral — neither backend is PCI-certified out of the box.
- HIPAA: neutral.
- FedRAMP/FIPS: future — neither backend ships FIPS 140-3 crypto today.
