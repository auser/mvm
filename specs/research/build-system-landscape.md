# MicroVM Build System: Research & Landscape Analysis

> Research document — no implementation changes. Captures findings from exploring
> alternative build approaches, performance optimization, and distribution strategies.
> Date: 2026-02-25

---

## Context

mvm uses Nix flakes to build Firecracker microVM templates (kernel + rootfs). The build
pipeline supports three backends (host, vsock, ssh), flake.lock-based caching, and
template reuse across tenants. This research validates the approach and maps the landscape
of alternatives.

**Verdict: The Nix approach is sound for mvm's architecture.** Reproducibility guarantees,
template reuse, and runtime configuration injection (config/secrets drives) are the right
design for a multi-tenant fleet manager. No architectural changes recommended.

---

## 1. Build Approach Comparison

| Approach | Reproducibility | Image Size | Build Speed | Ecosystem | Best For |
|----------|----------------|------------|-------------|-----------|----------|
| **Nix flakes** (current) | Excellent (bit-for-bit) | 300-500 MiB optimized | Slow first, cached after | Medium | Own templates, compliance |
| **OCI/Docker conversion** | Good (layer-based) | 200+ MiB | Fast (pre-built) | Largest | User-provided workloads |
| **Alpine rootfs** | Good | 50-130 MiB | Very fast | Large | High-density, minimal |
| **Buildroot** | Good | 10-50 MiB | Slow | Small | Embedded/appliance |
| **microvm.nix** (already using) | Excellent | 80-200 MiB | Slow first | Growing | NixOS-native guests |
| **Bake** (legacy) | Fair | Single ELF | Medium | Tiny | Single-image dev/deploy |

### Why Nix Wins for mvm

- **Flake.lock pinning** = cryptographic proof two tenants got identical images
- **Cache key** = `sha256(flake_lock + profile + role)` prevents cross-profile reuse bugs
- **Template reuse** = build once per (flake, profile, role) tuple, share across all tenants
- **Runtime customization** = config/secrets drives, not image rebuilds
- **BuildEnvironment trait** = already backend-agnostic; new input paths slot in cleanly

### Where Nix Falls Short

- **Learning curve** — if tenants bring their own templates, Nix is a barrier
- **First-build latency** — compiling NixOS closures from source takes minutes
- **Nix store growth** — needs GC strategy on builder nodes
- **Error messages** — Nix evaluation errors are notoriously opaque

---

## 2. OCI/Docker Conversion (Future Option)

If mvm ever needs to accept Docker images, the path is proven:

### Technical Flow
```
crane pull <image> → tar → mkfs.ext4 → mount → extract tar → inject init → unmount
```

### Key Requirements
- **Init binary**: Docker images lack `/sbin/init`. Need ~300-500 lines of Rust that:
  - Mounts devtmpfs, proc, sysfs, tmpfs
  - Reads config from `/mnt/config` (mvm config drive)
  - Configures network from kernel IP params
  - Execs the user's entrypoint as PID 1
- **Kernel**: Stock Firecracker-compatible vmlinux (not NixOS kernel)
- **Tools**: `crane` or `skopeo` for daemon-free image pulling
- **Gotchas**: glibc/musl mismatch, missing /dev nodes, no systemd

### How Others Do It
- **Fly.io**: Custom Rust init binary, LVM2 thin pool for CoW, WireGuard for SSH
- **Weaveworks Ignite**: `docker export` → ext4 → inject systemd + SSH
- **firecracker-containerd**: squashfs base + ext4 overlay, containerd snapshotter

### Integration Point
The `BuildEnvironment` trait and artifact contract (vmlinux + rootfs.ext4 → revision dir)
would not need to change. A new `OciBackend` would implement the same prepare/boot/build/
extract/teardown interface.

---

## 3. Nix Build Performance

### Priority Order (highest impact first)

