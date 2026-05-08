---
title: "Deploy mvm on Ubicloud bare-metal"
description: "Run mvm on Ubicloud's open-source bare-metal cloud, which uses Cloud Hypervisor under the hood. Includes license and posture trade-offs vs AWS."
---

[Ubicloud](https://www.ubicloud.com/) is an open-source cloud provider that runs Cloud Hypervisor on bare-metal Linux+KVM hosts, with Ruby + PostgreSQL as the control plane. It's a working alternative to AWS for self-hosted infrastructure, and mvm runs on it cleanly — Ubicloud provisions a Linux VM with `/dev/kvm`, and mvm runs Firecracker inside that VM.

> ### License caveat: AGPL
>
> Ubicloud is licensed **AGPL-3.0**. If you operate Ubicloud for internal use only, this is uneventful. If you offer a service to third parties built on Ubicloud, the AGPL's network-use clause requires you to provide source for the modifications you've deployed. mvm itself is unaffected by Ubicloud's license; this is only relevant to operators of Ubicloud.

## When to pick Ubicloud over AWS

Three real reasons:

1. **You want to own the hardware.** Ubicloud runs on rented bare-metal (Hetzner, Latitude.sh, your colo). Cost-per-vCPU is dramatically lower than AWS at scale. Trade: you operate the control plane.
2. **You need a fully open-source stack.** Ubicloud's source is published; AWS's isn't. If your compliance posture requires open-source-everywhere, Ubicloud is one of the few real answers.
3. **You're already running Ubicloud.** mvm composes with what you have.

If none of those apply, AWS C8i/M8i/R8i (see [the AWS guide](/deploy/aws)) is simpler.

## Architecture

Ubicloud's architecture is *also* matryoshka-shaped, which is nice from an mvm perspective:

```
Ubicloud bare-metal (Linux + KVM)
    └── Ubicloud-managed VM (Cloud Hypervisor + Linux namespaces)
            └── mvmctl
                    └── Firecracker microVM (mvm's L1+L2)
                            └── Your workload (mvm's L3-L5)
```

The Ubicloud VM gives you `/dev/kvm` (it advertises nested virt to the guest), so mvm runs Firecracker natively at full Tier 1. You get *two* hardware-isolated layers: Cloud Hypervisor for the bare-metal-to-tenant boundary, Firecracker for the workload boundary. Useful if you're running a multi-tenant offering on top of mvm.

## Provision a Ubicloud VM

Use the Ubicloud console or CLI to provision a VM. Recommended sizing for an mvm host:

- **vCPUs**: 8+ (mvm guests cost 1–2 vCPUs each; the host needs headroom for the Lima-equivalent build environment)
- **RAM**: 16+ GiB
- **Disk**: 100+ GiB
- **Image**: Ubuntu 24.04 LTS

When prompted for nested virtualization, **enable it**. Ubicloud's default may or may not — verify by checking `/dev/kvm` after first boot.

## Bootstrap

SSH in, then:

```bash
sudo apt-get update
sudo apt-get install -y curl git build-essential

sh <(curl -L https://nixos.org/nix/install) --daemon
. /etc/profile.d/nix.sh

cargo install --git https://github.com/auser/mvm mvmctl
mvmctl bootstrap
```

Same as the [AWS guide](/deploy/aws) from this point on. `mvmctl bootstrap` detects `/dev/kvm` and skips the Lima install path.

## Verify

```bash
$ mvmctl doctor
...
Active backend: firecracker (KVM available)

✓ SECURITY POSTURE: Tier 1 — full microVM isolation
   Layer coverage: L1 ✓  L2 ✓  L3 ✓  L4 ✓  L5 ✓
   All seven ADR-002 claims hold.
```

If `mvmctl doctor` reports KVM unavailable, the Ubicloud VM didn't get nested virt. Recreate it with nesting explicitly enabled.

## Trade-offs vs AWS

| | AWS C8i/M8i/R8i | Ubicloud |
|---|---|---|
| **Cost** | ~$0.86/hr (`c8i.4xlarge`) | Lower — depends on bare-metal provider |
| **Operational overhead** | Low (managed) | Higher (you run the control plane) |
| **License** | Closed-source | AGPL-3.0 (relevant for service operators) |
| **Region availability** | Global | Wherever your bare-metal provider runs |
| **Nested virt** | Standard on C8i/M8i/R8i | Manual enable per VM |
| **Time to first VM** | ~5 min | Depends on Ubicloud install/setup |
| **Egress cost** | High | Provider-dependent (often much lower) |

Both carry the same Tier 1 isolation when configured correctly. The decision is operational/economic, not security.

## Why mvm doesn't ship a Cloud-Hypervisor backend

Tangentially relevant since Ubicloud runs CH: mvm has *considered and rejected* adding Cloud Hypervisor as a first-class backend (see [Plan 54 in the project repo](https://github.com/auser/mvm/blob/main/specs/plans/54-cloud-hypervisor-deferred.md)). Reason: every advantage CH ships is a feature Firecracker deliberately excluded for attack-surface reasons. We keep Firecracker as the unambiguous security baseline. This is unrelated to Ubicloud's choice to use CH at *its* layer (where the larger device model is needed for general-purpose guests).

## See also

- [Matryoshka model](/security/matryoshka) — why running Firecracker inside a Cloud-Hypervisor host is two layers of microVM isolation, not one.
- [AWS deployment guide](/deploy/aws) — the simpler default if license/cost don't push you to Ubicloud.
- [Ubicloud documentation](https://www.ubicloud.com/docs) — for everything below the Linux VM in the matryoshka diagram.
