---
title: "Windows troubleshooting"
description: "Common Windows-specific issues with mvm: WSL2 setup, nested virt, port forwarding, BIOS, and Hyper-V interactions."
---

This page is the Windows-specific FAQ. For Windows install steps see [Install on Windows](/install/windows); for the WSL2 walkthrough see [the WSL2 guide](/guides/windows-wsl2). General mvm troubleshooting (build issues, network issues, etc.) lives in [the main troubleshooting page](/guides/troubleshooting).

## Setup

### `wsl --install` fails with "this update only applies..."

Your Windows version is too old. mvm requires Windows 10 21H2 (build 19044) or any Windows 11. Run `winver` to check; update via Windows Update if you're behind.

### `wsl --install` succeeds but the Ubuntu shell doesn't open

Two common causes:

1. **Hyper-V is disabled.** Open *Turn Windows features on or off* and enable both *Virtual Machine Platform* and *Windows Subsystem for Linux*. Reboot.
2. **CPU virtualization is off in BIOS.** Look for *Intel VT-x* / *AMD-V* / *SVM*. Enable, save, reboot.

After either fix, `wsl --shutdown` then reopen Ubuntu.

### "WSL2 requires an update to its kernel component"

Run:
```powershell
wsl --update
```

If `wsl --update` returns "no update available" but the error persists, the kernel package failed to register. Reinstall manually from the [Microsoft download page](https://learn.microsoft.com/en-us/windows/wsl/install-manual).

## Nested virt (`/dev/kvm` inside WSL2)

### `/dev/kvm` is missing

mvm needs `/dev/kvm` inside the WSL2 distro for Tier 1 isolation. If it's missing:

1. **Update WSL itself:**
   ```powershell
   wsl --update
   wsl --shutdown
   ```
2. **Confirm CPU support:**
   ```bash
   grep -E '(vmx|svm)' /proc/cpuinfo
   ```
   If empty, your CPU doesn't expose virt or BIOS has it disabled.
3. **Confirm Hyper-V isn't blocking nested virt:** modern Hyper-V exposes nested virt by default to WSL2. Some third-party security tools (Kaspersky, certain enterprise EDR products) install Hyper-V hypervisors that block nested virt. Disable those temporarily to confirm.

### `mvmctl doctor` reports "KVM not available" even though `/dev/kvm` exists

Permissions issue. Inside the WSL2 distro:

```bash
ls -la /dev/kvm
```

If the device is owned by root and your user isn't in the `kvm` group:

```bash
sudo usermod -aG kvm $(whoami)
exit  # log out and back in for the group change to take effect
```

If `kvm` group doesn't exist, `sudo groupadd kvm` then chown.

## Port forwarding

### `localhost:<port>` from Windows doesn't reach the mvm guest

mvm forwards guest port → WSL2-loopback (`127.0.0.1`). Microsoft's automatic localhost forwarding usually picks that up, but several things can break it:

- **Corporate VPNs** (Cisco AnyConnect, Palo Alto GlobalProtect) frequently kill WSL2's localhost mapping. Workaround: connect to the WSL2 distro's IP directly:
  ```bash
  hostname -I  # in the distro
  ```
  Then `http://<that-ip>:<port>` from Windows.
- **Windows Firewall** can block the forwarding listener. Allow inbound TCP on the relevant port for the *vEthernet (WSL)* adapter.
- **`localhost` resolves to IPv6** but the listener is IPv4. Try `127.0.0.1:<port>` explicitly.

### "Address already in use" when starting mvm guest

Another process on the WSL2 distro (or a stale forward from a prior run) is using the port. Run:

```bash
ss -tlnp | grep :<port>
```

— and kill the holder, or pick a different port forward.

## File system

### Nix builds are *extremely* slow

You're working from a `/mnt/c/...` path. NTFS-via-9p is many times slower than ext4 inside the WSL2 distro. **Move your project into the WSL2 ext4 filesystem** (e.g. `~/work/...`) and rebuild. Don't keep Nix store on `/mnt/c/`.

### `mvmctl bootstrap` reports "permission denied" creating `~/.mvm`

`HOME` inside the distro should be `/home/<your-user>`. If you accidentally launched the distro with `HOME=/mnt/c/Users/<you>`, NTFS permissions may not allow the `0700` chmod that `mvmctl` does for `~/.mvm` (W1.5). Fix: `unset HOME` and re-run, or set `HOME=/home/<user>` explicitly.

## Tier 3 Docker fallback

### Banner nagging on every `mvmctl run`

When the auto-selected backend is Tier 3 Docker, mvm prints a security warning. Once you've read [the Matryoshka model](/security/matryoshka) and accept the reduced isolation:

```powershell
$env:MVM_ACK_DOCKER_TIER = "1"
```

(or persist it in your shell profile / Windows env settings).

Equivalently: `~/.mvm/config.toml`:
```toml
[security]
ack_docker_tier = true
```

### Docker Desktop says "WSL2 backend required"

Docker Desktop on Windows 10/11 uses WSL2 by default. If you've manually switched to Hyper-V backend, switch back via *Docker Desktop → Settings → General → Use the WSL 2 based engine*.

## Anything else

[Open a GitHub issue](https://github.com/auser/mvm/issues) with the output of:

```bash
mvmctl doctor --json > doctor.json
```

— attached, plus your Windows version (`winver`) and WSL kernel (`wsl --version`).
