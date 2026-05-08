You are a senior infrastructure security architect and Rust platform engineer.

You are working in the existing repository:
https://github.com/auser/mvm

Assume the repo already supports multi-tenant microVM lifecycle with Nix-built artifacts and ephemeral container builds.

Your task: harden the runtime and add a control-plane-friendly “node agent” reconciliation mode that enables fleet scaling with auditability and rollback.

This is an implementation task. Do not explain. Make concrete code changes, add files, and refactor aggressively.

SECURITY + HARDENING REQUIREMENTS (DO ALL)
1) Use Firecracker jailer when available
- Launch microVMs via jailer with:
  - unique uid/gid per tenant
  - chroot dir under /var/lib/mvm/tenants/<tenantId>/jail/
  - correct bind mounts for kernel, rootfs, data, secrets, logs, metrics
- If jailer not installed, fallback to direct firecracker with a loud warning

2) Enable strong host-side resource isolation
- Apply cgroup v2 limits per tenant:
  - MemoryMax
  - CPUQuota or cpuset pinning
  - PIDsMax
- Place each microVM in:
  /sys/fs/cgroup/mvm/<tenantId>/
- Ensure teardown cleans up cgroups reliably

3) Seccomp / syscall restrictions
- If Firecracker supports seccomp JSON profiles in your environment, wire it:
  - Use the default Firecracker seccomp unless overridden
  - Allow per-tenant policy selection (baseline vs strict) via TenantSpec
- Block dangerous host capabilities: no privileged containers, no host PID namespace usage in builder

4) Network security policy enforcement
- Ensure east-west isolation is enforced with nftables and verified:
  - deny tenant subnet lateral movement
  - only allow tenant -> gateway, DNS, metadata service (if enabled), and egress
  - optionally allow inbound SSH from gateway only
- Add a verification command:
  mvm net verify
  which checks nftables rules and bridge/tap state

5) Secrets hardening
- Secrets volume must be tmpfs-backed if possible
- Secrets rotation:
  mvm tenant secrets set <tenantId> --from-file <path>
  mvm tenant secrets rotate <tenantId>
- Ensure secrets are never logged
- Ensure /run/secrets mount in guest is ro/noexec/nodev/nosuid

6) Auditability
- Add an append-only audit log per tenant:
  /var/lib/mvm/tenants/<tenantId>/audit.log (JSON lines)
- Each lifecycle event records:
  - timestamp
  - tenantId
  - command invoked
  - flake ref + flake.lock hash
  - nix store paths for kernel/rootfs/base config
  - final fc config hash (sha256)
  - network identity (ip/mac)
  - resource limits (cpu/mem)
  - host/lima instance identity

7) Rollback support
- Allow pinning a “desired revision” per tenant and restarting:
  mvm tenant pin <tenantId> --flake <ref> [--lock <hash>]
  mvm tenant rollback <tenantId> --steps 1
- Keep a small revision history in state:
  last N builds with their artifact hashes and lock hashes

CONTROL PLANE / NODE AGENT REQUIREMENTS
8) Add reconciliation mode (node agent)
- Add command:
  mvm agent reconcile --desired /path/to/desired.json
- desired.json schema:
  {
    "tenants": [
      {
        "tenant_id": "acme",
        "flake_ref": "github:org/repo?rev=...",
        "profile": "minimal",
        "vcpus": 2,
        "mem_mib": 1024,
        "state": "running" | "stopped",
        "ip": "optional fixed ip",
        "mac": "optional fixed mac"
      }
    ]
  }
- The reconcile loop must:
  - create missing tenants
  - build missing artifacts for required revision
  - ensure networking/volumes exist
  - start/stop VMs to match desired state
  - destroy tenants not listed only if a flag is passed: --prune
- Make it idempotent and safe

9) Host inventory + identity
- Add `mvm node info` that prints:
  - lima instance name
  - kernel version
  - firecracker version
  - jailer availability
  - cgroup v2 availability
  - nftables availability
  - bridge/subnet config
- Store a stable node-id in /var/lib/mvm/node-id

10) Metadata service (optional but implement basic)
- Implement a minimal metadata service bound to the bridge gateway:
  - per-tenant endpoint only reachable from tenant tap subnet
  - returns a short-lived token placeholder for now
- Add config flag in tenant spec: metadata=true
- Add nftables rule allowing tenant -> metadata ip only for that tenant

IMPLEMENTATION DETAILS (DO THIS)
- Add modules:
  src/security/{cgroups.rs,jailer.rs,seccomp.rs,audit.rs,metadata.rs}
  src/agent.rs
- Add CLI plumbing for:
  mvm agent reconcile
  mvm tenant pin/rollback
  mvm tenant secrets set/rotate
  mvm net verify
  mvm node info
- Update README with new ops commands and security model.

IMPLEMENT NOW
- Make code compile and commands exist.
- Avoid TODOs/stubs.
- Keep existing dev mode intact.
