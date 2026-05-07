# Plan 58 — gVisor backend (deferred)

> Status: deferred
> Decision recorded: 2026-05-07 (follow-up to plan 53)
> Trigger to revisit: see "Trigger conditions" below

## Why this plan exists

When Cloud Hypervisor (plan 54), crosvm (plan 55), and rust-vmm internalization (plan 56) were filed as deferred backlog placeholders alongside plan 53, gVisor wasn't named. This plan captures the same analysis for gVisor so a future contributor doesn't have to re-derive whether it belongs in mvm.

The short answer: **gVisor is genuinely interesting and doesn't fail plan 53's "fork test" the way Cloud Hypervisor did, but it doesn't pass the demand test today** — same situation as crosvm.

## What gVisor is

gVisor is Google's open-source userspace kernel (Apache-2.0). It sits between an unprivileged process and the host kernel, intercepting syscalls and re-implementing them in Go rather than passing them straight to the host. Production users include Google Cloud Run, Modal, AWS App Runner.

Three execution modes:

- `ptrace` — uses ptrace to intercept syscalls. Portable, slow.
- `kvm` — uses KVM as a hypervisor under the userspace kernel. Faster, requires `/dev/kvm`.
- `systrap` — newer, uses a custom signal-driven trap mechanism. Fast and portable.

Critically: gVisor is **not a hypervisor or VMM**. It's a different security model — one that re-implements the kernel rather than hardware-isolating it. The closest comparison in mvm's existing tier vocabulary is "stronger than Docker, weaker than a microVM."

## How gVisor maps onto the Matryoshka model

ADR-002's five trust layers don't fit gVisor cleanly:

| Layer | gVisor |
|---|---|
| **L1** Host + hypervisor | Depends on the gVisor mode. `kvm` mode uses KVM; `systrap`/`ptrace` modes don't have a hypervisor at all. Security doesn't come from hardware here. |
| **L2** VMM | gVisor itself plays this role — but as a userspace Go re-implementation of the Linux syscall surface (~400K+ LOC), not as a Rust VMM linking rust-vmm. Different software domain entirely. |
| **L3** Guest kernel | **Replaced.** gVisor is the kernel. There's no separate Linux guest kernel running inside a VM. |
| **L4** Guest agent | Same as Firecracker — a process running in the sandbox. |
| **L5** Workload | Same as Firecracker — workload runs under the gVisor sandbox. |

So adding gVisor would be adding a **different security category** to mvm rather than another microVM tier — somewhere between Tier 3 (Docker, shared kernel) and Tier 2 (libkrun / Apple Container, hardware-isolated microVM).

## How the seven CI-enforced claims map

| # | Claim | gVisor |
|---|---|---|
| 1 | No host-fs access from a guest beyond explicit shares | **Holds** (enforced at the gVisor syscall filter, not via hardware) |
| 2 | No guest binary can elevate to uid 0 | **Holds** |
| 3 | A tampered rootfs ext4 fails to boot | **N/A** — gVisor doesn't boot a rootfs the way a microVM does |
| 4 | Guest agent has no `do_exec` in prod | Holds (orthogonal) |
| 5 | Vsock framing is fuzzed | **N/A** — gVisor uses syscall interception, not vsock |
| 6 | Pre-built dev image is hash-verified | Holds (cross-cutting supply chain) |
| 7 | Cargo deps audited on every PR | Holds (cross-cutting supply chain) |

A `BackendSecurityProfile` for gVisor would land roughly:

```rust
BackendSecurityProfile {
    claims: [
        ClaimStatus::Holds,        // 1
        ClaimStatus::Holds,        // 2
        ClaimStatus::DoesNotApply, // 3 — no rootfs boot
        ClaimStatus::Holds,        // 4
        ClaimStatus::DoesNotApply, // 5 — no vsock
        ClaimStatus::Holds,        // 6
        ClaimStatus::Holds,        // 7
    ],
    layer_coverage: LayerCoverage {
        l1_host_hypervisor: false, // syscall filter, not hypervisor
        l2_vmm: true,              // gVisor itself
        l3_guest_kernel: false,    // gVisor *is* the kernel
        l4_guest_agent: true,
        l5_workload: true,
    },
    tier: "Tier 3+", // stronger than Docker, weaker than libkrun/Apple Container
    notes: &[
        "Userspace kernel sandbox (Google gVisor).",
        "L1 collapses — no hardware isolation. L3 collapses — gVisor replaces the guest kernel.",
        "Stronger than Docker (real syscall isolation + Go-implemented kernel) but weaker than a microVM (no hardware boundary).",
    ],
}
```

## Why we're deferring (not rejecting)

Plan 54 (Cloud Hypervisor) was *rejected* for posture reasons: every CH advantage is a feature Firecracker excluded for attack-surface reasons, so adding CH would fork the security narrative around Firecracker.

Plan 58 (gVisor) is **deferred, not rejected**. gVisor doesn't fork the narrative the way CH does — it's a different security category entirely. Adding it would be more like adding a new column to the tier matrix than splitting Firecracker's column. So the question becomes about demand and complexity, not posture.

Four reasons to defer:

