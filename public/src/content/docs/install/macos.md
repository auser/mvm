---
title: "Install mvm on macOS"
description: "mvm on macOS uses native virtualization paths. No Docker Desktop required. Apple Silicon (arm64) is the primary target; Intel Macs work too."
---

mvm on macOS uses native virtualization paths documented in the backend matrix.
Apple Silicon can use Apple Container on macOS 26+ or direct libkrun.
Intel Macs use direct libkrun; Apple Container is not available there.

## Prerequisites

- macOS 13 (Ventura) or newer. macOS 12 might work but isn't tested.

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

## Linux Builds On macOS

macOS Nix can't build Linux derivations natively. mvm routes Linux builds through the project builder VM so the host does not need to build Linux artifacts directly.

### Optional: Host-Side Nix For Contributors

Most users skip this section. You may want host-side Nix if you're contributing to mvm itself, want a shared `/nix/store` for your editor's build commands, or already run `nix-darwin` for unrelated reasons.

[Determinate Nix](https://determinate.systems/posts/determinate-nix-installer) is the easiest path:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

To turn that install into a working Linux builder, configure [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary). mvm's auto-detection will pick it up on the next build.

## Verify

```bash
mvmctl doctor
```

`doctor` reports the active backend and relevant virtualization prerequisites.

## First microVM

```bash
mkdir my-app && cd my-app
mvmctl init
mvmctl run
```

`mvmctl init` scaffolds the project. On first `mvmctl run`, mvm bootstraps the builder microVM if needed, runs `nix build` inside it, and boots the resulting rootfs.

## Troubleshooting

**"Hypervisor.framework: entitlement missing"** — re-codesign the binary with the entitlement: `codesign --entitlements resources/mvmctl.entitlements -f -s - ~/.local/bin/mvmctl`. The release binary ships pre-signed; this only matters if you've stripped entitlements or built from source without the build script's signing step.

**`nix build` fails with "a 'x86_64-linux' with features … is required"** — the macOS host is trying to build Linux derivations directly. Use the project builder VM path, or configure [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary) for editor-side workflows.

**`mvmctl run` boots but `mvmctl console` fails to attach** — the `console` subcommand is only enabled for *accessible* images. If your `entrypoint.command = [ ... ]`, the build is *sealed* and console attach is rejected. Switch to `entrypoint.shell = "/bin/sh"` or pass `dev = true` in your `mkGuest` call. See [Building MicroVM Images](/guides/building-microvm-images).

## Apple Silicon vs Intel notes

- **Apple Silicon (M1/M2/M3/M4)** — Apple Container on macOS 26+ when available; otherwise direct libkrun via Hypervisor.framework.
- **Intel Macs** — direct libkrun via Hypervisor.framework. Install libkrun and verify with `mvmctl doctor`; Apple Container will not be selected.
