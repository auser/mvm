You are a senior systems engineer and Rust infrastructure architect.

You are working inside the existing repository:
https://github.com/auser/mvm

Your task is to integrate Nix-based, reproducible Firecracker microVM builds and multi-tenant lifecycle management into this repo, replacing the current “build image” approach entirely.

This is an implementation task. Do not explain. Make concrete code changes, add files, and refactor aggressively where needed.

🎯 High-level goals (must all be satisfied)

Firecracker microVMs remain the isolation boundary

One tenant = one Firecracker microVM

Dedicated CPU, memory, tap device, disks

Nix flakes define all build artifacts

Guest kernel

Guest root filesystem

Base Firecracker config (JSON)

Optional data + secrets disk layouts

All pinned and reproducible

Builds run in short-lived, ephemeral containers

No single long-lived “builder image”

Each build command launches a fresh container

Containers exit immediately after build

Results are copied/symlinked into mvm runtime dirs

All commands already exist

No TODOs, no placeholders

CLI commands must work end-to-end after implementation

Dev mode must continue to work

mvm dev (or equivalent) should still give a fast, mutable VM for iteration

Tenant mode is additive, not destructive

🧱 Architectural constraints

Keep Lima as the Linux/KVM execution environment

Firecracker runs inside Lima

Networking is TAP-based as today

Rust remains the orchestration language

Prefer systemd-free, explicit process management

Avoid global mutable state where possible

🧩 Required refactor + additions
1. Introduce a Tenant model

Add a new module (or extend existing ones) defining:

struct TenantSpec {
    tenant_id: String,
    flake_ref: String,     // git URL or local path
    profile: String,       // e.g. "baseline", "python", "gpu"
    vcpus: u8,
    mem_mib: u32,
}

struct TenantArtifacts {
    kernel_path: PathBuf,
    rootfs_path: PathBuf,
    base_fc_config: PathBuf,
}


Persist tenant state under:

/var/lib/mvm/tenants/<tenantId>/
  spec.json
  state.json
  runtime/


This directory is the audit root for that tenant.

2. Replace the existing build image logic completely

Remove or deprecate any existing “build image” or long-lived builder container logic.

Instead, implement ephemeral build containers:

Each build command:

launches a fresh container (Docker/nerdctl/podman — pick one, document it)

mounts:

the mvm repo

a writable output dir

runs nix build inside the container

exits immediately

No shared builder state is allowed.

3. Add a Nix flake to the repo

Create:

nix/
  flake.nix
  tenants/
    baseline.nix
    profiles/
      minimal.nix
      python.nix


The flake must expose outputs like:

packages.x86_64-linux.<tenantId>.kernel

packages.x86_64-linux.<tenantId>.rootfs

packages.x86_64-linux.<tenantId>.fcBaseConfig

The rootfs must be:

built with NixOS modules

immutable

minimal (SSH, networking, nothing extra)

4. New CLI commands (must be fully implemented)

Add the following commands to mvm:

mvm tenant create <tenantId> --flake <ref> --profile <name> \
    --cpus <n> --mem <MiB>

mvm tenant build <tenantId>
    # launches ephemeral build container
    # produces kernel + rootfs + base fc config

mvm tenant run <tenantId>
    # allocates tap
    # overlays runtime config (IP, MAC, cgroups)
    # launches firecracker

mvm tenant ssh <tenantId>

mvm tenant stop <tenantId>

mvm tenant destroy <tenantId>


All commands must be idempotent.

5. Networking changes

Extend the existing TAP logic so that:

Each tenant gets:

tap-<tenantId>

a deterministic MAC

A shared br-tenants bridge exists

East-west traffic is denied by default

NAT egress is allowed

Use nftables or iptables, but implement it fully.

6. Secrets and data disks

Add support for additional block devices:

/dev/vdb → tenant data (writable)

/dev/vdc → tenant secrets (read-only)

Secrets disk is:

created per run

populated by the host

attached read-only

mounted by the guest at /run/secrets

No secrets are baked into the rootfs.

7. Firecracker config generation

Do not hardcode Firecracker JSON.

Instead:

Base config comes from the Nix flake

Rust overlays:

tap name

MAC

CPU / memory

disk paths

log + metrics paths

Final config is written to:

/var/lib/mvm/tenants/<tenantId>/runtime/fc.json

8. Build containers: concrete behavior

Implement a helper that runs commands like:

docker run --rm \
  -v $MVM_REPO:/src \
  -v /var/lib/mvm/build:/out \
  ghcr.io/nixos/nix:latest \
  nix build /src/nix#tenant-<id>


Container exits immediately

Rust waits synchronously

Output paths are captured and recorded in tenant state

9. Keep dev mode intact

Do not break:

single-VM dev workflows

mutable rootfs experiments

quick iteration

Tenant mode must be clearly separate.

✅ Deliverables

Updated Rust code

New nix/ directory

CLI wiring complete

Old build image logic removed

Commands runnable end-to-end

Repo builds cleanly

Do not leave stubs, TODOs, or commentary.