#### A. Self-Hosted Binary Cache (Attic)
- **Impact**: Turns 10-minute builds into 30-second downloads
- **How**: Attic server backed by S3/MinIO, Rust implementation
- **Config**: `substituters = https://attic.internal?priority=5 https://cache.nixos.org?priority=20`
- **Alternative**: Harmonia (simpler, lighter) or Cachix (hosted, no ops)
- **Links**: [Attic](https://github.com/zhaofengli/attic), [Harmonia](https://github.com/nix-community/harmonia), [Cachix](https://cachix.org)

#### B. Substituter Priority Chain
```
Private Attic (priority 5) → Org Cachix (priority 10) → cache.nixos.org (priority 20)
```
First hit wins. Private cache catches org-specific builds; public cache catches nixpkgs deps.

#### C. Closure Optimization (300-500 MiB target)
```nix
documentation.enable = false;
documentation.man.enable = false;
documentation.info.enable = false;
# Strip firmware, unused kernel modules, locale data
```
Standard NixOS: 2-3 GiB. Minimal + perlless profiles: ~500 MiB. microvm.nix optimization
module helps further.

#### D. Build Parallelism
```
max-jobs = "auto"   # simultaneous derivations
cores = 0           # all cores per derivation
```

#### E. Remote Builders (for heavy builds)
```bash
nix build --builders 'ssh://builder.cloud x86_64-linux'
```
Use `builders-use-substitutes = true` so remote builders pull from cache too.

#### F. Nix Store GC
```nix
nix.gc.automatic = true;
nix.gc.dates = "weekly";
nix.gc.options = "--delete-older-than 14d";
nix.optimise.automatic = true;  # hard-link dedup, saves ~25-35%
```

---

## 4. microvm.nix Best Practices

mvm already uses microvm.nix in the OpenClaw template. Key patterns:

### Module Structure (current, validated)
```
baseline.nix          → microvm.hypervisor, boot params, mount points, security
roles/gateway.nix     → systemd service, ports, drive requirements
roles/worker.nix      → systemd service, ports, drive requirements
profiles/gateway.nix  → hostname, firewall, tmpfs sizing
profiles/worker.nix   → hostname, firewall, tmpfs sizing
```

### Networking
Use `systemd.network` (not legacy NixOS networking) for static IPs:
```nix
systemd.network.networks."10-eth0" = {
  matchConfig.Name = "eth0";
  address = [ "${ipAddress}/24" ];
  routes = [{ Gateway = gateway; }];
};
```

### Kernel
microvm.nix now boots NixOS kernel with initrd — no custom kernel build needed.
Firecracker-specific: uncompressed ELF on x86_64, PE format on aarch64.

### Closure Minimization
- microvm.nix optimization module (auto-strips NixOS bloat)
- `documentation.enable = false`
- Strip firmware, locale, unused services
- For high-density: virtiofs-shared `/nix/store` across VMs (avoids duplicating store in every rootfs)

### Key microvm.nix Options
- `microvm.hypervisor` — selects hypervisor (firecracker, qemu, cloud-hypervisor, etc.)
- `microvm.shares` — shared directories (9p or virtiofs)
- `microvm.guest.cpus`, `microvm.guest.memory` — resource allocation
- `microvm.extraArgs` — hypervisor-specific arguments

### References
- [microvm.nix GitHub](https://github.com/microvm-nix/microvm.nix)
- [microvm.nix Documentation](https://microvm-nix.github.io/microvm.nix/)
- [Options Reference](https://microvm-nix.github.io/microvm.nix/microvm-options.html)
- [Minimal Overhead VMs with Nix](https://blog.koch.ro/posts/2024-03-17-minimal-vms-nix-microvm.html)

---

## 5. Template Distribution

### Recommended Architecture: ORAS + Cosign

```
Build Node                     OCI Registry (ECR/GHCR)           Agent Node
┌──────────┐    oras push     ┌──────────────────────┐  oras pull  ┌──────────┐
│ nix build │ ──────────────► │ mvm/templates:v2.1.0 │ ──────────► │ pull +   │
│ + sign    │   + cosign sign │ (vmlinux + rootfs)   │  + verify   │ extract  │
└──────────┘                  └──────────────────────┘             └──────────┘
```

- **Storage**: ORAS pushes rootfs + vmlinux as OCI artifacts with custom media type
  (`application/vnd.mvm.template.v1+tar`)
- **Registry**: Any OCI-compliant registry (ECR, GHCR, Docker Hub, Harbor)
- **Signing**: Cosign (industry standard, Rekor transparency log, keyless via OIDC)
- **Versioning**: Content-addressed (sha256 of flake_lock + profile + role) or semver tags
- **Auth**: Standard registry token flow

### Alternatives Considered

| Approach | Pros | Cons |
|----------|------|------|
| **ORAS** (recommended) | Standard OCI, any registry, Cosign signing | No delta updates |
| **Nix binary cache** | Native Nix, closure-aware | Requires Nix on agents |
| **S3 + casync** | Delta updates (40-60% BW savings) | Custom tooling |
| **Plain S3/MinIO** | Simple | No standard auth/versioning |

### Delta Updates (Future Optimization)
For large rootfs images, casync provides content-addressed chunking with HTTP-friendly
distribution. ~40-60% bandwidth savings on updates. Not needed initially — full pulls of
300-500 MiB images are fast enough.

### References
- [ORAS](https://oras.land/)
- [Cosign (Sigstore)](https://github.com/sigstore/cosign)
- [casync](https://0pointer.net/blog/casync-a-tool-for-distributing-file-system-images.html)
- [Fly.io Machines](https://fly.io/docs/machines/overview/)

---

## 6. Bake: Why mvm Moved Away

Bake (https://github.com/losfair/bake) packages Firecracker + kernel + rootfs into a
single self-contained ELF executable. Great for single-image distribution, but
fundamentally incompatible with mvm's model:

- **No template sharing** — each ELF is monolithic, can't share across tenants
- **No runtime customization** — everything baked in at build time
- **No versioned artifacts** — can't independently update kernel vs rootfs
- **No multi-pool** — single image per workload, not composable

The legacy `mvm build` (Mvmfile.toml → bake ELF) path still exists in
`crates/mvm-runtime/src/vm/image.rs` but is superseded by `mvm pool build` for production.

---

## Summary: Production Readiness Checklist

| Area | Status | Next Step |
|------|--------|-----------|
| Build pipeline | Done | — |
| Caching (flake.lock) | Done | — |
| Template reuse | Done | — |
| Build backends (host/vsock/ssh) | Done (3 modes) | — |
| Binary cache (Attic/Cachix) | Not set up | Biggest perf win |
| Closure optimization | Not tuned | Easy win, <500 MiB target |
| Template distribution (ORAS) | Not implemented | Needed for multi-node |
| OCI input path | Not implemented | Only if needed for ecosystem access |
| Image signing | Not implemented | Needed for production trust |
| Nix store GC | Not automated | Prevent disk exhaustion |
