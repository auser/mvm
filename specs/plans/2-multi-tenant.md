You are a senior systems engineer and Rust infrastructure architect.

You are working inside the existing repository:
https://github.com/auser/mvm

Your task is to integrate Nix-flake-based, reproducible Firecracker microVM builds and multi-tenant lifecycle management into this repo, replacing the current “build image” approach entirely.

This is an implementation task. Do not explain. Make concrete code changes, add files, and refactor aggressively where needed.

HARD REQUIREMENTS
1) Firecracker microVMs remain the isolation boundary:
   - One tenant = one microVM process (Firecracker + optional jailer later)
   - Dedicated CPU, memory, tap device, disk(s), logs/metrics paths per tenant

2) Nix flakes define and build all guest artifacts:
   - guest kernel (or a pinned kernel artifact)
   - guest root filesystem (ext4, immutable, minimal NixOS)
   - base Firecracker config JSON (template/skeleton)
   - profiles (baseline/minimal/python) via Nix modules
   - outputs must be pinned via flake.lock for auditability

3) Builds must run in ephemeral containers:
   - NO single long-lived builder container
   - Each build command launches a fresh container, runs nix build, exits
   - Container mounts repo + writable output dir
   - Rust must orchestrate builds synchronously and capture resulting paths

4) All new CLI commands must exist and work end-to-end:
   - no TODOs, no stubs

5) Dev workflow must continue to work:
   - preserve existing dev microVM flows and commands; tenant mode is additive

ARCHITECTURAL CONSTRAINTS
- Keep Lima as the Linux/KVM environment (Firecracker runs inside Lima)
- Keep current TAP-based networking approach, extend it per-tenant
- Prefer deterministic, idempotent operations
- Persist tenant state for auditability under /var/lib/mvm/tenants/<tenantId> inside Lima
- Avoid hardcoding Firecracker JSON; generate by overlaying runtime values on a Nix-provided base config

IMPLEMENTATION PLAN (DO THIS)
A) Add a tenant model
- Create a new module (e.g., src/tenant.rs) defining:
  struct TenantSpec {
    tenant_id: String,
    flake_ref: String,   // git URL or local path
    profile: String,     // baseline|minimal|python
    vcpus: u8,
    mem_mib: u32,
    // optionally: disk sizes, egress allowlist, etc.
  }

  struct TenantNet {
    ip: String,
    gateway: String,
    cidr: u8,
    mac: String,
    tap: String,
    bridge: String,
  }

  struct TenantArtifacts {
    kernel_path: PathBuf,
    rootfs_path: PathBuf,
    fc_base_config_path: PathBuf,
    // optional: initrd, extra drives, etc.
  }

  struct TenantState {
    spec: TenantSpec,
    net: TenantNet,
    artifacts: Option<TenantArtifacts>,
    running: bool,
    firecracker_pid: Option<u32>,
    // audit fields:
    flake_lock_hash: Option<String>,
    nix_store_paths: Vec<String>,
    last_started_at: Option<String>,
  }

- Persist:
  /var/lib/mvm/tenants/<tenantId>/
    spec.json
    state.json
    runtime/
      fc.final.json
      firecracker.sock
      logs/
      metrics/

B) Add Nix flake in-repo
- Create directory:
  nix/
    flake.nix
    tenants/
      baseline.nix
      profiles/
        minimal.nix
        python.nix

- flake.nix must expose packages for x86_64-linux:
  packages.x86_64-linux.tenant-rootfs-<tenantId> (ext4 image)
  packages.x86_64-linux.tenant-kernel-<tenantId> (vmlinux/bzImage)
  packages.x86_64-linux.tenant-fc-base-<tenantId> (JSON skeleton)

- Provide a generic output too:
  packages.x86_64-linux.tenant-rootfs (parameterized via env vars or via separate outputs for profiles)
  BUT: keep it simple: allow selecting profile via build arg or separate outputs.

- Guest rootfs must be minimal NixOS:
  - sshd enabled
  - static IP on eth0, configurable via boot args or Nix module variables
  - mount /data (vdb) and /run/secrets (vdc) with nofail; secrets ro/noexec/nodev/nosuid
  - no secrets baked into image

