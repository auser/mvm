---
title: "WSL2 walkthrough for mvm"
description: "Step-by-step WSL2 setup for mvm with notes on nested virt, port forwarding, file sharing, and the bootstrap automation that's coming in Sprint 48."
---

This page is the long-form companion to the [Windows install quickstart](/install/windows). It walks through the WSL2 setup choices that matter for mvm and documents a few quirks unique to running microVMs on Windows.

## Why WSL2 for mvm

WSL2 is a real Linux kernel running under Hyper-V. From inside a WSL2 distro:

- **`/dev/kvm` is available** (since Windows 10 21H2). Firecracker runs natively, giving you Tier 1 microVM isolation — the same as a native Linux host.
- **Filesystem is real Linux ext4**. APFS-style copy-on-write isn't here yet (Sprint 47 Plan D ships APFS CoW for macOS Apple Container; WSL2's ext4 will fall back to byte-copy in `mvm-runtime::vm::cow::reflink_or_copy`), but everything functionally works.
- **Networking is bridged** through Hyper-V's vmswitch. Port forwarding from Windows host to a WSL2 distro is automatic for `127.0.0.1` binds; mvm guests behind the WSL2 distro need an additional hop, covered below.

The cost is one nested VM hop: workload runs inside Firecracker microVM → which runs inside WSL2 → which runs inside Hyper-V on the Windows host. The boot-time penalty is small (~150ms), the runtime penalty is in the noise.

## Setup

The [install quickstart](/install/windows) covers `wsl --install` + `cargo install`. Two follow-up steps that matter for mvm specifically:

### Confirm nested KVM works

```bash
ls -l /dev/kvm
```

Should show a character device. If `/dev/kvm` is missing, WSL2 didn't pick up nested virt. Two fixes:

1. **Update Windows + WSL** to current versions:
   ```powershell
   wsl --update
   ```
2. **Confirm BIOS settings**: VT-x / AMD-V enabled, Hyper-V allowed.

If `mvmctl doctor` still reports KVM unavailable, see the [No /dev/kvm available](/guides/troubleshooting#no-devkvm-available-cloud-vms-without-nested-virt) entry.

### Allocate WSL2 resources

WSL2 starts with a default of 50% of host RAM and all CPUs. mvm guests run inside this budget. For a comfortable dev machine:

`%USERPROFILE%\.wslconfig`:
```ini
[wsl2]
memory=12GB
processors=8
```

Then restart WSL: `wsl --shutdown` and reopen the Ubuntu shell.

## Port forwarding (Windows host ↔ mvm guest)

mvm guests inside WSL2 run on the `172.16.0.0/24` TAP bridge, which is invisible from the Windows host by default. To expose a guest service to a Windows-side browser:

1. **Forward the guest port to the WSL2 distro's loopback** with mvm's standard mechanism:
   ```bash
   mvmctl run --port-forward 8080:80 --flake .
   ```
   This binds `127.0.0.1:8080` inside the WSL2 distro to the guest's port 80.

2. **WSL2's automatic localhost forwarding** ([documented by Microsoft](https://learn.microsoft.com/en-us/windows/wsl/networking#accessing-network-applications)) makes `localhost:8080` on Windows reach the WSL2 distro's loopback. Open `http://localhost:8080` in a Windows browser and you're hitting the mvm guest.

If localhost forwarding isn't working (some corporate VPN clients break it), fall back to the WSL2 distro's IP:

```bash
hostname -I  # inside WSL2 — gives the distro's IP on the Hyper-V vmswitch
```

Then `http://<that-ip>:8080` from Windows.

## File sharing

`/mnt/c/` (and similar) inside WSL2 maps to Windows drives. **Don't put your Nix store on `/mnt/c/`** — the cross-fs perf is brutal and Nix's case-sensitivity assumptions don't hold on NTFS. Keep mvm work inside the WSL2 ext4 filesystem (e.g. `~/work/...`).

If you need to read source from a Windows directory, do it explicitly with `cp` rather than running `cargo build` against a `/mnt/c/` path.

## What's coming in Sprint 48

[Plan I.4](https://github.com/auser/mvm/blob/main/specs/plans/53-cross-platform-roadmap.md) (WSL2 bootstrap automation) will turn the manual steps above into:

```powershell
mvmctl bootstrap
```

— detect Windows + WSL2 status, offer to install if missing, install mvm into the chosen distro, set up shared `~/.mvm`. Until then, follow the [install quickstart](/install/windows).

## See also

- [Install on Windows](/install/windows)
- [Matryoshka model](/security/matryoshka) — what Tier 1 promises and why WSL2 carries it
- [Windows troubleshooting](/guides/windows-troubleshooting)
- [WSL2 documentation](https://learn.microsoft.com/en-us/windows/wsl/)
