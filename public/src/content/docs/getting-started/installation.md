---
title: Installation
description: Install mvmctl on macOS or Linux.
---

## One-Liner

```bash
curl -fsSL https://raw.githubusercontent.com/tinylabscom/mvm/main/install.sh | sh
```

## Pin a Version

```bash
MVM_VERSION=v0.7.0 curl -fsSL https://raw.githubusercontent.com/tinylabscom/mvm/main/install.sh | sh
```

## From Source

```bash
git clone https://github.com/tinylabscom/mvm.git
cd mvm
cargo build --release
cp target/release/mvmctl ~/.local/bin/
```

## Cargo Install

```bash
cargo install mvmctl
```

## Self-Update

```bash
mvmctl update
```

## Prerequisites

- **macOS** (Apple Silicon or Intel) or **Linux** (x86_64 or aarch64)
- [Homebrew](https://brew.sh/) (macOS only -- mvmctl will install it if missing)

### Backend Auto-Detection

mvmctl automatically detects your platform at startup and selects the best VM backend:

| Platform | Backend | What happens |
|----------|---------|-------------|
| **Linux with `/dev/kvm`** | Firecracker | Runs directly on KVM. Smallest attack surface, fastest cold boot. |
| **macOS** (Apple Silicon / Intel) | microsandbox (libkrun) | Native Hypervisor.framework. No Lima, no Docker Desktop. |
| **Linux without `/dev/kvm`** | microsandbox | Software-emulation fallback (slower; meant for CI runners). |
| **Docker available** | Docker | Tier 3 container fallback. Used only if no hypervisor backend works. |

You don't need Nix on the host. On first build, mvm bootstraps a Linux builder microVM, runs `nix build` inside it, and extracts the rootfs back. Host-side Nix is detected and used when present; otherwise the builder VM handles it.

### First-Time Setup

After installation, run the setup wizard:

```bash
mvmctl init
```

This walks through platform detection, dependency installation (Firecracker on Linux, libkrun via microsandbox on macOS), default network setup, and XDG directory creation. Use `--non-interactive` for scripted environments.

Running `mvmctl dev` or `mvmctl bootstrap` also handles setup automatically -- they detect your platform, select the backend, and stage the builder microVM image on first use.

You can force a specific backend with `--hypervisor`:

```bash
mvmctl up --flake . --hypervisor microsandbox
mvmctl up --flake . --hypervisor firecracker
mvmctl up --flake . --hypervisor docker
mvmctl up --flake . --hypervisor qemu    # microvm.nix
```

Use `mvmctl doctor` to check which backends are available on your system.