1. **No user demand on record.** Same situation as crosvm (plan 55). We deferred crosvm with explicit trigger conditions; the same pattern fits here.
2. **The Nix rootfs compatibility story is unverified.** gVisor implements ~80% of Linux syscalls. Modern workloads using `io_uring`, certain `eBPF` features, niche namespace operations may fail. Validating our existing example flakes against gVisor is its own spike — likely 2–3 days.
3. **Adding a fifth tier complicates the Matryoshka pitch.** Today's 4-tier story (Tier 1 Firecracker / Tier 2 Apple Container + libkrun / Tier 3 Docker / fallback Lima) is clean. Adding "Tier 3+: gVisor" needs a doc rewrite to explain why it's stronger than Docker but weaker than libkrun. Worth doing if there's demand; not worth doing pre-emptively.
4. **It mostly improves the same path Docker covers** (the no-hardware-virt fallback). That path is *intentionally* a loud Tier 3 banner today (plan 53 §"Plan B"); making it nicer is in low-grade tension with the deliberate "this isn't real isolation" messaging.

## Trigger conditions to revisit

Either of these warrants pulling gVisor off the backlog:

1. **A user demonstrates high-density sandboxing needs.** Running 100s of micro-tenants per host where Firecracker's per-VM RAM floor (~30 MiB minimum) is too expensive. gVisor sandboxes can be tiny — a typical Cloud Run instance is ~5 MiB of overhead. mvm doesn't target that profile today, but a fleet/coordinator user (mvmd land) might.
2. **A user needs Tier 3+ isolation where microVMs aren't available and Docker is too weak.** Most likely: regulated environments where containers don't satisfy compliance but the host can't run KVM (air-gapped environments without nested virt, certain managed PaaS layers, niche cloud VM types where SlicerVM PVM also doesn't fit).

A "we should probably support gVisor" suggestion without one of those signals is **not** a trigger.

## What "implementation if pulled" would look like

For estimation only; not commitments.

### Phase G1 — Validate the Nix rootfs runs cleanly under gVisor (the real spike)

- Install `runsc` (gVisor's container runtime).
- Build `examples/minimal` and run the resulting rootfs under `runsc run`.
- Audit which syscalls our Nix images hit that gVisor doesn't implement — `runsc` logs unimplemented calls.
- Decide: do we adjust the Nix image to avoid them, or constrain gVisor to a "compatibility mode" subset of mvm flakes? Either path is real work.
- **Effort**: 2–3 days. Could land *as a follow-up to plan 57's libkrun spike* since both involve "boot a Nix rootfs in a non-Firecracker environment and see what breaks."

### Phase G2 — Add `mvm-gvisor` crate

- Thin shell-out to `runsc` (gVisor isn't a library — it's a CLI/daemon).
- Files mirror mvm-libkrun's shape: `Cargo.toml`, `src/lib.rs` with `is_available`, `start`, `stop`, `install_hint`. No bindgen — runsc is a process, not a library.
- **Effort**: ~1 day.

### Phase G3 — `GvisorBackend` impl in `mvm-runtime`

- Mirrors the existing libkrun and docker shapes. `BackendSecurityProfile` per the matrix above.
- Wire into `AnyBackend` with `--hypervisor gvisor` (alias `runsc`).
- `auto_select` policy: don't add gVisor to the auto-select chain — it's an opt-in tier. Users who want gVisor pass `--hypervisor gvisor` explicitly. This avoids the "is it a microVM?" decision creeping into auto-select silently.
- **Effort**: ~1 day.

### Phase G4 — Doctor + bootstrap + docs

- `mvmctl doctor` reports a `gvisor` row.
- `mvmctl bootstrap` hints at install on supported platforms (Linux distro packages, or `go install` from upstream).
- Update Matryoshka doc and ADR-002 to add the "Tier 3+" row with explicit caveats.
- **Effort**: ~1 day.

### Phase G5 — CI lane

- gVisor only runs on Linux (no macOS, no Windows). Linux runner, install runsc, smoke test.
- **Effort**: 0.5 days.

### Total

~5–7 days of work split across the phases. About the same shape as the libkrun spike (plan 57), just smaller because there's no native FFI.

## Trade with adjacent plans

- **Plan 57 (libkrun spike)** is the more impactful immediate work. It unblocks Intel Mac users and macOS-no-Lima — a real, identified user base.
- **Plan 54 (Cloud Hypervisor)** would fork the security narrative; this plan does not.
- **Plan 55 (crosvm)** is a closer analog to plan 58 in shape (both deferred, both await demand).
- **Plan 56 (rust-vmm internalization)** is a different layer of abstraction entirely.

If a hypothetical "Sprint 49 — adjacent backends" ever materializes, the priority order would be:

1. Land the libkrun spike (plan 57 W1–W3) — known user demand.
2. *Then* consider gVisor (plan 58) if demand surfaces.
3. crosvm (plan 55) and Cloud Hypervisor (plan 54) only on explicit user request.

## References

- Plan 53 §"Security posture decision" — the "fork test" gVisor passes (different category, doesn't fork Firecracker's narrative).
- Plan 54 — Cloud Hypervisor (rejected for posture reasons; *contrast* with this plan's "deferred for demand reasons").
- Plan 55 — crosvm (the closest analog: deferred with explicit demand triggers).
- ADR-002 §"Per-backend tier matrix" — the four-tier scale this plan would extend with a "Tier 3+" row.
- "Your Container Is Not a Sandbox" (emirb 2026): <https://emirb.github.io/blog/microvm-2026/> — discusses gVisor's role in the broader microVM landscape.
- gVisor upstream: <https://github.com/google/gvisor>
