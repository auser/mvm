# Plan 72 — Builder VM via libkrun (drop libkrun from the build path)

> Status: proposed
> Owner: TBD
> Started: —
> Depends on: plan 57 (libkrun spike — boot + vsock + console viability)
> Implements: ADR-046 (move the builder VM off libkrun onto libkrun + firecracker)
> Tracking: `mvm_build::builder_vm::LibkrunBuilderVm` → `LibkrunBuilderVm`; `mvmctl dev up` failure with `error: writing to file: No space left on device`

## Why

`mvmctl dev up` on macOS goes through `mvm_cli::commands::env::apple_container::ensure_dev_image → build_image_via_libkrun → LibkrunBuilderVm::run_build`. The Nix build closure for the dev image (rustc + ~480 cargo crate derivations from cache.nixos.org) overflows libkrun's hardcoded 4 GiB writable overlay (`libkrun-image-0.4.5/lib/ext4/mod.rs:25`). The library exposes no knob to widen it. Workarounds (bind-mount host `/nix`, named volumes, volume seeding) either shadow the OCI image's `/bin/sh` or require multi-pass dances that pay back none of the structural cost.

ADR-046 settled the strategy: replace `LibkrunBuilderVm` with `LibkrunBuilderVm` — direct libkrun (macOS) / firecracker (Linux), virtio-fs for the workspace + artifact mounts, virtio-blk for a persistent `/nix` store. This plan sequences that work into shippable phases, each with green-bar acceptance criteria, so the campaign can be merged incrementally without a single overwhelming PR.

If plan 57 stalls, **W0 still ships standalone** — the three sandbox-correctness fixes (workspace-at-`/work`, `MVM_WORKSPACE_PATH`, `git config --global` before cd) are correct under any launcher and unblock dev-up for non-worktree checkouts on hosts with enough disk pressure to dodge the overlay limit. Vendoring libkrun with a `upper_size_mib` knob is the named fallback for W1+.

## Prerequisites

- Plan 57 W1–W3 landed: `mvm_libkrun::start_with_config` boots a Linux kernel + ext4 rootfs on macOS Apple Silicon with vsock, console redirected to host stdout, and clean power-off detection. Linux KVM path doesn't have to be wired yet — Firecracker covers Linux for the duration of this plan.
- `mvm_libkrun::Error::NotYetWired` removed (plan 57 acceptance criterion).
- A macOS Apple Silicon laptop with Xcode CLI tools + Homebrew + `brew install libkrun`. Plan 72 W0–W4 develop against this; CI matrix adds Linux KVM in W5.

## Acceptance criteria (whole-plan)

`mvmctl dev up` on a stock macOS Apple Silicon host with no host Nix:

1. Downloads the published builder VM image (kernel + rootfs.ext4) from the matching GitHub release, hash-verified per ADR-002 §W5.1.
2. Launches it via libkrun, attaching the worktree at `/work` (read-only), an artifact dir at `/out`, a persistent virtio-blk Nix store at `/nix`.
3. Runs `nix build path:/work/nix/images/builder#packages.aarch64-linux.default --no-link --print-out-paths --no-write-lock-file --impure` inside.
4. Produces `vmlinux` + `rootfs.ext4` in the artifact dir.
5. Exits 0 with no manual intervention.
6. Cold cache: completes in <8 min on a sustained 100 Mb/s connection.
7. Warm cache (`/nix` virtio-blk preserved from a prior run): rebuilds unchanged dev image in <30 s.
8. `cargo metadata --features ''` shows no `libkrun*` crate in the default-feature closure of `mvmctl`.
9. CI passes the workspace + clippy + every plan-71-introduced live test on macOS aarch64 and Linux x86_64.

When all nine hold, ADR-046 flips from Proposed to Accepted and the plan moves to `specs/backlog/`.

---

## W0 — Sandbox-correctness fixes (independent; ship now)

> These three changes are correct regardless of which launcher we use. They unblock `mvmctl dev up` on hosts that don't hit the overlay-size ceiling (linear `nix build` runs against the existing libkrun path can complete on hosts with very large overlays, or when the upstream PR for `upper_size_mib` lands). They also become load-bearing for W3 once the libkrun path is wired.

### W0.1 — Mount the workspace at `/work`, not the flake dir

`crates/mvm-cli/src/commands/env/apple_container.rs::build_image_via_libkrun` derives `workspace_root = flake_dir.parent().parent().parent()` and sets:

