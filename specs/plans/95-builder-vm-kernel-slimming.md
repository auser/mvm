# Plan 95 — Builder-VM kernel slimming (Plan 92 followup)

**Status:** drafted 2026-05-20.
**Follows:** Plan 92 (`specs/plans/92-minimal-builder-vm-kernel.md`).
**Depends on:** Plan 92's `linuxManualConfig` + `tinyconfig` base
(commits `fd04817c` + `e663abf4`, currently on
`worktree-plan-92-stock-kernel`, not yet merged to main). This plan
lands in a single PR that **carries those two commits forward**
together with the additions below — they're not split across two PRs.

## Problem

Plan 92 replaced the libkrunfw-patched stock kernel with a custom
minimal build via `pkgs.linuxManualConfig` + `make tinyconfig` + a
curated `enables` / `disables` list. The disables list at
`nix/images/builder-vm/kernel/default.nix:107-112` covers the obvious
subsystems (`MODULES`, `IPV6`, `DRM`, `SOUND`, `USB`, `WIRELESS`,
`BT`, `FB`) but **does not disable ARM64 SoC platform clusters**.

On aarch64-linux, `tinyconfig` plus `PCI=y` (required for our
`VIRTIO_PCI`) causes `make olddefconfig` to default a swarm of
SoC-specific drivers to `=y`:

- `PCIE_MESON` (`drivers/pci/controller/dwc/pcie-meson.c`, Amlogic).
- `OWL_SIRQ` (`drivers/irqchip/irq-owl-sirq.c`, Actions Semi).
- Various `drivers/clk/meson/*`, `drivers/pinctrl/meson/*`,
  `drivers/soc/amlogic/*`, and the analogous clusters for Allwinner,
  Apple, Broadcom, Hisilicon, MediaTek, Marvell EBU, NXP Layerscape,
  Qualcomm, Renesas, Rockchip, Tegra, etc.

We will never see any of these — the builder VM boots under libkrun
(Apple Silicon virt) or Firecracker (KVM virt), never on real SoC
hardware.

~~Separately, `nix/images/builder-vm/flake.nix:69-72` declares a
`microvm.nix` flake input that is **threaded through `libFor` but
never used** by any builder-VM derivation. Dead dep.~~

**Correction (2026-05-20, post-survey).** The earlier "dead dep"
diagnosis was wrong. `microvm.nix` is *locally unused* by the
builder-vm flake's own derivations, but `nix/lib/default.nix:6`,
`nix/lib/mk-guest.nix:34`, and `nix/lib/mkFunctionWorkload.nix:35`
take `microvm` as a required parameter (per ADR-013 — see
`nix/lib/mk-guest.nix:29` and the root `nix/flake.nix:40-47`).
The root flake actively uses `microvm.nixosModules.microvm` and
`microvm.declaredRunner`. Dropping `microvm` from the builder-vm
flake's `libFor` import would require restructuring `nix/lib/` to
make the parameter optional, which is bigger than the win.
Keep the input; W1 is dropped from this plan's scope.

## Decision

1. **Extend the kernel `disables` list** with the SoC `ARCH_*`
   clusters and any other platform symbols `olddefconfig` is
   defaulting on. Source-of-truth list derived empirically from the
   actual `.config` `olddefconfig` produces — not guessed.
2. **Defer kernel-warning surfacing UX** to a follow-up issue — out
   of scope for this plan.

### Why not microvm.nix

Considered and rejected on two angles:

- **As a base for our own kernel.** microvm.nix's optimization
  module trims userspace only — no `structuredExtraConfig`, no
  kernel `.config`. Its `microvm.kernel` option docstring redirects
  to nixpkgs' `boot.kernelPackages`, which inherits the full
  distribution kernel — the exact opposite of slim.
- **As a builder for user workload microVMs.** The eight files in
  `lib/runners/{alioth,cloud-hypervisor,crosvm,firecracker,kvmtool,
  qemu,stratovirt,vfkit}.nix` are launch-script generators tightly
  coupled to NixOS-module + their own runner. Adopting them means
  giving up `LibkrunBuilderVm` (Plan 72 W4) and our Firecracker
  launcher, which contradicts ADR-055 / Plan 88. Our `nix/lib/mkGuest`
  already produces the `$out/{vmlinux,rootfs.ext4,cmdline.txt,
  manifest.json}` layout our launchers expect.

The one legitimate adoption story is interop — supporting users who
already maintain microvm.nix-format flakes. That's an adapter we
write if a user asks, not a dependency we import.

## Implementation

### W0 — Carry Plan 92's kernel commits forward

`fd04817c` (slim custom builder-VM kernel via `linuxManualConfig`)
and `e663abf4` (kernel build fix — `runCommandCC` + `patchShebangs`
+ `defconfig`) are the base this plan extends. They land in the
same PR as W2-W3.

### ~~W1 — Drop dead `microvm.nix` flake input~~ (dropped)

