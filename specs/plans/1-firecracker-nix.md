You are a senior systems engineer and Rust infrastructure architect.

You are working inside the existing repository:
https://github.com/auser/mvm

Your task is to integrate Nix-flake-based, reproducible Firecracker microVM builds and multi-tenant lifecycle management into this repo, replacing the current “build image” approach entirely.

IMPORTANT CONSTRAINT:
❌ Docker / podman / nerdctl MUST NOT be used.
✅ ALL builds must run inside ephemeral Firecracker microVMs managed by mvm itself.

This is an implementation task. Do not explain. Make concrete code changes, add files, and refactor aggressively where needed.

----------------------------------------------------------------
HARD REQUIREMENTS
----------------------------------------------------------------

1) Firecracker microVMs are the ONLY execution primitive
   - One tenant = one Firecracker microVM
   - Build environments ALSO run inside Firecracker microVMs
   - No dependency on container runtimes

2) Nix flakes define all guest artifacts
   - guest kernel
   - guest root filesystem (immutable ext4)
   - base Firecracker config JSON
   - profiles (baseline/minimal/python)
   - all pinned via flake.lock

3) Builds run in *ephemeral build microVMs*
   - Each `build` command:
     - boots a short-lived Firecracker microVM
     - runs `nix build`
     - copies artifacts out
     - shuts the microVM down
   - Build microVMs must be stateless and disposable
   - No shared mutable builder state

4) All CLI commands must exist and work end-to-end
   - no TODOs
   - no stubs

5) Existing dev workflow must remain intact
   - dev microVMs are mutable and fast
   - tenant mode is additive

----------------------------------------------------------------
ARCHITECTURAL CONSTRAINTS
----------------------------------------------------------------

- Lima remains the Linux/KVM environment
- Firecracker runs inside Lima
- TAP-based networking remains
- Rust orchestrates everything
- Deterministic, idempotent behavior
- Tenant state persisted under /var/lib/mvm/tenants/<tenantId>
- No hardcoded Firecracker JSON

----------------------------------------------------------------
IMPLEMENTATION PLAN
----------------------------------------------------------------

A) Tenant model (required)

Add src/tenant.rs:

struct TenantSpec {
  tenant_id: String,
  flake_ref: String,   // git URL or local path
  profile: String,     // baseline|minimal|python
  vcpus: u8,
  mem_mib: u32,
}

struct TenantArtifacts {
  kernel: PathBuf,
  rootfs: PathBuf,
  fc_base_config: PathBuf,
}

struct TenantState {
  spec: TenantSpec,
  artifacts: Option<TenantArtifacts>,
  running: bool,
  firecracker_pid: Option<u32>,
  revision_history: Vec<BuildRevision>,
}

Persist state under:
  /var/lib/mvm/tenants/<tenantId>/

----------------------------------------------------------------

B) Add Nix flake (guest + builder)

Create:
nix/
  flake.nix
  guests/
    baseline.nix
    profiles/
      minimal.nix
      python.nix
  builders/
    nix-builder.nix   # minimal NixOS that can run nix build

The flake must produce:

- guest rootfs images
- guest kernel
- base Firecracker config
- builder rootfs image (Nix-enabled, no secrets)

The builder microVM rootfs:
- includes nix + git
- has outbound network
- mounts a writable build volume
- has SSH or vsock control channel

----------------------------------------------------------------

C) Ephemeral *build microVM* execution

Replace all existing build-image logic.

Implement:

mvm build vm start --purpose nix-builder
mvm build vm exec nix build <flake-output>
mvm build vm fetch artifacts
mvm build vm stop

Internally:
- mvm boots a Firecracker microVM using the builder rootfs
- mounts:
  - repo source (read-only)
  - build output volume (rw)
- executes nix build inside the microVM
- copies kernel/rootfs/config artifacts into:
    /var/lib/mvm/tenants/<tenantId>/artifacts/
- immediately shuts the builder microVM down

Builder microVMs:
- are NOT tenants
- never persist state
- are uniquely named per build invocation

----------------------------------------------------------------

D) Firecracker config generation

- Nix provides base Firecracker JSON
- Rust overlays:
  - CPU count
  - memory
  - tap device
  - MAC
  - disk paths
  - logging + metrics

Final config written to:
  /var/lib/mvm/tenants/<tenantId>/runtime/fc.json

----------------------------------------------------------------

E) Networking

- Shared bridge: br-tenants
- One tap per tenant: tap-<tenantId>
- Deterministic MAC (hash of tenantId)
- IP allocation stored in TenantState
- East-west traffic denied by default
- Egress NAT allowed

----------------------------------------------------------------

F) Data + secrets disks

- vdb: tenant data ext4 (persistent)
- vdc: secrets ext4 (read-only)
- secrets disk created per run
- secrets mounted at /run/secrets (ro,noexec,nodev,nosuid)

----------------------------------------------------------------

G) CLI (must exist)

mvm tenant create <id> --flake <ref> --profile <name> --cpus N --mem MiB
mvm tenant build <id>
mvm tenant run <id>
mvm tenant ssh <id>
mvm tenant stop <id>
mvm tenant destroy <id>

----------------------------------------------------------------

H) Keep dev mode intact

Existing dev commands must continue to work unchanged.

----------------------------------------------------------------

IMPLEMENT NOW
- Remove docker/container assumptions
- Use Firecracker microVMs for builds
- Code must compile
- Commands must run end-to-end
