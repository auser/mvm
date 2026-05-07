---
title: "Deploy mvm on AWS EC2"
description: "Run mvm on AWS EC2 with full Tier 1 (Firecracker + KVM) isolation using nested-virt instance families."
---

mvm runs natively on EC2 instance families that expose nested KVM. As of February 2026, AWS made nested virtualization standard on the **C8i / M8i / R8i** Intel-based families — you no longer need a bare-metal instance to get `/dev/kvm`. This page walks through provisioning a fresh instance and getting `mvmctl` running with full Tier 1 isolation.

## Pick an instance type

| Family | Use case | Sweet spot |
|---|---|---|
| **C8i** | CPU-heavy workloads, CI runners | `c8i.4xlarge` (16 vCPU, 32 GiB) — about $0.86/hr in Frankfurt as of Feb 2026 |
| **M8i** | General-purpose dev / mixed workloads | `m8i.2xlarge` (8 vCPU, 32 GiB) |
| **R8i** | Memory-heavy workloads (large guest counts) | `r8i.2xlarge` (8 vCPU, 64 GiB) |

Older families (C7i, M7i, R7i, M6i, etc.) do *not* expose `/dev/kvm`. If you're stuck on one of those, see the [troubleshooting "No /dev/kvm available" section](/guides/troubleshooting#no-devkvm-available-cloud-vms-without-nested-virt) for the Tier 3 Docker fallback.

The `*.metal` (bare-metal) variants also work but are dramatically more expensive and only worth it for very large workload counts. The standard nested-virt families are the default answer in 2026.

## Provision

Pick an AMI:

- **Ubuntu 24.04 LTS** (recommended) — `ami-*-ubuntu-noble-24.04-amd64-server-*`
- **Amazon Linux 2023** — `al2023-ami-*-x86_64`

A standard EC2 launch — VPC public subnet, security group allowing SSH from your IP, EBS gp3 root volume sized for image cache. We recommend **at least 50 GiB** of EBS — the Nix store + Firecracker images add up quickly. 100 GiB is comfortable for active development.

No special IAM role is required for basic mvm operation. If you plan to push images to ECR or S3, add the corresponding IAM permissions to the instance profile.

## Bootstrap

SSH in, then:

```bash
# Ubuntu 24.04
sudo apt-get update
sudo apt-get install -y curl git build-essential

# Amazon Linux 2023
sudo dnf install -y curl git gcc

# Both — install Nix (multi-user)
sh <(curl -L https://nixos.org/nix/install) --daemon
. /etc/profile.d/nix.sh

# Install mvmctl
cargo install --git https://github.com/auser/mvm mvmctl
# or grab a release binary from https://github.com/auser/mvm/releases

# First-time setup
mvmctl bootstrap
```

`mvmctl bootstrap` is idempotent. On a Linux host with `/dev/kvm` it skips the Lima install (Lima is only needed on macOS). It pulls the dev image, verifies the SHA-256 manifest (claim 6), and installs Firecracker.

## Verify Tier 1 isolation

```bash
$ mvmctl doctor
...
Active backend: firecracker (KVM available)

✓ SECURITY POSTURE: Tier 1 — full microVM isolation
   Layer coverage: L1 ✓  L2 ✓  L3 ✓  L4 ✓  L5 ✓
   All seven ADR-002 claims hold.
```

If you see "Tier 3 — Docker" or a missing-KVM warning, you're not on a nested-virt instance — go back to the instance-type table.

## Networking

mvm guests run on a private TAP bridge (`172.16.0.0/24`); they're NAT'd through the EC2 instance's primary interface. **No EC2 security-group changes are required for normal mvm use** — the vsock-based guest agent stays on the loopback inside the instance, and any port forwards mvm exposes are explicit and use loopback by default (claim, ADR-002 W4.4).

If you `mvmctl run --port-forward 8080:8080`, the guest's port 8080 maps to the EC2 instance's `127.0.0.1:8080`. To expose that to the public internet, add a security-group rule **explicitly** — mvm doesn't do that for you.

## EBS sizing

Rule of thumb:

- 10 GiB — Nix store overhead (mvm bootstrap closure)
- 5 GiB per Firecracker rootfs you keep around
- 5–20 GiB per template snapshot
- 5 GiB Lima VM (only on macOS hosts; not applicable here)

For a CI runner that builds and runs a few microVMs, 50 GiB is comfortable. For a developer instance with many templates, 100 GiB.

## Operational notes

- **Stopped instances** retain their EBS volume; mvm state is preserved across stop/start. The instance gets a new public IP unless you attach an Elastic IP.
- **Image rebuilds** can be CPU-bound; the C8i family is the right pick for that.
- **AWS Bedrock AgentCore** also runs on Firecracker. If you're weighing self-hosted mvm against managed AgentCore, both carry the same Tier 1 isolation; the trade is operational complexity vs cost vs lock-in.
- **Spot instances** work fine for mvm — the only state that matters across reboots is what you've persisted to disk. Templates survive an instance restart; running VMs do not (they're intentionally ephemeral).

## See also

- [Quick start](/getting-started/quickstart) for the basic `mvmctl` flow.
- [Matryoshka model](/security/matryoshka) for what Tier 1 actually promises.
- [Troubleshooting → No /dev/kvm available](/guides/troubleshooting#no-devkvm-available-cloud-vms-without-nested-virt) if you ended up on a non-nested-virt instance.