Dropped post-survey. See Correction in §Problem. `microvm` is a
required parameter of `nix/lib/default.nix` and is actively used by
the root `nix/flake.nix` (`microvm.nixosModules.microvm` + the
declared runner) — not a dead dep. Removing it would require
restructuring `nix/lib/` to make the parameter optional. Out of
scope for kernel slimming.

### W2 — Expose `configfile` for verification baseline

Per `kernel/README.md:23-25`, the documented way to see what
`olddefconfig` produced is to temporarily expose `configfile` from
`kernel/default.nix` as a flake output and `nix build` it. Make this
a permanent debug output (gated by attr name, not stripped on the
prod path) so future audits don't have to re-patch the flake.

Workflow:

```sh
nix build .#configfile -o /tmp/kconfig
grep '^CONFIG_.*=y$' /tmp/kconfig | sort > /tmp/kconfig-before.txt
```

### W3 — Aggressive SoC / platform cluster disables

Append to the `disables` list in `kernel/default.nix:107-112`. Seed
set (refine from the `configfile` output in W2 before committing):

```nix
# ARM64 SoC platform clusters — we boot under libkrun (Apple Silicon
# virt) or Firecracker (KVM virt), never on real SoC hardware.
"ARCH_ACTIONS" "ARCH_ALPINE" "ARCH_APPLE" "ARCH_BCM" "ARCH_BRCMSTB"
"ARCH_EXYNOS" "ARCH_HISI" "ARCH_K3" "ARCH_LAYERSCAPE" "ARCH_LG1K"
"ARCH_MEDIATEK" "ARCH_MESON" "ARCH_MVEBU" "ARCH_NPCM" "ARCH_QCOM"
"ARCH_REALTEK" "ARCH_RENESAS" "ARCH_ROCKCHIP" "ARCH_S5PV210"
"ARCH_SEATTLE" "ARCH_SPRD" "ARCH_STM32" "ARCH_SUNXI" "ARCH_SYNQUACER"
"ARCH_TEGRA" "ARCH_THUNDER" "ARCH_THUNDER2" "ARCH_UNIPHIER"
"ARCH_VEXPRESS" "ARCH_VISCONTI" "ARCH_XGENE" "ARCH_ZYNQMP"

# Storage / device classes we never see in virtio-only microVMs.
"MTD" "PARPORT" "ATA" "SCSI" "INFINIBAND"
"STAGING" "MEDIA_SUPPORT"
```

Iterate: build → grep `=y` → confirm the offenders are gone →
`cargo run -- dev up` end-to-end → if it boots, ship.

Some symbols may need a parent disabled first if `olddefconfig`
re-enables them via a hard `select`. The README at
`kernel/README.md:18-21` documents that case.

### W4 — Surface kernel build warnings (deferred)

Out of scope for this plan. Filed as a follow-up issue. Current
behavior at `crates/mvm-build/src/libkrun_builder.rs:1131-1144`
discards stderr on success and surfaces only `tail -200` on failure.
A small UX win: on clean exit, emit `kernel build: N compiler
warnings (see /job/nix-stderr.log)`. Gated behind `MVMCTL_VERBOSE`.

## Verification

1. **Baseline `.config` + warning capture** (W2): grep `=y` from the
   current `configfile`, save as `kconfig-before.txt`. `nix log` the
   kernel derivation, save warnings as `warnings-before.txt`.
2. **Apply W3 disables**, rebuild, diff:
   `diff kconfig-before.txt kconfig-after.txt` — expect ~hundreds
   of `=y` lines removed (SoC platform drivers).
3. **`vmlinux` size**: `ls -l` the resulting kernel — expect a
   measurable size decrease (10-30% realistic on aarch64).
4. **End-to-end smoke**: `cargo run -- dev down && dev up` must
   succeed (kernel boots, `mvm-builder-init` reaches PID 1, builder
   VM becomes ready). Non-negotiable — if the VM doesn't boot we
   re-enabled something load-bearing; the diff tells us which.
5. **Workspace tests**: `cargo test --workspace` +
   `cargo clippy --workspace -- -D warnings`. The kernel `.config` is
   opaque to Rust, so these should be unaffected, but run them.
6. **Cross-arch**: verify on aarch64-linux (primary target — Apple
   Silicon libkrun host) and x86_64-linux (Linux KVM hosts) if
   reachable. ARM64 disables are no-ops on x86 and vice-versa.

## Risks

- **R1 — Disabling an `ARCH_*` symbol breaks `ARM64=y` itself.** Some
  parent symbols may have a hard dependency on at least one SoC being
  selected. Mitigation: the empirical W2 baseline tells us which
  `ARCH_*` are currently `=y`; we disable only those, not symbols we
  haven't observed in our build.
- **R2 — `olddefconfig` re-enables a disabled symbol via `select`.**
  Documented at `kernel/README.md:18-21` — disable the parent too.
  Iteration in W3 catches this.
- **R3 — Plan 92's base hasn't landed in main yet.** This plan's PR
  carries `fd04817c` + `e663abf4` forward. Risk: if Plan 92 lands
  separately first, we have a rebase / dedup step. Mitigation:
  bundle all three commits into one branch + PR; coordinate with
  the user before pushing if Plan 92 is being landed elsewhere.