C) Replace “build image” logic with ephemeral build containers
- Remove existing builder image approach entirely (delete old code/commands if present)
- Implement a helper in Rust that runs ephemeral container builds inside Lima:
  - Choose docker OR nerdctl inside Lima. Prefer docker if present; otherwise implement detection with fallback.
  - Build container image should be a public nix-enabled image (ghcr.io/nixos/nix:latest) OR build a tiny local image once, but still launched ephemerally per build.
  - The container run must be:
    docker run --rm \
      -v /var/lib/mvm/src:/src:ro \
      -v /var/lib/mvm/build:/out \
      -w /src \
      ghcr.io/nixos/nix:latest \
      nix --extra-experimental-features "nix-command flakes" build .#<output> -o /out/<tenantId>/<name>

- Important: because repo is on macOS host, copy it into Lima first (you already manage files via limactl).
  Implement:
   - mvm sync (or integrate into tenant build) to rsync/copy the repo into /var/lib/mvm/src in Lima.
   - Alternatively, mount via Lima, but ensure path stability. Prefer copying for determinism.

- After nix build, locate artifacts:
  /out/<tenantId>/kernel
  /out/<tenantId>/rootfs
  /out/<tenantId>/fc-base.json
  Copy/symlink them into /var/lib/mvm/tenants/<tenantId>/artifacts/

D) Firecracker config generation
- Read base config JSON from Nix output.
- Overlay runtime values in Rust:
  - machine-config: vcpu_count, mem_size_mib, smt=false
  - network-interfaces: tap, mac
  - drives:
    - rootfs (ro, root device)
    - data ext4 image (rw) created on tenant create or first run
    - secrets ext4 image (ro) created on each run (or each secrets update)
  - logger/metrics paths under runtime/

- Write final config to:
  /var/lib/mvm/tenants/<tenantId>/runtime/fc.final.json

E) Networking: per-tenant TAP and shared bridge
- Extend existing network.rs:
  - Ensure bridge br-tenants exists; host gateway IP e.g. 10.50.0.1/24 (configurable)
  - Each tenant gets tap-<tenantId>, enslaved to br-tenants
  - Deterministic MAC: locally-administered + hash(tenantId)
  - Deterministic IP allocation:
    - simplest: sequential from 10.50.0.10 upward stored in a registry file
    - store allocation in TenantState so it is stable across restarts
  - Enforce isolation:
    - deny east-west by default on br-tenants (drop traffic where src/dst are in tenant subnet and dst != gateway)
    - allow tenant -> gateway
    - allow outbound NAT to internet
  Use nftables if available, else iptables.

F) Secrets + data drives
- Data volume:
  - create /var/lib/mvm/tenants/<tenantId>/volumes/data.ext4 if missing
  - size configurable in TenantSpec (default 2GiB)
- Secrets volume:
  - created fresh per run: secrets.ext4 (tmpfs-backed if possible)
  - populate from:
    - placeholder for now: read a local file /var/lib/mvm/tenants/<tenantId>/secrets.json
    - convert into files under /secrets inside the ext4 image
  - attach read-only
  - guest mounts it at /run/secrets

G) Process lifecycle
- Start:
  - create networking
  - create volumes
  - launch firecracker with final config
  - record pid + running=true in state.json
- Stop:
  - gracefully signal firecracker
  - cleanup socket
  - running=false
- Destroy:
  - stop
  - delete tap
  - wipe runtime
  - optionally delete volumes (or keep if flag)

H) CLI: implement these commands fully
Add subcommand group: tenant
  mvm tenant create <tenantId> --flake <ref> --profile <name> --cpus N --mem MiB
  mvm tenant build <tenantId>
  mvm tenant run <tenantId>
  mvm tenant ssh <tenantId>
  mvm tenant stop <tenantId>
  mvm tenant destroy <tenantId> [--wipe-volumes]
  mvm tenant list

Make them idempotent and with helpful errors.

I) Documentation
Update README:
- Explain dev mode vs tenant mode
- Explain ephemeral build containers
- Show example workflow:
  mvm start
  mvm tenant create acme --flake . --profile minimal --cpus 2 --mem 1024
  mvm tenant build acme
  mvm tenant run acme
  mvm tenant ssh acme
  mvm tenant stop acme
  mvm tenant destroy acme

IMPLEMENT NOW
- Modify the repo accordingly.
- Add tests where feasible (unit tests for config overlay, MAC/IP allocator).
- Ensure `cargo build` passes.
- Ensure commands are wired in and compile.
- Avoid leaving any non-functional placeholders.
