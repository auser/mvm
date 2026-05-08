---
title: "Install mvm on macOS"
description: "mvm on macOS runs natively via microsandbox + libkrun on Hypervisor.framework — no Lima, no Docker Desktop. Apple Silicon (arm64) is the primary target; Intel Macs work too."
---

mvm on macOS uses **microsandbox** as the backend — a libkrun wrapper that boots microVMs on Apple's Hypervisor.framework. There's no Lima VM in the loop, no Docker Desktop, no Apple Container detour. The choice is recorded in [ADR-013](/contributing/adr/013-microsandbox-pivot/) and ADR-031.

Apple Silicon (M-series) is the primary target. Intel Macs (x86_64) work but the upstream microsandbox path on Intel is less exercised — file an issue if anything misbehaves.

## Prerequisites

- macOS 13 (Ventura) or newer. macOS 12 might work but isn't tested; macOS 11 lacks the Hypervisor.framework features microsandbox needs.
- A working **Nix** install (Determinate Nix or upstream).
- Optionally: a **Linux remote builder** if you want to build microVM rootfs images locally. macOS native can't `nix build` Linux derivations directly; either configure `nix-darwin`'s `linux-builder` or shell out to a remote `nix-daemon`. Skipping this means you can't build images on the macOS host, but you can still consume images built elsewhere.

## Install Nix

[Determinate Nix](https://determinate.systems/posts/determinate-nix-installer) is the easiest:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

After install, open a fresh terminal and verify:

```bash
nix --version
```

## Install mvmctl

### One-liner

```bash
curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

### Pin a version

```bash
MVM_VERSION=v0.13.0 curl -fsSL https://raw.githubusercontent.com/auser/mvm/main/install.sh | sh
```

### From source

```bash
git clone https://github.com/auser/mvm.git
cd mvm
cargo build --release
install -m 0755 target/release/mvmctl ~/.local/bin/mvmctl
```

### From crates.io

```bash
cargo install mvmctl
```

`mvmctl` is a regular Mach-O binary on macOS — no codesigning surprises in the typical install path. Hypervisor.framework requires the host process to hold the `com.apple.security.hypervisor` entitlement; the install script handles ad-hoc signing automatically. If you build from source via `cargo`, the same entitlement is added by the build script.

## Configure a Linux builder (optional but recommended)

To build microVM rootfs images on the macOS host (rather than pulling pre-built images), you need a Linux Nix builder. The cleanest path on macOS is **`nix-darwin`'s `linux-builder`**:

```nix
# in your nix-darwin configuration
{
  nix.linux-builder.enable = true;
  nix.linux-builder.ephemeral = true;     # rebuild on every boot; cheaper for occasional use
  nix.linux-builder.maxJobs = 4;
  nix.linux-builder.config = {
    virtualisation.darwin-builder.diskSize = 40 * 1024;   # MB
    virtualisation.darwin-builder.memorySize = 8 * 1024;  # MB
  };
}
```

Without `nix-darwin`, the [`nix-system-features = aarch64-linux x86_64-linux`](https://nix.dev/manual/nix/2.18/command-ref/conf-file#conf-system-features) flag plus a remote builder also works — see the Nix manual.

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

If you've configured a Linux builder, `mvmctl run` builds the rootfs locally and boots it via microsandbox. Expected cold boot: ≤ 500ms (Hypervisor.framework adds ~100ms vs Linux/KVM).

If you don't have a Linux builder, `mvmctl init` still scaffolds the project but `mvmctl run` will fail at the `nix build` step. Either set up the builder, or use a CI-built image (see [Building MicroVM Images](/guides/building-microvm-images) for the artifact path).

## Troubleshooting

**"Hypervisor.framework: entitlement missing"** — re-codesign the binary with the entitlement: `codesign --entitlements resources/mvmctl.entitlements -f -s - ~/.local/bin/mvmctl`. The release binary ships pre-signed; this only matters if you've stripped entitlements or built from source without the build script's signing step.

**`nix build` fails with "a 'x86_64-linux' with features … is required"** — you don't have a Linux builder configured. Either set up `nix-darwin`'s `linux-builder` (above) or pull a pre-built image.

**`mvmctl run` boots but `mvmctl console` fails to attach** — the `console` subcommand is only enabled for *accessible* images. If your `entrypoint.command = [ ... ]`, the build is *sealed* and console attach is rejected. Switch to `entrypoint.shell = "/bin/sh"` or pass `dev = true` in your `mkGuest` call. See [Building MicroVM Images](/guides/building-microvm-images).

**"microsandbox: libkrunfw not found"** — microsandbox 0.4.5 vendors libkrunfw, so this should never happen on a normal `cargo install`. If you see it, check that your `mvmctl` binary wasn't compiled with `--no-default-features`; `microsandbox` is in the default feature set.

## Apple Silicon vs Intel notes

- **Apple Silicon (M1/M2/M3/M4)** — Tier 2 microVM isolation via Hypervisor.framework + libkrun. Boot ~500ms cold, ~60ms snapshot-cloned. The supported path.
- **Intel Macs** — Hypervisor.framework still works on x86_64 macOS, but the upstream microsandbox testing is sparser. Expect occasional rough edges; file issues with `mvmctl doctor` output attached.

The Apple Container backend (macOS 26+ Virtualization framework) was previously a Tier 2 fallback in the auto-select ladder. It still exists in the codebase but microsandbox supersedes it on every macOS host that supports both — see ADR-013 §"Backend selection ladder" for the priority.
