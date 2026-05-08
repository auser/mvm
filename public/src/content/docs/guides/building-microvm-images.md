---
title: "Building MicroVM Images"
description: How to build mvm microVM images from your own project — the mvm repository is a library, not a place to put your code.
---

mvm is a **library**, not a project to fork. You keep your code, your `flake.nix`, and your `mvm.toml` in your own repository, and `mvmctl` builds your microVM image by running `nix build` against your flake. **You should never need to edit anything inside the mvm repository.**

Under the hood, mvm wraps [microvm.nix](https://github.com/microvm-nix/microvm.nix) (MIT) — that's the NixOS module that abstracts Firecracker, Cloud Hypervisor, QEMU, crosvm, kvmtool, and stratovirt. The choice is recorded in [ADR-013](/contributing/adr/013-microsandbox-pivot/).

## The two files in your project

Every mvm project has a `mvm.toml` and a `flake.nix`:

```toml
# my-app/mvm.toml
flake     = "."
profile   = "default"
vcpus     = 1
memory_mib = 256
```

```nix
# my-app/flake.nix
{
  description = "my microVM app";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    mvm.url     = "github:auser/mvm";
  };

  outputs = { self, nixpkgs, mvm, ... }: {
    packages.x86_64-linux.default = mvm.lib.x86_64-linux.mkGuest {
      name = "my-app";
      services.web = {
        command = [ "/usr/local/bin/web" ];
      };
    };
  };
}
```

That's the whole user-side surface. `mvmctl build` reads `mvm.toml`, follows `flake = "."` to your flake, and runs `nix build` against it.

## Building

From your project directory:

```sh
mvmctl build              # reads mvm.toml; builds the named flake target
mvmctl run                # builds (if needed) + boots
```

`mvmctl` selects the backend automatically (Firecracker on Linux+KVM, microsandbox on macOS / Linux without KVM). Override with `--hypervisor microsandbox` if you want to force the cross-platform path.

If you want to drive `nix build` directly without `mvmctl` in the loop:

```sh
nix build .#default
```

## What `mkGuest` accepts

`mvm.lib.<system>.mkGuest { … }` takes a single attribute set:

| Field | Type | Purpose |
|---|---|---|
| `name` | `string` | A human-readable identifier for the image. |
| `services` | `attrs` | Map of service name → `{ command, restart?, env? }`. Each service runs at boot under its own uid + seccomp tier. |
| `packages` | `[pkg]` | Extra Nix packages to add to the rootfs closure. |
| `hypervisor` | `string` (optional) | Override the default hypervisor (`firecracker` on Linux, `microsandbox` elsewhere). |
| `extraFiles` | `attrs` (optional) | Map of in-guest path → host source path or contents. |

The `mkGuest` library composes microvm.nix's `microvm` NixOS module with mvm's security overlay (per-service uids, seccomp tier, dm-verity, read-only `/etc`). You don't see those layers in your flake — they're applied automatically.

> **Note (Phase 1):** the `mkGuest` library is being ported from the previous iteration of mvm in Phase 1 W5+. Until that wave lands, calling `mkGuest` emits a clear error message pointing you at the microvm.nix module directly. The user-facing flake shape above is final and won't change when the implementation fills in.

## What's inside the mvm repository (and why you don't touch it)

The repository's `nix/` directory contains:

- `nix/flake.nix` — exposes `lib.<system>.mkGuest` for your flake to consume.
- `nix/profiles/minimal.nix` — an **internal** test fixture used by mvm's own smoke tests (`tests/smoke_microsandbox.rs`, `tests/nix_flake_structure.rs`). Not a starter template.

The internal fixture lives under the `internal-` namespace in flake outputs (`nixosConfigurations.internal-minimal-…`, `packages.<system>.internal-minimal-runner`) so the boundary is mechanical: anything `internal-*` is for mvm developers, not for users.

## Validating a change to your flake

```sh
cd my-app
nix flake check --no-build
```

`mvmctl validate` does the same with extra `mvm.toml` checks layered on.

## Cross-platform notes

- **Linux**: Nix builds natively against `/dev/kvm`. Firecracker is the default backend.
- **macOS**: Nix builds need a Linux builder — either [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary) or a remote `nix-daemon`. Once the build succeeds, microsandbox (libkrun-backed) runs the resulting microVM directly on Hypervisor.framework — no Lima hop. See [ADR-013](/contributing/adr/013-microsandbox-pivot/).
- **Windows**: Tauri-only (the `mvm-studio` desktop app packages a WSL2-backed builder + runtime). See [ADR-031](https://github.com/auser/mvm/blob/main/specs/adrs/031-cross-platform-strategy.md).

## Why no OCI

mvm is microVMs, not containers. Even though the underlying microsandbox library exposes OCI image pulls (`RootfsSource::Oci`), mvm uses **only** the host-local disk-image path. The bridge between your Nix-built `.ext4` rootfs and the runtime is a sibling `.raw` hard-link with `fstype("ext4")` — no registry, no auth, no pull cache, fully offline-by-default once your rootfs is built. ADR-013 §"Non-goal: OCI / container images" carries the full rationale.
