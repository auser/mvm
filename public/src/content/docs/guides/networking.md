---
title: Networking
description: Network layout and connectivity in mvmctl microVMs.
---

## Network by Backend

Networking differs by backend:

| Backend | Network Type | Guest IP | Host Access |
|---------|-------------|----------|-------------|
| Firecracker (Linux) | TAP device | 172.16.0.2/30 | Direct via TAP |
| Firecracker (Lima) | TAP in Lima VM | 172.16.0.2/30 | Via Lima NAT |
| Apple Container | vmnet | DHCP-assigned | Via vmnet bridge |
| microvm.nix | TAP device | 172.16.0.2/30 | Direct via TAP |
| Docker | Docker bridge | Docker-assigned | Via Docker port mapping |

## Firecracker Network Layout

```
Firecracker microVM (172.16.0.2/30, eth0)
    | TAP interface (tap0)
Lima VM (172.16.0.1/30, tap0)  --  iptables NAT  --  internet
    | Lima virtualization
Host (macOS / Linux)
```

The microVM has internet access via NAT through the Lima VM (or directly on native Linux). The TAP device connects the microVM to the host network namespace.

## Port Forwarding

Forward guest ports to the host with `-p`:

```bash
mvmctl up --flake . -p 8080:8080
mvmctl up --flake . -p 3000:3000 -p 8080:8080   # multiple ports

# Or forward after boot
mvmctl forward my-vm -p 3000:3000
```

## vsock Communication

MicroVMs don't use networking for host communication -- they use **vsock**:

| Port | Protocol | Purpose |
|------|----------|---------|
| 52 | Length-prefixed JSON | Guest agent (health checks, status, snapshot lifecycle) |

The host connects by writing `CONNECT 52\n` to the vsock socket and reading `OK 52\n`. All requests are request/response pairs. vsock is supported on Firecracker, Apple Container, and microvm.nix backends. Docker uses a unix socket instead.

## No SSH

MicroVMs have **no SSH access** by design. Communication is exclusively via vsock. This eliminates:

- SSH key management
- SSH daemon attack surface
- Network-based authentication bypasses

For debugging dev builds, use `mvmctl logs <name>` to view guest console output, or `mvmctl logs <name> -f` to follow in real time.

## Network Policies

By default, microVMs have unrestricted internet access via NAT. Use `--network-preset` or `--network-allow` to restrict outbound traffic:

```bash
# Built-in presets
mvmctl up --flake . --network-preset dev          # GitHub, npm, PyPI, crates.io, OpenAI, Anthropic
mvmctl up --flake . --network-preset registries    # Package registries only
mvmctl up --flake . --network-preset none          # No outbound (DNS only)

# Explicit allowlist
mvmctl up --flake . \
    --network-allow github.com:443 \
    --network-allow api.openai.com:443
```

Network policies are enforced via iptables FORWARD rules on the bridge interface inside the Lima VM. DNS (port 53) is always allowed so domain resolution works. Rules are automatically cleaned up when the VM stops.

**Built-in presets:**

| Preset | Allowed Domains |
|--------|----------------|
| `unrestricted` | All traffic (default) |
| `dev` | github.com, api.github.com, registry.npmjs.org, crates.io, static.crates.io, index.crates.io, pypi.org, files.pythonhosted.org, api.openai.com, api.anthropic.com |
| `registries` | registry.npmjs.org, crates.io, static.crates.io, index.crates.io, pypi.org, files.pythonhosted.org |
| `none` | No outbound traffic (DNS only) |

## Seccomp Profiles

Restrict the syscalls available inside the microVM with `--seccomp`:

```bash
mvmctl up --flake . --seccomp standard    # File ops + process control (no sockets)
mvmctl up --flake . --seccomp network     # Standard + socket syscalls
mvmctl up --flake . --seccomp minimal     # Signals, pipes, timers only
```

The seccomp manifest is written to the config drive as `seccomp.json` for the guest init to apply via `prctl(PR_SET_SECCOMP)`. Tiers are cumulative — each includes all syscalls from lower tiers.

| Tier | Syscalls | Use Case |
|------|----------|----------|
| `essential` | ~40 | Process bootstrap only (linker, glibc init) |
| `minimal` | ~110 | + signals, pipes, timers, process control |
| `standard` | ~140 | + file manipulation, fs operations |
| `network` | ~160 | + sockets, connect, bind (for networked agents) |
| `unrestricted` | all | No restrictions (default) |

## DNS

The guest's `/etc/resolv.conf` is configured at build time to use the host's DNS resolver. Internet access works out of the box through the NAT chain (Firecracker), vmnet (Apple Container), or Docker bridge networking (Docker).
