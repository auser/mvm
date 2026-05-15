---
title: "Install mvm on macOS"
description: "mvm on macOS supports Apple Silicon through Hypervisor.framework-backed local builder/runtime paths. Intel Macs are not a supported local microVM host."
---

mvm on macOS is supported on **Apple Silicon (M-series)**. The local builder/runtime path uses Apple's Hypervisor.framework via Apple Container and libkrun-backed components. No Docker Desktop is required for the supported path.

Intel Macs are not a supported local microVM host. Use a Linux machine with `/dev/kvm` or a remote Linux builder/runtime if you need first-class isolation from Intel macOS.

## Prerequisites

- Apple Silicon Mac.
- macOS 26+ for the Apple Container dev VM path.
- libkrun installed for libkrun-backed builder/runtime components.

You **do not need Nix on your Mac**. You run `mvmctl build` from macOS, and mvm runs Nix evaluation and `nix build` inside the Linux builder VM, then extracts the resulting rootfs back to the host. See [§"Linux builds on macOS"](#linux-builds-on-macos--zero-config-by-default) below for the design.

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

`doctor` reports the active backend and libkrun availability. On an Apple Silicon Mac with macOS 26+, the dev path uses Apple Container and source image builds use the libkrun-backed builder VM.

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

**"libkrun shared library not found"** — install libkrun, then rerun the command. On Apple Silicon with Homebrew:

```bash
brew install libkrun
```

## Apple Silicon vs Intel notes

- **Apple Silicon (M1/M2/M3/M4 and newer)** — supported local path. Apple Container covers the dev VM, and libkrun backs builder/runtime components that need Hypervisor.framework directly.
- **Intel Macs** — unsupported for the local microVM path. Run mvm on a Linux KVM host, or use future remote/Windows-style builder work when it lands.

The Apple Container backend requires Apple Silicon and macOS 26+. libkrun is also treated as an Apple Silicon macOS path for mvm support purposes.
