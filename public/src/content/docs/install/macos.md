---
title: "Install mvm on macOS"
description: "mvm on macOS runs natively via microsandbox + libkrun on Hypervisor.framework — no Lima, no Docker Desktop. Apple Silicon (arm64) is the primary target; Intel Macs work too."
---

mvm on macOS can use **microsandbox** as the backend — a libkrun wrapper that boots microVMs on Apple's Hypervisor.framework. Source builds keep this dependency-heavy backend behind the `contributor-bootstrap` feature; release builds that include macOS microsandbox support are built with that feature enabled. The choice is recorded in [ADR-013](/contributing/adr/013-microsandbox-pivot/) and ADR-031.

Apple Silicon (M-series) is the primary target. Intel Macs (x86_64) work but the upstream microsandbox path on Intel is less exercised — file an issue if anything misbehaves.

## Prerequisites

- macOS 13 (Ventura) or newer. macOS 12 might work but isn't tested; macOS 11 lacks the Hypervisor.framework features microsandbox needs.

You **do not need Nix on your Mac** when using a build that includes `contributor-bootstrap`. mvm bootstraps a small Linux builder microVM (microsandbox-backed) on first build, runs `nix build` inside it, and extracts the resulting rootfs back to the host. See [§"Linux builds on macOS"](#linux-builds-on-macos--zero-config-by-default) below for the design.

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

To include the microsandbox/libkrun backend in a source build:

```bash
cargo build --release --features contributor-bootstrap
install -m 0755 target/release/mvmctl ~/.local/bin/mvmctl
```

### From crates.io

```bash
cargo install mvmctl
```

`mvmctl` is a regular Mach-O binary on macOS — no codesigning surprises in the typical install path. Hypervisor.framework requires the host process to hold the `com.apple.security.hypervisor` entitlement; the install script handles ad-hoc signing automatically. If you build from source via `cargo`, the same entitlement is added by the build script.

## Linux builds on macOS — zero-config by default

macOS Nix can't build Linux derivations natively, and most Mac users don't have Nix installed at all. mvm handles both cases **without requiring host-side configuration**: on first `mvmctl build`, mvm pulls a small Nix-bearing image, spawns a microsandbox sandbox from it, bind-mounts your project source, runs `nix build` inside, and extracts the resulting rootfs back to the host. See [ADR-013 §"Linux builder via microsandbox"](/contributing/adr/013-microsandbox-pivot/) for the design.

If you already have a host-side Linux builder (e.g., [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary), or a remote `nix-daemon` URL), mvm detects it and uses it instead — the microsandbox-bootstrapped path is the zero-config *default*, not a forced override. When host-side Nix is present, its `/nix/store` is shared into the builder sandbox so cached derivations are reused across both modes.

### Optional: host-side Nix for power users

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

`doctor` reports the active backend; on a stock macOS host you should see `microsandbox` selected. Hypervisor.framework presence + libkrun availability are checked.

## First microVM

```bash
mkdir my-app && cd my-app
mvmctl init
mvmctl run
```

`mvmctl init` scaffolds the project. On first `mvmctl run`, mvm bootstraps the builder microVM (one-time download), runs `nix build` inside it, and boots the resulting rootfs via microsandbox. Expected cold boot: ≤ 500ms (Hypervisor.framework adds ~100ms vs Linux/KVM); the first build pays a one-time builder-image fetch on top of that.

## Troubleshooting

**"Hypervisor.framework: entitlement missing"** — re-codesign the binary with the entitlement: `codesign --entitlements resources/mvmctl.entitlements -f -s - ~/.local/bin/mvmctl`. The release binary ships pre-signed; this only matters if you've stripped entitlements or built from source without the build script's signing step.

**`nix build` fails with "a 'x86_64-linux' with features … is required"** — you've opted into host-side Nix and the macOS host can't build Linux derivations directly. The microsandbox builder bootstrap should pick this up automatically; if it didn't, the bootstrap couldn't start (check `mvmctl doctor` for libkrun + Hypervisor.framework status). As a workaround, configure [`nix-darwin`'s `linux-builder`](https://nix.dev/manual/nix/stable/installation/installing-binary) — mvm will detect and use it.

**`mvmctl run` boots but `mvmctl console` fails to attach** — the `console` subcommand is only enabled for *accessible* images. If your `entrypoint.command = [ ... ]`, the build is *sealed* and console attach is rejected. Switch to `entrypoint.shell = "/bin/sh"` or pass `dev = true` in your `mkGuest` call. See [Building MicroVM Images](/guides/building-microvm-images).

**"microsandbox backend unavailable"** — source builds omit microsandbox unless built with `--features contributor-bootstrap`. Rebuild with that feature if you need the libkrun-backed macOS path.

## Apple Silicon vs Intel notes

- **Apple Silicon (M1/M2/M3/M4)** — Tier 2 microVM isolation via Hypervisor.framework + libkrun. Boot ~500ms cold, ~60ms snapshot-cloned. The supported path.
- **Intel Macs** — Hypervisor.framework still works on x86_64 macOS, but the upstream microsandbox testing is sparser. Expect occasional rough edges; file issues with `mvmctl doctor` output attached.

The Apple Container backend (macOS 26+ Virtualization framework) was previously a Tier 2 fallback in the auto-select ladder. It still exists in the codebase but microsandbox supersedes it on every macOS host that supports both — see ADR-013 §"Backend selection ladder" for the priority.