## Cross-references

- Plan 92 (`specs/plans/92-minimal-builder-vm-kernel.md`) — establishes
  the `linuxManualConfig` + `tinyconfig` kernel build approach this
  plan extends. Commits `fd04817c` + `e663abf4` are the base.
- ADR-055 (`specs/adrs/055-passt-virtio-net.md`) — networking pivot
  to passt/gvproxy + virtio-net that made TSI patches obsolete and
  freed us to use a slim kernel without vendored patches.
- Plan 72 W4 (`specs/plans/72-builder-vm-via-libkrun.md`) —
  `LibkrunBuilderVm` launcher contract; our `nix/lib/mkGuest` writes
  to exactly its expected output layout.
- Plan 88 (`specs/plans/88-gvproxy-macos-backend.md`) — the macOS
  gvproxy backend reachable from inside the slim kernel via virtio-net.

## Validation note — Stage 0 tmpfs cap

During end-to-end `dev up` validation we hit `No space left on
device` during the `rustc-wrapper-1.91.1` substitute, repeatedly,
across 7 attempts. Initial diagnosis blamed VM RAM (bumped
`DEFAULT_MEMORY_MIB` 8 → 16 GiB, no effect). The actual root cause:
`crates/mvm-build/src/stage0/init.sh:87` hardcodes the `/nix`
tmpfs cap at `size=4G`, sized for the original ~600 MB builder-VM
closure. Stage 0 now also builds the kernel + the Rust binaries
(`mvm-builder-init`, `mvm-egress-proxy`) so the working set is
~10–13 GiB; the 4 GiB cap clips long before the VM RAM is
exhausted. Bumping VM RAM without bumping the tmpfs cap is a no-op.

The fix is one line in `init.sh` (`size=4G` → `size=14G`) plus the
matching `DEFAULT_MEMORY_MIB` bump (8 → 16 GiB) so the VM has
enough RAM to back the larger tmpfs. They're paired.

## Follow-ups (out of scope for this plan)

Discovered while validating Plan 95 end-to-end:

### FU-1 — `mvmctl cache prune --reap-orphans` (or `mvmctl doctor --fix`)

**Problem.** mvmctl spawns `mvm-libkrun-supervisor`, which spawns
`gvproxy`, which is a grandchild of mvmctl. When mvmctl exits
abnormally (crash, ^C, SIGKILL) the supervisor + gvproxy are
reparented to launchd PID 1 and outlive mvmctl indefinitely. Same
for the `tail -F …console.log` watchers some flows spawn. A long
session leaves ~10-20 orphans per day, plus their gigabyte-scale
`~/.cache/mvm/builder-vm/vms/<id>/` directories. The existing
`mvmctl cache prune` and `mvmctl dev down` do not reap them.

**Proposed shape.** New verb (or `cache prune --reap-orphans` flag)
that walks `~/.cache/mvm/builder-vm/vms/*/`, reads each
`{supervisor.pid, gvproxy.pid, stage0.pid}` sidecar, and for each
PID where (a) the process is alive AND (b) its parent is launchd
(PID 1) AND (c) no live `mvmctl dev` claims that VM dir → `kill
-TERM`, then `rm -rf` the dir. Idempotent. Respects the
stage0.lock contract (per memory
`[[project_stage0_audit_and_cache_prune_contract]]`).

**Reference implementation.** `/tmp/plan95/reap.sh` from the Plan 95
validation session has the algorithm correct except for an over-
eager `rm -rf` on dirs whose PIDs are still alive. The Rust port
should check liveness before deletion.

### FU-2 — Process-group isolation in spawn path

**Problem.** Even with FU-1, orphans only get cleaned at the next
`cache prune`. Better: never produce them in the first place.

**Proposed shape.** In `LibkrunBuilderVm::run_stage0` and
`run_build` (and the steady-state supervisor spawn), wrap the
`Command::spawn` with the `command-group` crate (or call
`setpgid`/`setsid` directly) so supervisor and its children
share a fresh process group. On mvmctl's `Drop` and signal
handlers (SIGINT/SIGTERM), send `kill(-pgid, SIGTERM)` to drop
the whole tree. Doesn't help on SIGKILL (Rust can't intercept
that), but covers ^C and clean exit — the common case. FU-1
remains the safety net for the crash case.

### FU-3 (nice to have) — kqueue parent-death watchdog

**Problem.** SIGKILL on mvmctl still strands children even with
FU-2.

**Proposed shape.** The supervisor binary registers a kqueue
`EVFILT_PROC NOTE_EXIT` watcher on its parent's pid at startup
(macOS equivalent of Linux `PR_SET_PDEATHSIG`). If the parent
dies, the supervisor self-terminates and reaps its own gvproxy
child. Provides the "no orphans ever, even on SIGKILL" guarantee.
Heavier engineering (supervisor code change + test for the
failure modes). Defer until FU-1+FU-2 prove insufficient.
