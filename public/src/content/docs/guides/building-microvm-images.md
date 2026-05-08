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
| `name` | `string` | Human-readable identifier; baked into the rootfs at `/etc/mvm/name`. |
| `entrypoint` | `attrs` | The boot-time workload. Exactly one of three forms (see below). |
| `services` | `attrs` (optional) | Auxiliary supervised services. Same shape as `entrypoint.services`. |
| `packages` | `[pkg]` (optional) | Extra Nix packages added to the rootfs closure. |
| `hypervisor` | `string` (optional) | Override the default (`firecracker`). |
| `vcpus`, `memory_mib` | `int` (optional) | Resource defaults; `mvm.toml` overrides at run time. |
| `dev` | `bool` (optional) | Explicit accessible-vs-sealed override. Inferred from entrypoint by default. |
| `extraFiles` | `attrs` (optional) | `{ "/abs/path" = { content; mode?; }; }` baked into the rootfs at build time. |

## Entrypoint forms

`entrypoint` declares **exactly one** of:

```nix
# Form 1 — interactive PTY shell (accessible image, dev-friendly)
entrypoint.shell = "/bin/bash";

# Form 2 — single sealed program (production default)
entrypoint.command = [ "/usr/local/bin/serve" "--port" "8080" ];

# Form 3 — supervised multi-service
entrypoint.services = {
  web    = { command = [ "/bin/web" ]; };
  worker = { command = [ "/bin/worker" ]; restart = "always"; };
};
```

## Sealed vs accessible — the same flake works for both

The mvm builder transparently determines whether the resulting image is **sealed** (production — no console attach) or **accessible** (dev — `mvmctl console <vm>` opens an interactive PTY over vsock). The decision is encoded in `passthru.mvm.{accessible, sealed, entrypointKind}` on the resulting derivation, and `mvmctl` reads that metadata to gate the `console` subcommand.

The default inference:

| Entrypoint form | Default mode |
|---|---|
| `entrypoint.shell = …` | **accessible** (`dev = true`) |
| `entrypoint.command = …` | **sealed** (`dev = false`) |
| `entrypoint.services = …` | **sealed** (`dev = false`) |

Override either way with the explicit `dev` field:

```nix
# A shell entrypoint that's still sealed (no console attach allowed)
mkGuest { entrypoint.shell = "/bin/bash"; dev = false; ... }

# A command entrypoint that's accessible for debugging
mkGuest { entrypoint.command = [ "..." ]; dev = true; ... }
```

The same flake source is consumed in **both** dev and production builds — there's no separate "dev flake" the user has to maintain. The difference is purely in the resulting image's metadata + the host-side `console` gate.

The `mkGuest` library composes microvm.nix's `microvm` NixOS module with mvm's security overlay (per-service uids, seccomp tier, dm-verity, read-only `/etc`). You don't see those layers in your flake — they're applied automatically.

> **Boot-time note.** The current `mkGuest` implementation produces a NixOS+systemd rootfs and boots in 1-3 seconds on Firecracker. That misses the project's sub-200ms target by an order of magnitude. The Phase 1 W5.1 rewrite replaces the NixOS path with busybox-as-PID-1 — same user-facing surface, sub-200ms cold boot. See [ADR-013 §"Boot-time budget"](https://github.com/auser/mvm/blob/main/specs/adrs/013-microsandbox-libkrun-microvm-nix-pivot.md) for the per-backend targets.

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
