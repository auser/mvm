---
title: "Install mvm on Windows"
description: "Windows is not a supported local microVM host today. WSL2 nested KVM and Hyper-V managed Linux builders are future backend work."
---

mvm does **not** currently support native Windows as a local microVM host. The supported local hosts are:

- macOS Apple Silicon.
- Native Linux with `/dev/kvm`.

WSL2 with nested KVM is a future/experimental backend candidate, not a support promise. A managed Linux builder VM on Hyper-V is also future work.

## Current Windows Guidance

Use one of these paths today:

- Run mvm on a Linux host with `/dev/kvm`.
- Run mvm on an Apple Silicon Mac.
- Use Docker only for non-security-sensitive experimentation, understanding that it is Tier 3 and not a microVM isolation boundary.

## Experimental: WSL2 With Nested KVM

Some WSL2 installations expose `/dev/kvm` through nested virtualization. If present, pieces of the Linux path may work, but this is not a supported backend yet. Before treating it as usable, verify inside the WSL2 distro:

```bash
test -c /dev/kvm && test -w /dev/kvm
mvmctl doctor
```

If either check fails, use a supported host. See the [WSL2 notes](/guides/windows-wsl2) for details and caveats.

### Optional: host-side Nix (in WSL2) for power users

Skip this unless you're contributing to mvm itself or want a shared `/nix/store` between your editor and mvm. Inside the WSL2 distro:

```bash
sh <(curl -L https://nixos.org/nix/install) --daemon
. /etc/profile.d/nix.sh
```

Installing host-side Nix is optional. The normal `mvmctl build` path still treats the CLI as the host control plane and the builder VM as the image build boundary. See [Builder VM](/guides/builder-vm/).

## Alternative: Tier 3 Docker fallback

Docker Desktop on Windows can run mvm at Tier 3. **You give up microVM isolation** — see the [Matryoshka model](/security/matryoshka) — so this path is for *non-security-sensitive* workloads only.

```powershell
# Inside a Docker Desktop linux container or WSL2 distro:
mvmctl run --hypervisor docker --flake .
```

`mvmctl run` will print a loud warning banner naming the seven CVEs and the dropped claims. Suppress it once you've acknowledged the tier:

```powershell
$env:MVM_ACK_DOCKER_TIER = "1"
```

The Docker tier exists as a convenience, not a sandbox. If your workload involves untrusted code, AI-generated scripts, or anything you wouldn't run as your own user, switch to a supported Apple Silicon or Linux KVM host.

## What about native Windows microVMs?

There isn't a maintained native-Windows microVM stack we support today. Hyper-V is the likely future Windows direction, but as a **managed Linux builder/backend VM**, not as part of the libkrun path.

That future backend needs its own lifecycle, filesystem, networking, and trust model. Until it lands, native Windows remains unsupported.

## Troubleshooting

- **`/dev/kvm` missing inside WSL2** — this is expected on many hosts. WSL2 nested KVM is experimental for mvm.
- **`mvmctl doctor` reports "no KVM available"** — use a supported Linux KVM host or Apple Silicon Mac.

See [Windows troubleshooting](/guides/windows-troubleshooting) for the full Windows-specific FAQ.
