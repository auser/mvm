---
title: "Install mvm on macOS"
description: "mvm on macOS runs natively via libkrun on Hypervisor.framework. No Docker Desktop or host-side Nix required."
---

mvm on macOS uses **libkrun** to boot microVMs on Apple's Hypervisor.framework. Docker Desktop is not required, and mvm does not depend on host-side Nix for `mvmctl` runtime/build commands.

Apple Silicon (M-series) is the primary target. Intel Macs (x86_64) use the same libkrun path where supported.

## Prerequisites

- macOS 13 (Ventura) or newer. macOS 12 might work but isn't tested; macOS 11 lacks the Hypervisor.framework features this path needs.

You **do not need Nix on your Mac**. mvm runs `nix build` inside the project builder VM and extracts the resulting rootfs back to the host. If that builder VM image is missing or broken, `mvmctl` reports that directly; it does not fall back to host Nix.

## Install mvmctl

### One-liner

```bash
curl -fsSL https://raw.githubusercontent.com/tinylabscom/mvm/main/install.sh | sh
```

### Pin a version

```bash
MVM_VERSION=v0.13.0 curl -fsSL https://raw.githubusercontent.com/tinylabscom/mvm/main/install.sh | sh
```

### From source

```bash
git clone https://github.com/tinylabscom/mvm.git
cd mvm
cargo build --release
install -m 0755 target/release/mvmctl ~/.local/bin/mvmctl
```

### From crates.io

```bash
cargo install mvmctl
```

`mvmctl` is a regular Mach-O binary on macOS — no codesigning surprises in the typical install path. Hypervisor.framework requires the host process to hold the `com.apple.security.hypervisor` entitlement; the install script handles ad-hoc signing automatically. If you build from source via `cargo`, the same entitlement is added by the build script.

## Linux builds on macOS — zero-config by default

macOS Nix can't build Linux derivations natively, and most Mac users don't have Nix installed at all. mvm handles both cases **without requiring host-side configuration**: on first `mvmctl build`, mvm resolves the project builder VM image, mounts your project source into it, runs `nix build` inside the VM, and extracts the resulting rootfs back to the host.

The builder VM is the execution boundary for Nix. Host-side Nix can still be useful for editor tooling or direct contributor workflows, but `mvmctl` does not probe `nix` on the host path when building or running microVMs.

### Optional: host-side Nix for power users

Most users skip this section. You may want host-side Nix if you're contributing to mvm itself, want a shared `/nix/store` for your editor's build commands, or already run `nix-darwin` for unrelated reasons. This is not part of the `mvmctl` runtime path.

[Determinate Nix](https://determinate.systems/posts/determinate-nix-installer) is the easiest path:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

If you configure [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary), use it for your own host-side commands; mvm's managed build path continues to run inside the project builder VM.

## Verify

```bash
mvmctl doctor
```

`doctor` reports the active backend and checks Hypervisor.framework + libkrun availability.

## First microVM

```bash
mkdir my-app && cd my-app
mvmctl init
mvmctl run
```

`mvmctl init` scaffolds the project. On first `mvmctl run`, mvm resolves the builder microVM image, runs `nix build` inside it, and boots the resulting rootfs via libkrun. The first build may pay a one-time builder-image fetch on top of VM boot time.

## Troubleshooting

**"Hypervisor.framework: entitlement missing"** — re-codesign the binary with the entitlement: `codesign --entitlements resources/mvmctl.entitlements -f -s - ~/.local/bin/mvmctl`. The release binary ships pre-signed; this only matters if you've stripped entitlements or built from source without the build script's signing step.

**`nix build` fails with "a 'x86_64-linux' with features … is required"** — that is a host-side Nix error. The managed `mvmctl` build path should run inside the builder VM instead; check `mvmctl doctor` for libkrun + Hypervisor.framework status and rebuild/refetch the builder VM image if the builder-side `nix` is missing.

**`mvmctl run` boots but `mvmctl console` fails to attach** — the `console` subcommand is only enabled for *accessible* images. If your `entrypoint.command = [ ... ]`, the build is *sealed* and console attach is rejected. Switch to `entrypoint.shell = "/bin/sh"` or pass `dev = true` in your `mkGuest` call. See [Building MicroVM Images](/guides/building-microvm-images).

**"builder VM is missing nix"** — the builder image is broken or stale. Rebuild/refetch the builder VM image; installing Nix on the macOS host is not the fix.

## Apple Silicon vs Intel notes

- **Apple Silicon (M1/M2/M3/M4)** — Tier 2 microVM isolation via Hypervisor.framework + libkrun. Boot ~500ms cold, ~60ms snapshot-cloned. The supported path.
- **Intel Macs** — Hypervisor.framework works on x86_64 macOS where libkrun supports it. File issues with `mvmctl doctor` output attached.

The Apple Container backend (macOS 26+ Virtualization framework) still exists as a fallback; libkrun is the direct microVM and dev VM backend for mvm workloads on macOS.
