---
title: "Building MicroVM Images"
description: How to build mvm microVM images from the Nix flake under `nix/`, using the microvm.nix foundation.
---

mvm builds microVM images via [microvm.nix](https://github.com/microvm-nix/microvm.nix) — an MIT-licensed NixOS module that abstracts Firecracker, Cloud Hypervisor, QEMU, crosvm, kvmtool, and stratovirt as a single declarative interface. The choice is recorded in [ADR-013](/contributing/adr/013-microsandbox-pivot/).

## Layout

```
nix/
├── flake.nix                — imports microvm.nix; defines nixosConfigurations
├── flake.lock               — hash-pinned input set; CI re-audits on bump
└── profiles/
    └── minimal.nix          — smallest viable rootfs (boots, has shell, nothing else)
```

Profiles compose the `microvm` NixOS module with mvm's security overlay. As of the Phase 1 W4 wave only `minimal` ships; subsequent profiles (`worker`, `builder`, `ai-sandbox`, `safe-openclaw`, `computer-use`, `repl`) land in later waves per [Plan 60](https://github.com/auser/mvm/blob/main/specs/plans/60-mvm-microsandbox-migration.md).

## Building

The build path runs `nix build` against the flake. On Linux this works out of the box; on macOS you need a Linux builder (either via [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/2.18/installation/upgrading) or a remote `nix-daemon`).

### Build the runner script

```sh
cd nix
nix build .#minimal-runner
./result/bin/microvm-run                 # boots the configured hypervisor
```

The runner script is a thin shell wrapper produced by `microvm.declaredRunner` — it knows the hypervisor (`firecracker` by default) and assembles the artifacts (kernel, initrd, rootfs) on the fly.

### Build the rootfs ext4 directly

For consumers that want the raw rootfs ext4 file (e.g., `mvmctl run --rootfs <path>` paths), evaluate the NixOS configuration and reach into the runner:

```sh
nix build .#nixosConfigurations.minimal.config.microvm.declaredRunner
```

The resulting `result/` symlink contains the ext4 image and kernel artifacts.

### Cross-arch

Both `x86_64-linux` and `aarch64-linux` are declared:

```sh
nix build .#packages.aarch64-linux.minimal-runner    # arm64
nix build .#packages.x86_64-linux.minimal-runner     # x86_64
```

## How `mvmctl` consumes the artifacts

`mvmctl run --hypervisor microsandbox <flake>` translates the rootfs path to a microsandbox-consumable disk image via the **`.ext4 → .raw` hard-link bridge** (see [`crates/mvm-runtime/src/vm/microsandbox.rs::ensure_microsandbox_rootfs_alias`](https://github.com/auser/mvm/blob/main/crates/mvm-runtime/src/vm/microsandbox.rs)). microsandbox's `.disk()` API accepts only `.raw`/`.qcow2`/`.vmdk` extensions; our rootfs is `.ext4`. Hard-linking to a sibling `.raw` (rather than copying) keeps disk usage flat and lets virtio-blk attach with `fstype("ext4")`.

This is the **only** path mvm uses to bridge our Nix-built rootfs into the microsandbox runtime. Per ADR-013's "Non-goal: OCI / container images", we explicitly do not use microsandbox's `RootfsSource::Oci` variant — every rootfs is host-local.

## Validating a flake change

Before committing changes to `nix/flake.nix` or any profile, validate with:

```sh
cd nix
nix flake check --no-build
```

`--no-build` evaluates without actually building the artifacts (faster, doesn't need a Linux builder on macOS). For a full build verification, drop `--no-build`.

CI runs `xtask audit-flake` on every flake.lock bump to verify the pinned `microvm.nix` commit against the supply-chain audit rubric. Bumps without an audit are blocked at the merge gate.

## Adding a new profile

1. Create `nix/profiles/<name>.nix` — a NixOS module that imports `../profiles/minimal.nix` (or composes from scratch) and adds the profile-specific config.
2. Reference it from `nix/flake.nix`'s `nixosConfigurations` map.
3. Run `nix flake check --no-build` from `nix/`.
4. Update the workspace structural tests under `tests/nix_flake_structure.rs` if your profile introduces new invariants worth guarding.

## Fallback

ADR-013 names a fallback path in case a microvm.nix per-bump audit surfaces a security regression we can't accept: revert to the previous iteration's hand-rolled NixOS modules under `mvm/nix/`. The cost is roughly 5K LOC of NixOS-module maintenance returning to scope; the benefit is a smaller trust boundary. The fallback is a named, ready-to-execute escape hatch — not a vague intention.
