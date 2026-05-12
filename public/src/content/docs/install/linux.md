---
title: "Install mvm on Linux"
description: "mvm on Linux is the Tier 1 production target — Firecracker + KVM, no virtualization wrapper, sub-200ms cold boot."
---

Linux is mvm's Tier 1 target. The full security posture (verified boot, jailer, seccomp tier "strict") and the project's tightest boot-time budget (≤ 200ms cold on Firecracker; ≤ 30ms snapshot-cloned) hold here. Other platforms get the same API surface via [ADR-013](/contributing/adr/013-microsandbox-pivot/), but Linux is where mvm runs at full pace.

## Prerequisites

You'll need:

- A CPU + kernel with **KVM** enabled. Most modern x86_64 / aarch64 hosts qualify; verify with:

  ```bash
  test -w /dev/kvm && echo "KVM accessible" || echo "KVM not accessible"
  ```

  If `/dev/kvm` exists but is `root`-only, add yourself to the `kvm` group: `sudo usermod -aG kvm "$USER"` (re-login required).
- **Rust 1.85+** if you build `mvmctl` from source.

You **do not need Nix on your host**. mvm bootstraps a small Linux builder microVM on first build, runs `nix build` inside it, and extracts the resulting rootfs back to your host. See [ADR-013 §"Linux builder via microsandbox"](/contributing/adr/013-microsandbox-pivot/) for the design.

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

## Verify

```bash
mvmctl doctor
```

`doctor` checks for `/dev/kvm` access, the cache directory permissions, and the active backend. On a healthy Linux + KVM host you'll see Firecracker selected as the auto-default. Host-side Nix is reported but not required.

## First microVM

```bash
mkdir my-app && cd my-app
mvmctl init
mvmctl run
```

`mvmctl init` scaffolds an `mvm.toml` + `flake.nix` in your project. `mvmctl run` reads `mvm.toml`, builds the rootfs via Nix (using your flake's `mvm.lib.x86_64-linux.mkGuest` call), and boots it on Firecracker. Expected cold boot: ≤ 200ms.

See [Building MicroVM Images](/guides/building-microvm-images) for the user-facing flake API.

## Troubleshooting

**"`/dev/kvm`: permission denied"** — your user isn't in the `kvm` group. `sudo usermod -aG kvm "$USER"` and start a new shell.

**"`mvmctl run` falls back to microsandbox even though I have KVM"** — check `mvmctl doctor` output. The auto-select ladder picks Firecracker only when `/dev/kvm` is writable; if it's `root`-only, microsandbox wins as the cross-platform fallback. Same fix as above.

**Nix build is slow** — first builds pull from `cache.nixos.org` and `cache.flakehub.com`. Subsequent builds hit the builder VM's `/nix/store`, which mvm keeps warm across runs. If you've opted into host-side Nix (below), the host store is shared into the builder VM and reused across both modes.

**Firecracker errors with "TooManyOpenFiles"** — bump the open-files ulimit: `ulimit -n 4096`. mvm sets a sensible default but very-high-density runs need headroom.

## Optional: host-side Nix for power users

mvm doesn't need Nix on the host — the builder microVM handles all `nix build` invocations. You may still want host-side Nix if you're:

- contributing to mvm itself and want a shared `/nix/store` between your editor's build commands and mvm's,
- on a fast box where shaving the builder VM's per-invocation overhead matters,
- already running a `nix-daemon` for other projects.

If you opt in, [Determinate Nix](https://determinate.systems/posts/determinate-nix-installer) is the easiest path:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

The upstream NixOS installer also works:

```bash
sh <(curl -L https://nixos.org/nix/install) --daemon
```

When `mvmctl build` detects a working host-side Nix that can build Linux derivations, it uses it directly and skips the builder microVM. Otherwise it falls through to the bootstrapped path.

## Distro-specific notes

- **Ubuntu/Debian** — `apt install qemu-utils e2fsprogs` if you need `mkfs.ext4` for the [smoke test](https://github.com/tinylabscom/mvm/blob/main/tests/smoke_microsandbox.rs).
- **Fedora/RHEL** — `dnf install e2fsprogs qemu-img`. Make sure SELinux isn't blocking `/dev/kvm` access (it usually isn't, but `audit2why` is your friend if it does).
- **Arch** — `pacman -S e2fsprogs qemu-img`. Already lean.
- **NixOS** — easiest path: `nix profile install github:tinylabscom/mvm`. KVM is enabled by default; `kvm` group membership is the only thing to verify.
