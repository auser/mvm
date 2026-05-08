---
title: "Install mvm on Windows"
description: "mvm runs on Windows via WSL2 with first-class Tier 1 microVM isolation. This page is the entry point for Windows users."
---

mvm on Windows uses **WSL2** as its primary supported path. Inside a WSL2 distro you get the same Tier 1 microVM isolation as a Linux host (Firecracker + KVM). This is the recommended setup for any Windows user who wants real microVM workloads.

If WSL2 isn't an option for your environment, you can fall back to the Tier 3 Docker tier — but that's a container, not a microVM, and the security model collapses. See the [Matryoshka model](/security/matryoshka) for what each tier promises.

## Quickstart (WSL2 — recommended)

You'll need: Windows 10 21H2+ or Windows 11 (any), with a CPU that supports virtualization (most modern Intel/AMD). Most laptops bought after 2018 qualify.

1. **Enable WSL2** (one-time, requires admin):
   ```powershell
   wsl --install
   ```
   This installs WSL2 + the default Ubuntu distro and enables the Hyper-V components mvm needs. Reboot when prompted.

2. **Open the Ubuntu shell** (Start menu → "Ubuntu") and update:
   ```bash
   sudo apt-get update && sudo apt-get install -y curl git build-essential
   ```

3. **Install Nix (multi-user)**:
   ```bash
   sh <(curl -L https://nixos.org/nix/install) --daemon
   . /etc/profile.d/nix.sh
   ```

4. **Install mvmctl**:
   ```bash
   cargo install --git https://github.com/auser/mvm mvmctl
   ```
   Or download a release binary from the [releases page](https://github.com/auser/mvm/releases).

5. **First-time setup**:
   ```bash
   mvmctl bootstrap
   ```
   This pulls the dev image, verifies the SHA-256 manifest, and installs Firecracker. Idempotent — safe to re-run.

6. **Verify Tier 1 isolation**:
   ```bash
   mvmctl doctor
   ```
   Look for `Active backend: firecracker (KVM available)` and the all-✓ Security posture section.

You're done. From here, follow the [Quick Start](/getting-started/quickstart) — everything works the same as on a native Linux host because WSL2 *is* a Linux host as far as mvm is concerned.

See the [WSL2 walkthrough](/guides/windows-wsl2) for detail on bootstrap automation, port forwarding from Windows to a guest, and a few WSL2-specific quirks.

## Alternative: Tier 3 Docker fallback

If WSL2 isn't an option (corporate IT lock-down, no nested virt, etc.), Docker Desktop on Windows can run mvm at Tier 3. **You give up microVM isolation** — see the [Matryoshka model](/security/matryoshka) — so this path is for *non-security-sensitive* workloads only.

```powershell
# Inside a Docker Desktop linux container or WSL2 distro:
mvmctl run --hypervisor docker --flake .
```

`mvmctl run` will print a loud warning banner naming the seven CVEs and the dropped claims. Suppress it once you've acknowledged the tier:

```powershell
$env:MVM_ACK_DOCKER_TIER = "1"
```

The Docker tier exists as a convenience, not a sandbox. If your workload involves untrusted code, AI-generated scripts, or anything you wouldn't run as your own user, switch to WSL2.

## What about native Windows microVMs?

There isn't a maintained native-Windows microVM stack we'd want to ship today. We considered Cloud Hypervisor + WHPX (the Hyper-V virtualization API) and rejected it for security-posture reasons — see [plan 53 §"Plan F"](https://github.com/auser/mvm/blob/main/specs/plans/54-cloud-hypervisor-deferred.md) and the [Matryoshka model](/security/matryoshka). The 2026 microVM ecosystem treats Windows as a *guest OS*, not a *host platform* for microVM tooling. WSL2 is the right answer.

If you need a real native-Windows microVM tool, look at Docker Desktop's own sandbox feature (released Jan 2026) — it uses Hyper-V directly and is purpose-built for that. mvm complements rather than competes.

## Troubleshooting

- **"WSL2 not available" on install** — make sure CPU virtualization is enabled in BIOS. On laptops this is often *Intel VT-x* / *AMD-V*. After enabling, run `wsl --install` again.
- **`/dev/kvm` missing inside WSL2** — older Windows builds don't expose nested KVM. Update Windows to the latest 11 release; `wsl --update` after.
- **`mvmctl doctor` reports "no KVM available"** — see the [No /dev/kvm available](/guides/troubleshooting#no-devkvm-available-cloud-vms-without-nested-virt) troubleshooting entry.

See [Windows troubleshooting](/guides/windows-troubleshooting) for the full Windows-specific FAQ.