- `BuilderMounts.flake_src = workspace_root`
- `BuilderJob.flake_ref = format!("path:{}/{}", GUEST_WORK_DIR, flake_rel)` where `flake_rel = flake_src.strip_prefix(workspace_root)`

Drops the previous shape where only `nix/images/builder/` was mounted at `/work` and `path = ../../..` in the flake resolved to `/` of the sandbox, hitting `/.msb/agent.sock` (or its libkrun equivalent — the sandbox internals will differ but `..` escaping the mount always hits *something*).

### W0.2 — Pass `MVM_WORKSPACE_PATH` to the flake's `builtins.path`

`nix/images/builder/flake.nix` reads `builtins.getEnv "MVM_WORKSPACE_PATH"` under `--impure` and uses that absolute path for `builtins.path { path = ...; }` instead of `../../..`. The relative form still works outside the sandbox (host `nix build` from a contributor's machine), so the env-var path is opt-in.

The host-side build script in `crates/mvm-build/src/builder_vm.rs:543` adds `export MVM_WORKSPACE_PATH=/work` before `nix build`.

Reason: `path:` URL semantics store-copy the flake dir; `../../..` resolves against the store path, not the host mount. Without the env var override, the workspace import lands outside the mount and trips on sandbox-internal files (`/.msb/agent.sock` on libkrun, virtio-fs metadata files on libkrun).

### W0.3 — `git config --global` before `cd /work`

Same file, build_script. Reorder so `git config --global --add safe.directory '*'` runs at sandbox cwd `/` (no `.git` to discover), then `cd /work` after. In a git-worktree workspace, `/work/.git` is a *file* whose `gitdir:` redirect points to a host path that doesn't exist in the sandbox. Modern git (2.35+) walks cwd upward at startup for repo discovery; if it finds a broken pointer, the whole git invocation fails with exit 128 *before* `--global` is processed.

### W0.4 — Nix filter on workspace import

`nix/images/builder/flake.nix::workspace` adds a `filter` to `builtins.path` excluding `target`, `.git`, `result`, `result-*`, `node_modules`, `.direnv`, `.cargo`, `.claude`, `.worktrees`. Without this, mounting the full workspace at `/work` makes `builtins.path` hash and store-copy multi-GB build output on every evaluation. Filter is conservative (only known-bulky dirs) so we don't accidentally exclude something the build needs.

### W0 acceptance

- `mvmctl dev up` on a non-worktree checkout (regular `.git` directory) completes the flake-evaluation phase. The 4 GiB overlay still bites for the actual build, but the error is now "disk full" not "/.msb/agent.sock has unsupported type" — confirming W0 fixes the path-resolution problem cleanly.
- New live test `crates/mvm-build/tests/builder_vm_path_resolution.rs` asserts that `build_image_via_libkrun` (with the libkrun feature on) constructs a `BuilderJob` whose `flake_ref` starts with `path:/work/` and a `BuilderMounts` whose `flake_src` equals the workspace root.

### W0 ships as

A single PR. No new dependencies. No feature flag changes. The three fixes already exist on `feat/builder-recovery` as of 2026-05-13; W0 is "tidy and merge that branch."

---

## W1 — `LibkrunBuilderVm` skeleton

New file `crates/mvm-build/src/libkrun_builder.rs` with a struct `LibkrunBuilderVm` implementing the existing `BuilderVm` trait from `crates/mvm-build/src/builder_vm.rs`. Same `BuilderJob` / `BuilderMounts` / `BuilderArtifacts` types — the contract that `mvm-cli` consumes doesn't change.

### Interface

```rust
pub struct LibkrunBuilderVm {
    pub vcpus: u8,         // default 4
    pub memory_mib: u32,   // default 4096
    pub nix_store_mib: u32, // default 65536 (64 GiB sparse)
}

impl BuilderVm for LibkrunBuilderVm {
    fn host_can_build(&self) -> Result<bool, BuilderVmError> { ... }
    fn run_build(&self, job: &BuilderJob, mounts: &BuilderMounts)
        -> Result<BuilderArtifacts, BuilderVmError> { ... }
}
```

### Inner shape

`run_build` does, in order:

1. Resolve the builder VM image (kernel + rootfs.ext4) — call `ensure_builder_vm_image()` (new in `mvm-cli`, mirroring `download_dev_image`). On macOS without a published prebuilt, fall back to the libkrun path with a loud warning (deprecation window).
2. Allocate / locate the persistent `/nix` store image at `~/.cache/mvm/builder-vm/nix-store-<arch>.img` (sparse, default 64 GiB). First-build cost: format ext4; subsequent builds reuse.
3. Stage the job dir at `~/.cache/mvm/builder-vm/jobs/<job-id>/`:
   - `cmd.sh` — the build script (same body as today's `build_script` in `builder_vm.rs:543`, just without the libkrun framing).
   - `env` — KV env vars one-per-line.
   - `result` — empty; the guest writes here on completion.
4. Launch via `mvm_libkrun::start_with_config(...)` with:
   - kernel: `<image>/vmlinux`
   - initrd: none (rootfs has init at `/sbin/mvm-builder-init`)
   - rootfs: `<image>/rootfs.ext4` (read-only)
   - virtio-fs mounts: `mounts.flake_src → /work` (RO), `mounts.artifact_out → /out` (RW), `job_dir → /job` (RW)
   - virtio-blk: `nix-store-<arch>.img → /dev/vdb` (RW)
   - vsock: yes (console redirect)
   - net: virtio-net with TAP (Linux) / `vmnet` (macOS), DNS resolver from host
5. Poll for power-off (libkrun event or vsock end-of-stream); time out at 30 min wall clock.
6. Read `/job/result` for the exit code; tail of stdout/stderr is captured via vsock and echoed to mvmctl's stdout as the build runs.
7. Validate `mounts.artifact_out` contains `vmlinux` + `rootfs.ext4`; return `BuilderArtifacts`.

### Feature gate

`builder-vm` (new). Default-off in this phase.

### W1 acceptance

- `LibkrunBuilderVm` compiles against the plan-57 libkrun bindings with no `NotYetWired` returns from any path that this plan exercises.
- Unit tests for `BuilderMounts` validation (paths exist, non-UTF-8 rejection, job-id collision) mirror the existing libkrun suite.
- No call site switches over yet — pure scaffolding behind the new feature flag.

---

## W2 — Builder VM image (kernel + rootfs)

The artifact `LibkrunBuilderVm` boots into. Single flake, two outputs (aarch64-linux, x86_64-linux), built by CI on a Linux runner.

### Inputs

- Existing `nix/images/builder/flake.nix` is the starting point. Rename to `nix/images/builder-vm/` to disambiguate from "the dev VM image users run."
- `mkGuest` from `nix/lib/` produces the rootfs (already in use).
- Add the `mvm-builder-init` package (W3) to `builderPackages`.
- Strip what isn't needed for the build VM: no `iptables`, no `procps` interactive bits, no `less`. Just `bash`, `coreutils`, `gnugrep`, `gnused`, `gawk`, `findutils`, `nix`, `git`, `curl`, `jq`, `gnumake`, `e2fsprogs`, `util-linux`, `mvm-builder-init`.

### Outputs

`packages.<system>.default` produces `$out/{vmlinux,rootfs.ext4,manifest.json}`. The manifest gets SHA-256s + sizes — feeds the existing `*-checksums-sha256.txt` release artifact (`mvm_cli::commands::env::apple_container::download_dev_image` extends to also verify `download_builder_vm_image`).

### Kernel cmdline

Embedded in the image (no per-build customisation needed):

```
console=hvc0 root=/dev/vda ro rootfstype=ext4 init=/sbin/mvm-builder-init
```

`/dev/vda` is the rootfs virtio-blk. `/dev/vdb` is the persistent /nix virtio-blk (mounted by `mvm-builder-init`).

### Release pipeline

`.github/workflows/release.yml` grows a `builder-vm-image` job paralleling the existing `dev-image` job:

- Matrix: aarch64-linux, x86_64-linux.
- Runs `nix build .#packages.<system>.default` in `nix/images/builder-vm/`.
- Uploads `builder-vmlinux-<arch>` + `builder-rootfs-<arch>.ext4` + appended entries in `builder-checksums-sha256.txt`.

### W2 acceptance

- Local `nix build nix/images/builder-vm#packages.aarch64-linux.default` on a Linux runner produces a working kernel + rootfs.
- Booting the rootfs under qemu-system-aarch64 (CI smoke) lands at `mvm-builder-init` and powers off after running a no-op `/job/cmd.sh`.
- Release workflow uploads the artifacts; manifest verifies.
- Image size budget: rootfs.ext4 ≤ 300 MiB compressed, ≤ 1.2 GiB uncompressed. Hard fail in CI if exceeded.

---

## W3 — `mvm-builder-init`

Tiny in-guest init that does the job-execution dance. Two implementation options were debated in ADR-046 §"Open questions"; this plan picks **Rust**, single binary, statically linked, no runtime deps.

### Source location

`crates/mvm-builder-init/` (new crate). Linux-only (`#![cfg(target_os = "linux")]`). Built via mkGuest's `buildPackages.mvm-builder-init`.

### Behaviour

```rust
fn main() -> ! {
    // 1. mount essentials
    mount("proc", "/proc", "proc", 0, None);
    mount("sysfs", "/sys", "sysfs", 0, None);
    mount("devtmpfs", "/dev", "devtmpfs", 0, None);
    mount("tmpfs", "/tmp", "tmpfs", 0, None);

    // 2. fsck + mount the persistent /nix store (virtio-blk at /dev/vdb)
    if !nix_store_initialized("/dev/vdb") {
        format_ext4("/dev/vdb");
    }
    mount("/dev/vdb", "/nix-store", "ext4", 0, None);
    bind_mount("/nix-store", "/nix"); // shadow the rootfs's /nix

    // 3. network up (DHCP via udhcpc on eth0)
    let _ = run("udhcpc", &["-i", "eth0", "-n", "-q"]);

    // 4. execute the job
    let cmd_sh = "/job/cmd.sh";
    if !exists(cmd_sh) {
        write_result(2, "no /job/cmd.sh");
        poweroff();
    }
    let status = run("/bin/sh", &["-eu", cmd_sh]);
    write_result(status.code, status.stderr_tail);
    poweroff();
}
```

### Why bind-mount, not overlay

The rootfs ships with a minimal seed `/nix/store` (the closure of bash + coreutils + nix + the builder image's own dependencies — ~150 MiB). Bind-mounting `/nix-store` over `/nix` is the cleanest cross-build persistence story: the seed lives in the rootfs (read-only, regenerated on each builder VM image release), and writes go to the host-backed virtio-blk. Nix's `/nix/var/nix/db/db.sqlite` lives on the virtio-blk too, so the daemon's record of what's been built persists.

First-run on a fresh `/dev/vdb`: `mvm-builder-init` formats it ext4, copies the seed `/nix` contents into it (one-time cost ~15 s on Apple Silicon), then bind-mounts.

Subsequent runs: skip the format + copy; just mount + bind.

### W3 acceptance

- `cargo build -p mvm-builder-init --release` produces a ≤ 1.5 MiB static binary on aarch64-linux.
- Booted in qemu with a 5 MiB virtio-blk for `/dev/vdb` + a `/job/cmd.sh` that runs `echo ok > /out/result.txt`, the init mounts, runs, writes, and powers off. Boot-to-poweroff ≤ 8 s.
- Negative path: missing `/job/cmd.sh` → exit code 2 in `/job/result`, no panic.
- Negative path: cmd.sh non-zero exit → exit code captured + stderr tail written.

---

## W4 — Network, vsock, console plumbing

### Network

The builder needs cache.nixos.org. Default to a host-bridged virtio-net interface; `mvm-builder-init` runs `udhcpc` to bring it up.

- macOS: `vmnet` shared mode (no admin prompts in current libkrun crate? — plan 57 verifies in W4).
- Linux: TAP via `mvm-network` (the existing dev-network code in `crates/mvm/src/vm/network.rs`).

Host's DNS resolver list passed in via kernel cmdline or `/job/resolv.conf` bind-mount (TBD; plan 57's network spike informs the choice).

Air-gapped mode: `LibkrunBuilderVm::with_offline()` skips bringing up eth0 and sets `NIX_CONFIG="substituters ="` in the job env so Nix builds everything from source against the seed `/nix`. Not part of the W4 acceptance criteria; tracked for follow-up.

### vsock

Plan 57's vsock spike covers this — host-side reader on a known port pulls stdout/stderr lines as they happen and prints them.

`mvmctl` UI shows live build progress (every line nix emits about "building '/nix/store/..../foo.drv'") rather than the current libkrun-path silence-then-dump.

### Console

libkrun console wired to host stdout via vsock — same as plan 57. No additional plumbing.

### W4 acceptance

- Builder VM resolves `cache.nixos.org` (network smoke).
- Live stdout streaming visible in `mvmctl dev up` output. No 5-minute silent block before the dump-on-failure.
- Air-gapped variant builds the minimal dev image entirely from the seed `/nix` (proves the seed closure is complete).

---

## W5 — Cutover

Reshape `ensure_dev_image` to encode the two-layer artifact rule from ADR-046 §"Two artifact layers, two acquisition paths". The function's signature stays `(flake_dir, out_dir) -> Result<(kernel, rootfs)>`; its internal control flow is rewritten.

### Acquisition rule, in order of preference

`ensure_dev_image` now resolves both layers explicitly:

**Layer 1 — builder VM image** (the thing that runs `nix build`):

1. **In a source checkout** (`find_builder_vm_flake()` returns `Some`):
   - Hash `nix/images/builder-vm/flake.nix` + its lock to derive a cache key.
   - If `~/.cache/mvm/builder-vm/<hash>/` exists and is hash-valid → use it.
  - Otherwise download the mvm-published prebuilt for the running `mvmctl` version; SHA-256 verify per ADR-002 §W5.1; cache it locally.
2. **Outside a source checkout** (installed binary, no flake): download the mvm-published prebuilt for the running `mvmctl` version; SHA-256 verify per ADR-002 §W5.1; cache at `~/.cache/mvm/builder-vm/v<mvmctl-version>/`.

**Layer 2 — the user's target image** (dev shell, or whatever `--flake` points at):

3. **In a source checkout with a local flake**: always build locally using the Layer 1 builder VM. Cache at `~/.cache/mvm/dev/<flake-narHash>/`. **No prebuilt download ever happens for Layer 2 in a source checkout.**
4. **Outside a source checkout, no `--flake`**: download the published default-tenant prebuilt for the running version. Same hash-verification path.
5. **Outside a source checkout, with `--flake`**: build via Layer 1 from the user-supplied flake.

### Feature flag polarity

- `builder-vm` becomes the new default — covers Layer 1 + Layer 2 on user-facing paths.
- The old libkrun bootstrap flag is removed. End users without libkrun get an install hint, not a libkrun fallback.

### Implementation notes for `ensure_dev_image`

- `find_builder_vm_flake()` is new — sibling of `find_dev_image_flake()`. Returns `Ok(path)` when `<workspace_root>/nix/images/builder-vm/flake.nix` exists. The split mirrors the artifact-layer distinction: `nix/images/builder-vm/` is Layer 1, `nix/images/dev-shell/` (renamed from `nix/images/builder/`) is Layer 2.
- The cache-key hash is `sha256(flake.nix + flake.lock)`. Any contributor edit to either invalidates and forces a rebuild — that's the "next `mvmctl dev up` reflects my edit" promise.
- Libkrun is absent from the dependency graph.

### W5 acceptance

- All nine whole-plan acceptance criteria pass.
- `cargo test --workspace` is green on macOS aarch64 + Linux x86_64 with default features.
- Live test `tests/dev_up_libkrun.rs` runs the full Layer 1 + Layer 2 source-checkout path end-to-end against a tiny test flake (≤ 5 derivations) on both platforms.
- A contributor edit to `nix/images/builder-vm/flake.nix` (e.g. adding `htop` to `builderPackages`) shows up in the next `mvmctl dev up` without a release-pipeline round-trip.

---

## W6 — Hygiene

W5's cutover deletes the libkrun fallback. W6 is hygiene that lands once W5 has shipped without contributor-reported regressions:

- Update ADR-013 with a §"Superseded for the user-facing builder path by ADR-046" note.
- Update `CLAUDE.md`'s build instructions: `cargo build` is the canonical path.

### Removing libkrun entirely

Libkrun is removed from the Cargo dependency graph and runtime/backend surface.

### W6 acceptance

- `cargo metadata -p mvmctl` with default features shows no `libkrun*` crate.
- Default `cargo build` bin size: `du -h target/release/mvmctl` drops by at least 8 MiB vs. pre-plan-71 baseline.
- Release artifact `builder-checksums-sha256.txt` exists and is verified by `download_builder_vm_image`.

---

## Non-goals

- **Replacing libkrun as a runtime backend.** ADR-013's runtime selection (Firecracker on Linux, libkrun on macOS via plan 57) stays as-is. libkrun stays available as a runtime backend selectable via `--hypervisor libkrun` during the deprecation window.
- **Multi-platform Windows support.** Windows is out of scope for this plan; libkrun doesn't run there. The libkrun builder path is the only one that ever could have worked on Windows, and even then via WSL2.
- **Snapshotting the builder VM**. The builder is single-purpose and exit-on-completion. No reason to snapshot. If a contributor wants a long-lived dev VM they want `mvmctl dev shell` (different path, libkrun-backed via plan 57).
- **Honoring host Nix when present.** Per CLAUDE.md §"Host Nix is never used by mvmctl", mvmctl does not consult any host Nix install in any code path. This is a *removal* of ADR-013's "host Nix remains an opt-in power-user path" clause — that clause is superseded for everything inside `mvmctl`. Contributors with `nix-darwin`'s `linux-builder` see exactly the same behavior as contributors without it. Determinism is the reason.
- **Forcing contributors to download a prebuilt builder VM in a source checkout.** Source-checkout `mvmctl dev up` always runs `nix build` against the in-repo flakes for both Layer 1 (builder VM image) and Layer 2 (dev shell / user image). The mvm-published prebuilt is end-user infrastructure only — a contributor modifying `nix/images/builder-vm/flake.nix` must see their changes in the next invocation without any release-pipeline round-trip.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Plan 57 doesn't reach W1 acceptance before plan 72 W1 wants to start. | Vendoring fallback in ADR-046 §"Fallback / escape hatch" — fork libkrun, add `upper_size_mib`, ship that as v0.15.x while plan 57 catches up. |
| The published builder VM image takes >1.2 GiB compressed and bandwidth-constrained users complain. | Audit the rootfs closure in W2; trim packages until budget is met. `du -h $out/rootfs.ext4` is a CI gate. |
| Cold-cache `nix build` doesn't fit the <8 min budget. | Ship the builder VM image with a *bigger* seed `/nix/store` containing the most expensive paths (rustc closure, nixpkgs stdenv). Tradeoff: bigger image, faster cold-cache. Adjust in W2 based on measurement. |
| Bind-mount `/nix-store → /nix` interacts badly with the rootfs's existing `/nix/store/...-bash` lookups during init. | Mount order: tmpfs `/tmp`, then bind `/nix-store → /nix`, *then* exec `/bin/sh` for `/job/cmd.sh`. By the time the shell runs, `/nix` resolves to the persistent store with the seed copied in. The init binary itself uses no `/nix` paths (it's statically linked). |
| Worktree workspaces vs. main checkouts — different `.git` semantics. | W0.3 handles this; the order of `git config --global` before `cd` is correct for both. |

## Test matrix

| Test | Scope | Lives in | Runs on |
|---|---|---|---|
| `builder_vm_path_resolution.rs` | W0 — flake_ref and workspace_root computation | `crates/mvm-build/tests/` | Every PR (unit) |
| `dev_up_libkrun.rs` | W1–W4 — end-to-end builder VM run against a 5-derivation tiny flake | `crates/mvm-cli/tests/` | macOS aarch64 + Linux x86_64 CI |
| `builder_init_smoke.rs` | W3 — qemu-system-aarch64 boot, run, poweroff | `crates/mvm-builder-init/tests/` | Linux CI |
| `dev_up_full.rs` (the existing dev image build) | W5 — full path against the real `nix/images/builder-vm` flake | `crates/mvm-cli/tests/` | Release CI only (slow) |
| Bin-size regression | W6 — `target/release/mvmctl` size delta | `xtask check-bin-size` | Every release-candidate |

## Sequencing summary

```
W0 (correctness fixes, ships now, libkrun stays the launcher)
   │
   ▼ (plan 57 lands W1–W3)
W1 (LibkrunBuilderVm skeleton — gated, no callers)
   │
   ▼
W2 (Builder VM image — flake + CI workflow)
   │
   ▼
W3 (mvm-builder-init — Rust binary baked into rootfs)
   │
   ▼
W4 (Network + vsock + console plumbing)
   │
   ▼
W5 (Cutover — libkrun for user-facing, libkrun removed)
   │
   ▼ (≥ 8 weeks; no regressions)
W6 (Hygiene — doc the new build matrix)
```

W0 ships standalone. W1–W4 chain. W5 is the user-visible cutover. W6 is hygiene.
