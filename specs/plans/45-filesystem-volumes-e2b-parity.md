# Plan 45 — Filesystem Volumes for e2b parity (mvm + mvmd)

> **Status**: design approved 2026-05-06; implementation starting on `worktree-filesystem-volumes-e2b-parity`.
> **Companion spec**: `mvmd/specs/plans/29-filesystem-volumes-e2b-parity.md` (sister repo).

## Discoveries during implementation (read before starting any phase)

These were uncovered after the design conversation but before code landed. They don't invalidate the plan, but they change which existing code we extend vs. build new.

### D1 — mvmd already has a `StorageBucket` primitive that overlaps significantly

`mvmd-gateway/src/auth/types.rs` defines `StorageBucket` and `BucketAttachment` with:
- Org-scoped (`org_id`) — Sprint 135 Phase 0010 already adds `workspace_id`.
- `BucketProvider` enum (S3, GCS, R2, …) — exactly the `ObjectStoreBackend` use-case.
- Sealed credentials via `bucket_crypto::seal_credentials` — already encrypted at rest.
- `bucket_mount_policy::validate_mount_path` — analog of our planned `MountPathPolicy`.
- `read_only` flag with explicit comment "to match e2b's bucket-write semantics".
- Persisted in SQLite `storage_buckets` + `bucket_attachments` tables.

**Implication**: the mvmd-side `FilesystemVolume` type proposed in this plan is largely a *generalisation* of `StorageBucket` to support `LocalBackend` mounts in addition to remote object storage. Three reconciliation paths to evaluate during Phase 13:

1. **Extend `StorageBucket`**: add `BucketProvider::LocalVirtiofs { root }` and adapt the data plane to dispatch via the new `VolumeBackend` trait. Keep the existing tables, REST routes, and CLI verbs. Most code reuse, least disruption to mvmd consumers.
2. **Rename `StorageBucket` → `Volume` (breaking)**: align mvmd with e2b/mvm naming. Schema migration + REST URL migration. Cleanest naming, biggest blast radius.
3. **Coexist** (the original plan assumption): `StorageBucket` stays for object-storage-only use cases; new `FilesystemVolume` covers the broader trait-pluggable surface. Two parallel concepts; risk of confusion.

**Recommendation**: evaluate path 1 first. If it composes cleanly (the trait dispatch fits the existing handler shape), take it — minimal churn, no schema migration. The mvm-side wire types stay named `Volume` regardless; they just deserialise into `StorageBucket` on the mvmd side. Surface this as the first decision (Phase 13.0) before any mvmd code changes.

### D2 — `opendal` is already the workspace-standard storage abstraction, not `object_store`

`mvm/Cargo.toml` line 67 declares `opendal = { version = "0.50", features = ["services-s3"] }` as a workspace dep. The plan was written referencing the Apache `object_store` crate; in practice we use `opendal` for consistency with the existing dep tree.

`opendal` and `object_store` have very similar shapes — both abstract over S3/GCS/Azure/R2/local/in-memory behind a single trait — so the design holds; the implementation imports `opendal::Operator` instead of `object_store::ObjectStore`.

**Action**: every reference to `object_store` in this plan should be read as `opendal` during implementation. The `ObjectStoreBackend` trait impl wraps `opendal::Operator`; `ObjectStoreSpec.url` parses into the appropriate `opendal::services::*` builder; everything else is the same.

### D3 — Sprint 135 in mvmd is the org/workspace tenancy work

`mvmd/specs/plans/24-tenancy-promote-workspace.md`, `25-tenancy-rescope-resources.md`, `26-tenancy-route-restructure.md` land the `Org → Workspace` hierarchy in mvmd this sprint. Plan 45's mvmd-side workstream **follows Sprint 135's pattern** — same `org_id` + `workspace_id` field placement, same RBAC plumbing, same audit-log helper signature.

The "open question — `tenant_id` ↔ `org_id`" called out later in this plan is being resolved by Sprint 135. Plan 45 mvmd code consumes the post-Sprint-135 schema where `(org_id, workspace_id)` are the primary scope.

### D4 — mvm-storage placement via the `mvmctl` facade

mvmd's root `Cargo.toml` declares `mvmctl = { git = "https://github.com/auser/mvm.git", branch = "main" }`. Adding a new `mvm-storage` workspace member to mvm and re-exporting through the `mvmctl` facade is the correct slot — mvmd picks it up via the existing git dep with no extra wiring. **Confirmed.**

### D5 — mvm-side scope reduction (Path C: thin REST client to mvmd)

After D1, on closer reading, most of the original plan's mvm-side surface is unnecessary for a single-VM dev tool. **What a dev box actually needs is mounting a host directory into a microVM.** Provider-backed buckets, client-side AEAD, snapshots, backup policies, key rotation, full RBAC, and cross-workspace isolation are *fleet* concerns that live in mvmd. Letting mvmctl re-implement them on the dev box would:

- Sprawl provider credentials across every dev laptop (defeating mvmd's `bucket_crypto::seal_credentials`).
- Skip mvmd's audit log + RBAC enforcement.
- Force mvm to maintain its own crypto + key-rotation infrastructure independent of mvmd's.
- Add ~150 transitive deps (`opendal` + AEAD crates) for a feature most dev workflows don't exercise.

**Decision (Path C, hybrid)**: mvm ships only `LocalBackend` natively. For provider-backed volumes, `mvmctl` exposes a `--remote` flag that proxies to mvmd's REST API — same wire types, same auth, same audit trail.

```text
mvmctl volume create scratch                              # local, no mvmd needed
mvmctl volume create fixtures --remote \
       --org acme --workspace prod \
       --backend s3 --url s3://acme-fixtures/ \
       --creds-ref aws-prod                              # → POST /api/v1/orgs/acme/workspaces/prod/buckets
mvmctl volume cp ./data fixtures/data.tar --remote        # → mvmd REST data plane
```

Effects on the plan:

- **`mvm-storage` ships only the `VolumeBackend` trait + `LocalBackend`.** No `opendal`, no `aes-gcm`, no `aes-siv`, no `hkdf`. Just `tokio::fs` + `mvm-core` + `mvm-security`.
- **`ObjectStoreBackend` and `EncryptedBackend<B>` move to mvmd** (Sprint 137 W2). The trait stays in `mvm-storage`; mvmd implements its own backings against it.
- **mvmctl gains a small `mvmd_client` module** (~50–100 LoC) using the existing workspace `reqwest` dep. No new crate needed; lives in `crates/mvm-cli/src/mvmd_client.rs`.
- **`mvmctl` config** (`~/.mvm/config.toml`) gains a `[remote]` section: `endpoint`, `api_key_ref`, `default_org`, `default_workspace`. Fully optional — dev boxes that never go remote ignore it.
- **Encryption-at-rest mandate stays mandatory** for both backends, but enforcement points differ: mvm enforces host FDE for `LocalBackend` via `mvmctl doctor`; mvmd enforces provider SSE + client-side AEAD for `ObjectStoreBackend` (since mvmd is the only writer to provider buckets).
- **mvm-side phases 3, 4 (ObjectStoreBackend, EncryptedBackend) move to mvmd Sprint 137 W2** as part of the trait integration. mvm Sprint 46 ends at Phase 11 (live KVM smoke).

The trait abstraction is preserved — both sides implement `VolumeBackend` for what they need; the contract is the same; future backends (NFS, CephFs) can land in either crate as appropriate. This is the cleanest factoring after the `StorageBucket` discovery (D1).

---

## Context

Goal: close the gap between e2b's sandbox storage primitives and mvm/mvmd. Driving question was "where do buckets and volumes belong?" — but reading [e2b's volumes docs](https://www.e2b.dev/docs/volumes) sharpened it:

- **e2b has no buckets.** Drop the bucket workstream entirely. Object storage / artifact capture is non-parity work — revisit independently if ever needed.
- **e2b's Volume is filesystem-semantics + multi-attach + named.** Neither mvm's in-flight share registry nor mvmd's Sprint 39 `VolumeRecord` (exclusive-attach block) match this shape.

The work: introduce a named, virtio-fs-backed, multi-attach `Volume` primitive in mvm, mirrored as `FilesystemVolume` in mvmd, with both a mount path and an out-of-band data plane.

**Are volumes block devices?** No — not in this design. Two distinct shapes:

| Shape | Backing | Attach | Format | Example |
|---|---|---|---|---|
| Block volume | virtio-blk | Exclusive (one writer) | Raw bytes; guest formats | mvmd's existing `VolumeRecord` |
| **Filesystem volume** | **virtio-fs** | **Multi-attach** | **Host owns filesystem; guest mounts a dir** | **e2b Volume — what we're building** |

The existing block `VolumeRecord` in mvmd stays — useful for EBS-style workloads. The new `FilesystemVolume` is parallel, not a replacement.

## What e2b documents (verbatim where possible)

- "Volumes provide persistent storage that exists independently of sandbox lifecycles."
- "Data written to a volume persists even after a sandbox is shut down."
- "one volume shared across multiple sandboxes" — multi-attach explicit.
- Mounted at sandbox creation: `Sandbox.create({ volumeMounts: { '/mnt/my-data': volume } })`.
- Created by name: `Volume.create('my-volume')`.
- "SDK methods are meant to be used when the volume is not mounted to any sandbox" → standalone data plane (`upload`/`download`/`read`/`write`).
- Currently in private beta — match the documented surface, don't over-engineer.

## Locked-in decisions (from grilling)

1. **No buckets** in either repo for this plan. e2b doesn't have them.
2. **Vsock verb rename, no compat shim.** `MountShare`/`UnmountShare` → `MountVolume`/`UnmountVolume`. The in-flight share registry on this branch is untracked, so nothing to deprecate — fold it directly into the volume primitive. **No `share` CLI command. No `ShareEntry` type. Volume is the only concept.**
3. **mvmd is in scope** as part of this plan. Wire format defined in `mvm-core`, consumed by both repos. A companion spec file gets written to `/Users/auser/work/tinylabs/mvmco/mvmd/specs/plans/24-filesystem-volumes-e2b-parity.md`.
4. **`data_disk` (plan 38) stays separate.** Different shape (single-VM exclusive virtio-blk persistent disk). Don't fold in.
5. **No backward compatibility.** Greenfield rename of in-flight surfaces is fine.
6. **Backends are trait-pluggable, not enum-switched.** A `VolumeBackend` trait is the contract; each backing (Local, ObjectStore, future NFS/CephFs) is an impl. Mountability is a method on the trait, not a separate dimension. New backend = implement trait + register constructor.
7. **`object_store` crate as the data-plane primitive.** Use `object_store::ObjectStore` to get S3, R2, Hetzner Object Storage, GCS, Azure, in-memory, local filesystem all behind one trait. We wrap it (don't expose it directly in the wire format) so our domain types are stable and we can add non-`object_store` backends (NFS, CephFs) later.
8. **v1 backend split (post-D5)**: `mvm-storage` ships only the `VolumeBackend` trait + `LocalBackend` impl. `ObjectStoreBackend` and `EncryptedBackend<B>` impls live in mvmd (Sprint 137 W2). Cross-host network filesystems (`Nfs`, `CephFs`) deferred everywhere. See "Backing & durability."
9. **Provider-backed volumes are first-class via mvmd, accessible from mvmctl via `--remote`.** mvm CLI, when given `--remote`, proxies to mvmd's REST API. Wire types come from `mvm-core` so every surface (mvm CLI remote, mvmd CLI, mvmd REST, mvmd-client SDK, mvmd-mcp tools) speaks the same language. mvmctl never holds raw provider credentials; mvmd's sealed-creds infrastructure stays the single source of truth.
10. **Encryption at rest is mandatory.** `LocalBackend` requires host FDE (`mvmctl doctor` enforces on dev box; mvmd-side `LocalVirtiofs` requires host FDE on the mvmd worker). `ObjectStoreBackend` enforces provider SSE + client-side AES-256-GCM (no opt-out) — implemented and enforced in mvmd. See "Encryption at rest" in Security.
11. **Org / workspace scoping from day one.** `Volume` identity is `{ org_id, workspace_id, name }`. Cross-workspace mount denied; cross-org a fortiori denied. mvm CLI uses default `org="local"` / `workspace="default"` for single-VM dev so the wire format stays uniform.
12. **RBAC enforced on every endpoint.** Permission verbs (`fs_volume:create|read|write|delete|mount|snapshot|rotate_key|admin_key`) checked before state-store read or backend dispatch. mvmd reuses existing auth model. mvm-side single-VM dev relies on OS file perms. See "Authorization & permissions."
13. **Snapshots & backup policies in v1.** `FilesystemVolumeSnapshot` parallels Sprint 39's `VolumeSnapshot`. Cron-scheduled `FilesystemBackupPolicy` per-volume. Backend-specific mechanics: native FS snapshot for `LocalBackend`, provider-versioning manifest for `ObjectStoreBackend`. Best-effort consistency in v1.
14. **Key rotation in v1.** Master-key rotation (cheap, online, re-wraps per-volume keys) on 90-day default cadence. Per-volume data-key rotation (expensive, online, re-encrypts) on demand. Master keys versioned (`Active` / `Legacy` / `Revoked`).
15. **Per-workspace quotas in v1** (alongside per-org). Symmetry with per-org; cheap given existing `StorageClassPolicy` patterns.

## Backing & durability

### Two planes, separated

**Data plane** (PUT/GET/LIST/DELETE/RENAME/STAT) — abstracted via the `object_store` crate from Apache. One trait, every major provider for free:
- `LocalFileSystem` (host dir)
- `AmazonS3` — and via S3-compatible endpoints: Hetzner Object Storage, Cloudflare R2, Backblaze B2, MinIO, Wasabi, Ceph RGW
- `GoogleCloudStorage`
- `MicrosoftAzure`
- `Http` (WebDAV-ish)
- `InMemory` (testing)

**Mount plane** (virtio-fs into a guest) — needs a local filesystem path that virtiofsd can export. Object stores have no such path, so they're data-plane-only.

### `VolumeBackend` trait contract

Lives in the new `mvm-storage` crate. All backends implement this; consumers hold `Arc<dyn VolumeBackend>` and dispatch uniformly.

```rust
#[async_trait]
pub trait VolumeBackend: Send + Sync {
    fn kind(&self) -> &'static str;

    // Data plane
    async fn put(&self, key: &VolumePath, data: Bytes) -> Result<(), VolumeError>;
    async fn get(&self, key: &VolumePath) -> Result<Bytes, VolumeError>;
    async fn list(&self, prefix: &VolumePath) -> Result<Vec<VolumeEntry>, VolumeError>;
    async fn delete(&self, key: &VolumePath) -> Result<(), VolumeError>;
    async fn stat(&self, key: &VolumePath) -> Result<VolumeEntry, VolumeError>;
    async fn rename(&self, from: &VolumePath, to: &VolumePath) -> Result<(), VolumeError>;

    // Health
    async fn health_check(&self) -> Result<(), VolumeError>;

    // Mountability (returns Some(path) iff this backend can be virtio-fs-mounted)
    fn local_export_path(&self) -> Option<&Path>;
}
```

### v1 backend matrix

| Backend (impl name) | Wire-format `VolumeBackendConfig` | Mountable | Cross-host | Replication | Status |
|---|---|---|---|---|---|
| `LocalBackend` | `Local { root }` | ✅ | ❌ | ❌ | **v1 ship** |
| `ObjectStoreBackend` (wraps `object_store::ObjectStore`) | `ObjectStore(ObjectStoreSpec)` | ❌ (data-plane only) | ✅ (provider) | ✅ (provider) | **v1 ship** |
| `NfsBackend` | `Nfs { server, export }` | ✅ | ✅ | server-dep | follow-up |
| `CephFsBackend` | `CephFs { fs, subvolume }` | ✅ | ✅ | ✅ (3x typical) | follow-up |

`ObjectStoreSpec` carries `{ url: String, prefix: Option<String>, credentials_ref: Option<SecretRef> }`. The URL scheme picks the backend (`s3://`, `r2://`, `gs://`, `az://`, `file://`, `memory://`). Creds are referenced, never embedded — store via the existing secret mgmt.

### Why wrap `object_store` instead of exposing it directly

- Wire format stays stable even if `object_store` API churns.
- We add domain concerns (size limits, `VolumePath` validation, `mvm-security` policy enforcement, retry/timeout policy).
- Future non-`object_store` backends (NFS, CephFs) implement the same `VolumeBackend` trait — uniform dispatch.

### Replication / triplication

Not a `FilesystemVolume` field. mvmd Sprint 39's `StorageClassPolicy` already has replication factor — reuse it. `FilesystemVolume` references a storage class; the class declares "3-replica CephFs" or "single-host local" or "S3-tier durability". Same Volume API, different durability tiers.

### Object-storage semantic caveats (for `ObjectStoreBackend` users)

Object stores aren't POSIX. Renames are O(copy+delete), random writes to existing files are unsupported, append is iffy. v1 `ObjectStoreBackend` is intentionally data-plane only — guest can't mount an S3-backed volume. Suitable for: pushing build artifacts, pulling fixtures, sharing read-mostly data across sandboxes via SDK calls. *Not* suitable for: live workspace mount during dev iteration. Future: a `mountpoint-s3`-style FUSE bridge could promote `ObjectStoreBackend` to mountable (read-only), but that's out of scope.

## Architecture

### Crate layout

Following the existing pattern (CLAUDE.md: mvm-core is "pure types … no runtime deps"):

- **`mvm-core/src/volume.rs`** (new) — pure wire types only:
  - `VolumeName` (validated identifier; reuses `mvm-core/src/naming.rs` patterns)
  - `Volume`, `VolumeMount`, `VolumePath`, `VolumeEntry`, `VolumeError`
  - `VolumeBackendConfig` enum (declarative shape of a backend, not behavior)
  - `ObjectStoreSpec { url, prefix, credentials_ref }` — URL scheme drives provider
  - All `#[serde(deny_unknown_fields)]` (security claim 5).

- **`crates/mvm-storage`** (new workspace crate) — behavior, **minimal scope per D5**:
  - `VolumeBackend` async trait
  - `LocalBackend` impl — wraps `tokio::fs`; `local_export_path` returns `Some(root)`
  - `make_backend(config: &VolumeBackendConfig) -> Result<Arc<dyn VolumeBackend>>` constructor — only constructs `LocalBackend`; for `VolumeBackendConfig::ObjectStore(...)` returns a clear "not implemented in mvm-storage; use --remote to dispatch through mvmd" error.
  - Deps: `tokio`, `bytes`, `async_trait`, `mvm-core`, `mvm-security`. **No `opendal`, no AEAD crates.**
  - mvmd takes ownership of `ObjectStoreBackend` and `EncryptedBackend<B>` impls of the same trait — see Sprint 137 W2.

- **`crates/mvm-cli/src/mvmd_client.rs`** (new module, ~50–100 LoC) — thin REST client to mvmd for the `--remote` code path. Uses workspace `reqwest`. No new crate.

- **Facade**: `mvmctl::storage` re-export (root `src/lib.rs`). mvmd consumes via existing `mvmctl` git dep — no extra wiring.

```rust
// mvm-core/src/volume.rs
pub struct OrgId(/* validated; UUID or slug */);
pub struct WorkspaceId(/* validated; UUID or slug */);
pub struct VolumeName(/* validated identifier; unique per workspace */);

pub struct Volume {
    pub org_id: OrgId,
    pub workspace_id: WorkspaceId,
    pub name: VolumeName,
    pub created_at: SystemTime,
    pub size_bytes: Option<u64>,
    pub backend: VolumeBackendConfig,
}

pub struct VolumeMount {
    pub volume: VolumeName,         // resolved within instance's workspace
    pub guest_path: GuestPath,      // validated by mvm-security MountPathPolicy
    pub read_only: bool,
}

pub enum VolumeBackendConfig {
    Local { root: PathBuf },
    ObjectStore(ObjectStoreSpec),
    // Future: Nfs { server, export }, CephFs { fs, subvolume }
}

pub struct ObjectStoreSpec {
    pub url: String,                            // s3://, r2://, gs://, az://, file://, memory://
    pub prefix: Option<String>,
    pub credentials_ref: Option<SecretRef>,     // referenced, not embedded
}
```

`VolumeMount` deliberately doesn't carry `org_id` / `workspace_id` — mounts inherit from the instance's scope. Cross-workspace mounting is denied at the mvmd REST layer; locally, mvm's `~/.mvm/config.toml` provides the default scope.

### mvm side — three workstreams

**W-Volume — local volume primitive.**
- Host directory at `~/.mvm/volumes/<name>/`, mode 0700.
- Registry at `~/.mvm/volumes/registry.json`, mode 0700 (matches W1.5 posture).
- Commands: `mvmctl volume create <name> [--size <bytes>]`, `volume ls`, `volume rm <name> [--force]`.
- Implementation: new `crates/mvm-runtime/src/vm/volume_registry.rs`.
- **Replaces** the in-flight share registry. Delete `crates/mvm-runtime/src/vm/share_registry.rs`, `crates/mvm-cli/src/commands/vm/share.rs`, `crates/mvm-guest/src/share.rs` — fold their virtiofsd-spawn logic into the volume path.

**W-DataPlane — local file ops + remote proxy.**
- For local volumes: devs can `cp` directly to `~/.mvm/volumes/<name>/` on the host filesystem. v1 mvm CLI does not ship a separate data-plane subcommand (`cp`/`read`/`write`) for `LocalBackend` — the host FS is the data plane.
- For provider-backed volumes (`--remote`): `mvmctl volume cp ./local volume://name/path --remote` proxies to mvmd's REST data plane (`PUT /api/v1/orgs/{o}/workspaces/{w}/buckets/{name}/files/{path}`). Routing in `mvmctl::mvmd_client`.
- Path safety: confine to volume root, reject `..`, validated via `mvm-security` policy.

**W-RemoteClient — mvmctl as a thin mvmd REST client (post-D5).**
- `mvmctl --remote` flag (or auto-detect when `--org`/`--workspace` are supplied explicitly) routes operations through `mvmctl::mvmd_client`.
- `~/.mvm/config.toml` `[remote]` section: `endpoint`, `api_key_ref`, `default_org`, `default_workspace`. All optional; absent → no remote.
- Operations supported in v1: `volume create|ls|rm|cp|read|write|snapshot create|snapshot ls|snapshot restore` (read-side) plus `attach|detach`. Background-task operations (key rotation, backup-policy management) live in `mvmd-cli`, not mvmctl.
- Reuses workspace `reqwest`. No new crate. Auth via Bearer token resolved through existing secret store.

**W-Mount-API — declarative mount at boot.**
- `mvmctl up` / `mvmctl run` gain repeatable `--volume <name>:<guest_path>[:ro]` (matches e2b's `volumeMounts`).
- Boot path: resolve name → fetch backend via `make_backend(config)` → check `backend.local_export_path()`. If `Some(path)` → spawn `virtiofsd` on that path → boot Firecracker with virtio-fs device → guest agent runs `MountVolume { volume_name, guest_path, read_only }`. If `None` → return clear error (`"volume '<name>' has backend kind=ObjectStore which is data-plane-only and cannot be mounted; use `mvmctl volume cp` instead"`).
- Cap: 16 mounts/VM.

### mvmd side — one workstream

**W-Fleet-Volume — `FilesystemVolume` parallel to `VolumeRecord`.**
- Imports `mvmctl::core::volume::{Volume, VolumeBackendConfig, ObjectStoreSpec, ...}` and `mvmctl::storage::{VolumeBackend, make_backend}` via the existing `mvmctl` git dep.
- New types in `crates/mvmd-gateway/src/state.rs`:
  ```rust
  pub struct FilesystemVolume {
      pub org_id: OrgId,
      pub workspace_id: WorkspaceId,
      pub name: VolumeName,
      pub size_bytes: Option<u64>,
      pub created_at: SystemTime,
      pub backend: VolumeBackendConfig,           // shared with mvm
      pub storage_class: Option<StorageClassRef>, // optional reference to existing Sprint 39 policy
      pub wrapped_key: WrappedKey,                // per-volume AEAD key wrapped under per-org master
  }
  pub struct FilesystemVolumeMount {
      pub volume: VolumeName,
      pub instance_id: InstanceId,                // instance's (org, workspace) must match volume's
      pub guest_path: GuestPath,
      pub read_only: bool,
  }
  ```
- StateStore caches one `Arc<dyn VolumeBackend>` per `FilesystemVolume`; data-plane endpoints dispatch through it.
- New REST endpoints under `/api/v1/orgs/{org}/workspaces/{ws}/fs-volumes`:
  - `POST /fs-volumes` (create), `GET /fs-volumes` (list), `DELETE /fs-volumes/{name}`.
  - `POST /fs-volumes/{name}/mounts` (rejects if `backend.local_export_path().is_none()` OR if instance scope ≠ volume scope), `DELETE /fs-volumes/{name}/mounts/{instance_id}`.
  - **Data plane** (works for any backend): `PUT /fs-volumes/{name}/files/{path}`, `GET /fs-volumes/{name}/files/{path}`, `DELETE /fs-volumes/{name}/files/{path}`, `GET /fs-volumes/{name}/files/{path}?list=true`.
- All routes verify that the auth principal has access to the specified `(org, workspace)` before any state-store read or backend dispatch.
- **Multi-attach by design**: no `attached_instance_id` singleton; `FilesystemVolumeMount` is an N:1 join row.
- Existing `VolumeRecord` (block, exclusive) untouched. Both primitives coexist.
- Scheduler awareness: not in v1 (filesystem mounts have no co-location requirement).
- CLI: extend `crates/mvmd-cli/src/commands/storage.rs` with `fs-volumes` subcommands.

### Companion spec file in mvmd

Write `/Users/auser/work/tinylabs/mvmco/mvmd/specs/plans/24-filesystem-volumes-e2b-parity.md` with the mvmd-side workstream broken into phases (matches mvmd's existing plan format — see `16-dns-volumes-floating-ip.md` for shape: Context, Baseline, MiniMax-style gap table, phase breakdown, exit criteria).

## Provider-backed volumes across the ecosystem

`VolumeBackendConfig` is the single wire type. Every surface re-exposes it consistently:

| Surface | How `VolumeBackendConfig` is exposed |
|---|---|
| `mvmctl volume create` | `[--org <o>] [--workspace <w>]` + `--backend local --root <path>` or `--backend object-store --url <url> --prefix <p> --creds-ref <ref>`. Org/workspace default from `~/.mvm/config.toml`. |
| `mvmd-cli storage fs-volumes create` | Same flag shape; org/workspace required (no defaults). Targets mvmd REST. |
| **mvmd REST** `POST /api/v1/orgs/{org}/workspaces/{ws}/fs-volumes` | Body: `{ name, backend: VolumeBackendConfig, size_bytes?, storage_class? }`. Auth scope must match URL scope. |
| **mvmd-client SDK** (Python/TS) | `client.org("o").workspace("w").fs_volumes.create(name, backend={ kind: "object-store", url: "..." })` — auto-generated from REST schema. |
| **mvmd-mcp** | New tools: `create_fs_volume`, `list_fs_volumes`, `attach_fs_volume`, `volume_upload`, `volume_download`, `volume_list_files`. Workspace defaults from server config; tools accept `workspace` override. |
| **`mkGuest` Nix attrset** | `volumeMounts."/mnt/work" = { volume = "ws"; backend = { kind = "object-store"; url = "..."; }; readOnly = false; };` org/workspace inherited from CLI context unless explicitly overridden via `volume.org`/`volume.workspace`. |
| **`mvm.toml`** | Parallel TOML schema. `[volumes.workspace] backend = { kind = "local", root = "..." }`. Top-level `[scope] org = "..."` / `workspace = "..."` optional. |
| **Volume URL** | Short form `volume://<name>/<path>` — scope from context. Long form `volume://<org>/<workspace>/<name>/<path>` for explicit cross-context tooling. |

One source of truth in `mvm-core`. New backend variants and scope changes automatically propagate to all surfaces.

**MCP tool naming convention**: `create_fs_volume` / `attach_fs_volume` (with `fs_` prefix) avoids collision with mvmd's existing block `VolumeRecord` MCP surface, if any. Confirm naming against `crates/mvmd-mcp/` during implementation.

## Org / workspace scoping

### Hierarchy

`Org > Workspace > Volume`.

- **Org** = billing / auth boundary (top-level tenant).
- **Workspace** = project / isolation boundary within an org.
- **Volume** = resource owned by a workspace; `name` is unique per workspace, can collide across workspaces.

### Isolation rules (enforced in mvmd REST handlers and at vsock-mount time)

- A sandbox in `(org_A, ws_X)` may only mount volumes in `(org_A, ws_X)`.
- Cross-workspace mount within the same org → **denied**.
- Cross-org mount → **denied** (a fortiori).
- mvmd verifies `instance.org_id == volume.org_id && instance.workspace_id == volume.workspace_id` before issuing `MountVolume` to the agent. Trait layer (`mvm-storage`) doesn't know about scopes — enforcement is mvmd's job.

### CLI defaults for single-VM dev

mvm CLI runs locally on a dev box with no real multi-tenancy. To keep the wire format uniform without forcing flags everywhere:
- `~/.mvm/config.toml` default: `org = "local"`, `workspace = "default"`.
- `mvmctl volume create scratch` → creates `local/default/scratch`.
- Override: `mvmctl --org foo --workspace bar volume create scratch`.

### mvmd CLI / SDK

No defaults. Org and workspace are required on every operation. SDK ergonomic chaining (`client.org("o").workspace("w").fs_volumes...`) keeps boilerplate low.

### MCP tools

The MCP server is configured at startup with a default workspace (env var `MVMD_MCP_WORKSPACE=org/ws`). Tools accept an optional `workspace` parameter to override. This matches how LLM agents typically operate within a fixed workspace.

### Nix / mvm.toml

`mkGuest.volumeMounts` and `mvm.toml`'s `[volumes.*]` sections inherit org/workspace from the CLI context (where `mvmctl up` is invoked). Explicit override is supported for tooling that crosses contexts:
```nix
volumeMounts."/mnt/external" = { volume = "shared-fixtures"; org = "other-org"; workspace = "other-ws"; readOnly = true; };
```
Under the isolation rule above, an explicit cross-workspace mount is rejected unless the operator has explicit cross-workspace grant — that's a future ACL feature, out of scope for v1. v1: explicit overrides are allowed only for the local CLI defaults case.

### Quotas / billing

Quota scope is layered:
- **Per-org quota**: total volumes / total bytes across all workspaces — billing-aligned. **v1 ships this**.
- **Per-workspace quota**: lower cap within an org — project-isolation aligned. **v2 follow-up.**

mvmd Sprint 39's `VolumeQuota` (per-tenant) is the existing template; new `FilesystemVolumeQuota` is per-org. Tenant-level quota for the existing block `VolumeRecord` stays untouched.

### Key derivation under scoping

Encryption-at-rest key wrapping uses the scoped identity:
```
per_volume_key = HKDF(master_key, salt = "mvm-fs-volume" || org_id || workspace_id || volume_name)
```
Two volumes with the same `name` in different workspaces have unrelated keys. Master key is per-org (mvmd) or per-host (mvm).

### Open question — relationship to mvmd's existing `tenant_id`

mvmd Sprint 39's `VolumeRecord` is `tenant_id`-scoped. Three resolution paths to pick at implementation time:

1. **`tenant_id` ≡ `org_id`** (rename + alias) — cheapest if semantically equivalent.
2. **Coexist** — `tenant_id` stays as billing identity; `org_id` is added; relationship 1:1 or n:1 depending on mvmd's auth model.
3. **Migration** — `tenant_id` deprecated in favor of `org_id` + `workspace_id` everywhere; `VolumeRecord` schema updated.

**Decision deferred to mvmd implementation**, not blocking on the mvm-side schema. The new `FilesystemVolume` ships org+workspace-scoped from day one regardless.

## Security considerations

The codebase has 7 CI-enforced security claims (CLAUDE.md "Security model"). Volumes must fit that posture without weakening it.

### Guest-side
- `mvm-security::policy::MountPathPolicy` gates every `guest_path`. Extend its deny-list to `/nix`, `/nix/store`, `/nix/var`, `/run/booted-system`, `/run/current-system` (Nix-immutable paths) on top of the existing `/etc`, `/usr`, `/proc`, etc.
- Default mount flags: `nosuid,nodev,noexec` (aligns with claims 1 & 2). `noexec` opt-out via `--allow-exec` for workloads that legitimately run binaries from the volume.
- virtio-fs UID translation: explicitly map mounted volume owner uid to agent uid 901 (claim 7). A host-side root-owned file must not appear root-owned to the guest.
- Read-only enforcement is **defense-in-depth**: mount-level (`-o ro`) AND trait-level (data-plane `put`/`delete`/`rename` reject when `read_only: true` on the mount or on the volume).
- dm-verity rootfs (claim 3) stays untouched — volumes mount under `/mnt/...`, never overlay `/`.

### Host-side
- `~/.mvm/volumes/` directory and `~/.mvm/volumes/registry.json` mode 0700 (matches W1.5).
- virtiofsd runs as a separate unprivileged process (standard practice).
- New `mvm-security::policy::VolumeNamePolicy`: reject control chars, `/`, leading `.`, length cap, reserved names.
- Volume-internal path validation: reject `..`, embedded NULs, absolute paths in volume URLs. Resolve to canonical and assert `starts_with(volume_root)` (defeats symlink escape).
- Reject duplicate `guest_path` within a single VM's mounts (would shadow system paths).
- 16 mounts/VM cap.

### Object-store backend (`ObjectStoreBackend`)
- **Credentials never embedded** in `ObjectStoreSpec`. `credentials_ref: Option<SecretRef>` references the existing secret store. `Debug`/`Display` impls must scrub secrets — add a regression test.
- **TLS mandatory** for `s3://`, `r2://`, `gs://`, `az://`, `https://`. No `?insecure=true` bypass except `memory://` and `file://` schemes. URL scheme validated upfront.
- **SSRF guard**: reject endpoints in link-local / IMDS ranges (`169.254.0.0/16`, `127.0.0.1` except explicit dev opt-in). v1 minimum: assert URL is absolute, scheme allowlisted, host not in deny-list.
- **No bucket lifecycle management** — mvmd assumes the bucket exists with correct ACL. Document this in the volumes guide (operator concern).

### Encryption at rest (mandatory)

Volumes are always encrypted at rest. Two backends, two paths:

**`LocalBackend` — host FDE required.**
- `mvmctl doctor` adds a check: FileVault enabled (macOS) or LUKS-encrypted root (Linux). Refuse `volume create --backend local` if absent.
- Refuse `mvmctl up --volume <local-vol>:<path>` on a non-FDE host (atomic-fail at boot, matches Nix ethos).
- Rationale: industry-standard for ephemeral compute. No second crypto layer to maintain. Aligns with cloud-provider practice.
- *(Future opt-in: `--encrypted-backing` for per-volume LUKS dm-crypt container on Linux. Out of scope for v1.)*

**`ObjectStoreBackend` — two-layer mandatory.**
- **Layer 1 — Provider SSE (mandatory)**: bucket must have SSE enabled. Verified at `volume create` time via `object_store::ObjectStore::list_with_options` / provider HEAD. Reject volume creation if not present. SSE-S3 default; SSE-KMS / SSE-C configurable via `credentials_ref`.
- **Layer 2 — Client-side AEAD (mandatory, no opt-out)**: `EncryptedBackend<B: VolumeBackend>` decorator wraps `ObjectStoreBackend`. AES-256-GCM, per-volume key, random IV per object. Filename encryption via AES-SIV (deterministic, supports lookup). Always on for `ObjectStore`-backed volumes.
- Rationale: belt-and-suspenders. Provider SSE protects the storage tier; client-side AEAD protects against provider misconfiguration / IAM compromise / ACL leak. Cost is bounded by AES-NI hardware (GB/s throughput).

**Key management (mandatory layer 2 only):**
- **mvm-side (host)**: per-host master key at `~/.mvm/keys/master.key` (mode 0600, sealed by host FDE). Per-volume key derived via HKDF with salt `"mvm-fs-volume" || org_id || workspace_id || volume_name`; wrapped key stored in volume registry record.
- **mvmd-side (fleet)**: per-org master key in mvmd's existing secret store (reuse `crates/mvmd-coordinator` secret infrastructure). Per-volume key derived via HKDF with the same salt scheme; wrapped key stored in `FilesystemVolume.wrapped_key` field.
- Two volumes with identical `name` in different `(org, workspace)` scopes have unrelated keys.
- **Crypto primitives**: `aes-gcm` (AEAD), `aes-siv` (filename encryption), `hkdf`, `zeroize` — all already in mvmd's `Cargo.toml` workspace deps.
- **Key rotation**: out of scope for v1. Document. Volume's per-volume key is stable for the volume's lifetime.
- **Decorator pattern in `mvm-storage`**: `EncryptedBackend<B>` is a generic wrapper; `make_backend(VolumeBackendConfig::ObjectStore(...))` automatically returns `EncryptedBackend<ObjectStoreBackend>`. Trait contract preserved; encryption is invisible to callers.

### Wire / fuzzing
- Every new wire type `#[serde(deny_unknown_fields)]` (claim 5).
- Fuzz seeds added under `crates/mvm-guest/fuzz/corpus/fuzz_guest_request/` for `MountVolume` / `UnmountVolume` variants (claim 5).
- `prod-agent-no-exec` CI job (claim 4) must stay green — volume handler depends only on filesystem and vsock; no shell exec.

### Cross-org / cross-workspace isolation (mvmd)
- Volume identity is `(org_id, workspace_id, name)`. Mounting verifies instance and volume share the same `(org_id, workspace_id)` tuple — enforcement in mvmd REST handlers, not in the trait (trait doesn't know scopes).
- Cross-org mount: hard denied at the auth layer (auth principal has no access to the other org).
- Cross-workspace mount within an org: denied by REST handlers; v1 has no cross-workspace ACL grant mechanism (future feature).
- Authorization on every endpoint: principal has read or write access to `(org, workspace)` before any state-store read or backend dispatch.

### Audit
- mvmd audit-logs `create/delete/attach/detach` and data-plane `put/delete/rename`. Reuse existing audit infrastructure.
- mvm-side standalone ops log to the local instance log.

### Resource caps
- `size_bytes` soft cap: `LocalBackend` enforces (refuse `put` past cap; track via `stat`). `ObjectStoreBackend` cannot enforce — provider doesn't know about us. Document; rely on provider-side quotas + monitoring.
- Disk-fill DoS on host: cap is the primary mitigation. Document host-quota requirement when cap is `None`.

### Supply chain
- `object_store` adds a non-trivial dep tree. `cargo deny` + `cargo audit` jobs (claim 7) catch CVEs. Reproducibility double-build (W5.3) sanity-checks determinism stays intact.

## Nix semantics alignment

mvm uses Nix flakes via `mkGuest` (see `public/src/content/docs/guides/nix-flakes.md`). Creation-time mount fits Nix's declarative model. Concrete alignments:

- **Nix store is sacrosanct.** `MountPathPolicy` denies `/nix/store`, `/nix/var`, `/run/booted-system`, `/run/current-system`. Volumes never overlay Nix-managed paths.
- **`mkGuest` API extension** (in `mvm-build` and documented in nix-flakes.md):
  ```nix
  mkGuest {
    # ...
    volumeMounts = {
      "/mnt/work" = { volume = "workspace"; readOnly = false; };
      "/mnt/inputs" = { volume = "fixtures"; readOnly = true; };
    };
  }
  ```
  Nix evaluates → emits into the boot manifest → `mvmctl up` reads it. Standard Nix attrset, supports module composition / merging out of the box.
- **`mvm.toml` parity.** Same shape as `mkGuest.volumeMounts`. Non-Nix users author `mvm.toml`; Nix users declare in the flake; both desugar to the same `Vec<VolumeMount>`.
- **Reproducibility boundary.** Volumes are *the* mutable layer; rootfs stays immutable + verity-protected (claim 3). Volume contents do **not** influence the image hash. Document explicitly so users don't expect content-addressed volume state.
- **Atomic boot semantics.** A required volume missing from the registry → VM refuses to start with a clear error. No silent mount-skip. Matches Nix's "fail loudly on missing inputs."
- **Build vs runtime separation.** Volumes are runtime-only. `mvm-build`'s Nix pipeline (running `nix build` inside Lima) doesn't touch volumes. Keep the two pipelines disjoint.
- **Convention sugar (optional).** `volumeInputs` (read-only) and `volumeOutputs` (read-write) as Nix-side aliases that desugar to `VolumeMount { read_only: bool }`. Sugar only — internal types stay flat.
- **No new network channel.** Volume traffic stays on vhost-vsock + virtiofsd's unix socket. No SSH, no new port (claim 1 alignment + CLAUDE.md "No SSH in microVMs, ever" rule).

## Authorization & permissions

mvmd has an existing auth model (verified at implementation; reuse, don't invent). For volumes:

### Permission verbs

- `fs_volume:create` — create a volume in `(org, ws)`.
- `fs_volume:read` — list, stat, list-files, download.
- `fs_volume:write` — put, delete files, rename within the volume.
- `fs_volume:delete` — destroy the volume itself.
- `fs_volume:mount` — attach to an instance. Requires also `instance:control` on the target instance.
- `fs_volume:snapshot` — create / restore / delete snapshots.
- `fs_volume:rotate_key` — trigger per-volume data-key rotation (admin-level).
- `fs_volume:admin_key` — trigger master-key rotation (org-admin only).

### Role mapping (suggested defaults)

| Role | Permissions |
|---|---|
| Org owner | all `fs_volume:*` across all workspaces in the org |
| Org admin | all except `fs_volume:admin_key` (masters require owner) |
| Workspace admin | `create`, `read`, `write`, `delete`, `mount`, `snapshot` within own workspace |
| Workspace developer | `create`, `read`, `write`, `mount`, `snapshot` (no delete) |
| Workspace viewer | `read` only |

### Enforcement

- Every mvmd REST handler runs auth check **before** state-store read or backend dispatch — `(principal, action, resource)` tuple evaluated.
- Authorization decision is logged in audit alongside the operation result.
- mvm-side (single-VM dev): no permission system; relies on OS file permissions on `~/.mvm/` (mode 0700). Single-user environment.
- MCP tool usage: MCP server runs as a workspace-bound principal; tool authorization is implicit from the principal's role.
- Cross-workspace deny is **separate** from RBAC — even an org-owner cannot mount workspace A's volume into a workspace B instance without an explicit cross-workspace grant (out of scope for v1; see "Out of scope").

## Snapshots & backup policies

### `FilesystemVolumeSnapshot`

Point-in-time copy of a volume. Lifecycle parallels mvmd Sprint 39's `VolumeSnapshot`:

```rust
pub struct FilesystemVolumeSnapshot {
    pub org_id: OrgId,
    pub workspace_id: WorkspaceId,
    pub volume: VolumeName,
    pub snapshot_id: SnapshotId,
    pub state: SnapshotState,           // Pending | Ready | Failed
    pub created_at: SystemTime,
    pub size_bytes: Option<u64>,
    pub backend_kind: &'static str,     // "local" | "object-store"
}
```

### Backend-specific snapshot mechanics

- **`LocalBackend`**: prefer host-FS native snapshot (XFS reflink, Btrfs subvolume, ZFS snapshot, APFS clone — any one detected at `mvmctl doctor`). Fallback: hardlink-based recursive copy (`cp -al` semantics) — atomic-ish, much cheaper than full copy.
- **`ObjectStoreBackend`**: requires bucket versioning enabled. Snapshot creation writes a manifest object listing `(key, version_id)` for every key in the volume's prefix at point-in-time. Restore reads the manifest and provider-side `copy` brings each version back to the live keys.

### Consistency semantics

- v1 snapshots are **best-effort consistent**. Active in-flight writes during snapshot may or may not be captured. Document.
- For *guaranteed* consistency: caller quiesces writers (unmount or pause writers) before snapshot. v1 doesn't enforce; mvmd's lifecycle hooks could in v2.

### `FilesystemBackupPolicy`

Reuses Sprint 39's `BackupPolicy` patterns: cron schedule, retention. Per-volume opt-in.

```rust
pub struct FilesystemBackupPolicy {
    pub org_id: OrgId,
    pub workspace_id: WorkspaceId,
    pub volume: VolumeName,
    pub schedule: CronSchedule,
    pub retention: RetentionPolicy,     // last_n_snapshots | older_than_days
    pub enabled: bool,
}
```

Backup runner is a mvmd background task (existing patterns); creates snapshots on schedule, prunes per retention.

### REST endpoints

- `POST /api/v1/orgs/{o}/workspaces/{w}/fs-volumes/{name}/snapshots` — create
- `GET /…/snapshots` — list
- `DELETE /…/snapshots/{snapshot_id}` — delete
- `POST /…/snapshots/{snapshot_id}/restore` — restore-in-place (or restore to a new volume name)
- `PUT /…/backup-policy` — set policy
- `GET /…/backup-policy` — get
- `DELETE /…/backup-policy` — disable

### CLI

- `mvmctl volume snapshot create <name> [--label <l>]`
- `mvmctl volume snapshot ls <name>`
- `mvmctl volume snapshot restore <name> <snapshot_id>`
- `mvmd-cli storage fs-volumes backup-policy set <org/ws/name> --schedule "0 3 * * *" --retention 7d`

## Encryption-at-rest key rotation

### Two rotation flavors

- **Master-key rotation** (cheap): generate new master key version `v_{n+1}`. Re-wrap every per-volume key under the new master. Mark `v_n` as legacy-readable but not used for new wraps. Fully online — no data movement. Run on a 90-day cadence (configurable via mvmd policy).
- **Per-volume data-key rotation** (expensive): generate new per-volume AEAD key. Re-encrypt every blob/file under the new key. Online, but proportional to volume size. Triggered manually or by policy (suspected key compromise, scheduled annual rotation).

### Master-key versioning

```rust
pub struct MasterKeyRef {
    pub org_id: OrgId,
    pub version: u32,
    pub created_at: SystemTime,
    pub state: MasterKeyState,          // Active | Legacy | Revoked
}

pub struct WrappedKey {
    pub master_key_version: u32,        // selects which master to unwrap with
    pub wrapped: Vec<u8>,
    pub algorithm: WrapAlgorithm,       // e.g. AES-KWP
}
```

- `Active` master is used to wrap new volumes' keys.
- `Legacy` master can unwrap existing wrapped keys but isn't used for new wraps.
- `Revoked` master is gone; unwrap fails. Used as a tombstone after final re-wrap.

### Rotation flows

**Master-key rotation:**
1. Generate `master_v_{n+1}`.
2. Mark `master_v_n` as `Legacy`.
3. Background task re-wraps all `WrappedKey { master_key_version: n }` records to `n+1`.
4. When all re-wrapped, optionally promote `master_v_n` to `Revoked`.
5. Audit-log every step.

**Per-volume key rotation:**
1. Generate new per-volume key `K'`.
2. For each blob/file in the volume: read with old key, write with `K'` (atomic per-key).
3. Update `wrapped_key` to wrap `K'` under current master.
4. After all keys re-encrypted, retire old key.
5. Volume is read-only during rotation? **No** — both keys are valid until rotation completes; reads pick the right key from per-blob metadata. Writes go to `K'`.

### CLI / API

- `mvmd-cli storage rotate-master --org <o>` — admin-only.
- `mvmd-cli storage fs-volumes rotate-key <org/ws/name>` — admin-or-workspace-admin.
- mvmctl-side: same commands, default `org=local`, `workspace=default`.
- REST: `POST /api/v1/orgs/{o}/keys/rotate-master`, `POST /api/v1/orgs/{o}/workspaces/{w}/fs-volumes/{name}/rotate-key`.

### Audit

Every rotation event logged with `(principal, scope, master_version_before, master_version_after, ts, result)`. Per-volume key rotation also records duration and bytes processed.

## Operational concerns

### Failure modes & atomicity

- **Data-plane writes are atomic**: `LocalBackend::put` writes to `<key>.tmp.<random>` then renames; partial writes never visible. `ObjectStoreBackend::put` uses `object_store`'s native put (atomic per-object on all supported providers).
- **Multi-key operations are not atomic**: `mvmctl volume cp -r ./dir volume://foo/dir` is a sequence of `put` calls. Partial-failure semantics: error returned with first-failed key; already-written keys remain. Document — match `aws s3 cp` behavior.
- **Mount failure during boot**: if virtiofsd or `MountVolume` vsock call fails, the VM boot is aborted with a clear error. No partial-mount state. Volume registry record is unchanged.
- **VM crash with active mounts**: mvm-side stale `FilesystemVolumeMount` records cleaned up on next `mvmctl up` (registry reconciliation). mvmd-side: mounts garbage-collected when instance state transitions to `Terminated`.
- **virtiofsd crash mid-session**: guest sees I/O errors on the mount; no automatic remount in v1 (matches `mount` semantics for a vanished server). Documented; manual `mvmctl down && up` recovers.
- **`object_store` retries**: rely on the crate's built-in retry logic (configurable per-store). Default retry policy: 3 attempts with exponential backoff. Fatal errors (auth, 4xx) fail-fast.

### Resource cleanup on delete

- **`mvmctl volume rm <name>`**:
  - `LocalBackend`: backing directory `unlink`-ed (not shredded — host FDE provides at-rest protection; shred is overkill and slow). Document.
  - `ObjectStoreBackend`: every key under the volume's prefix is `delete`-ed. Provider-side soft-delete / versioning may retain copies — operator concern; document.
- **Refuse delete-while-mounted**: `volume rm` errors if any active `FilesystemVolumeMount` references it. `--force` flag available; documents that mounts in active VMs will see I/O errors.
- **Orphan reaper**: `mvmctl cache prune` extended to detect `~/.mvm/volumes/*/` directories not in the registry (or vice versa) and report. Cleanup on operator confirm.

### Concurrency on mvmd

- **State store locking**: existing mvmd `StateStore` patterns (per-resource locks). New `FilesystemVolume` locks: per-volume for delete/attach/detach; volume-list operations are read-locked.
- **Concurrent `put`/`delete` on the same key**: last-writer-wins for `LocalBackend` (POSIX semantics); provider-defined for `ObjectStoreBackend` (S3 strong-read-after-write guarantees apply).

### Observability

- **Metrics** (extend `crates/mvm-core/src/observability/`):
  - Counters: `volume_create_total{org,workspace,backend_kind,result}`, `volume_data_op_total{org,workspace,op,backend_kind,result}`, `volume_mount_total{org,workspace,result}`.
  - Histograms: `volume_data_op_duration_seconds`, `volume_mount_duration_seconds`, `volume_data_op_bytes`.
  - Gauges: `volume_count{org,workspace}`, `volume_bytes_used{org,workspace}` (sampled).
- **Audit logs** (mvmd): every create/delete/attach/detach + data-plane mutation logged with `(principal, org, workspace, volume, op, result, ts)`.
- **Tracing**: data-plane ops opt into `tracing` spans for latency breakdown across backend dispatch / encryption / network.
- **CLI progress**: `mvmctl volume cp` for files > 1 MiB shows a progress indicator (existing `mvm-cli` progress utility).

### CLI ergonomics for large files

- `mvmctl volume cp` streams large files chunked (default 8 MiB) — no full-buffer in memory.
- For `ObjectStoreBackend`, multipart upload is provider-native via `object_store::ObjectStore::put_multipart`.
- Hard cap per single file: 5 GiB (matches S3 single-object practical ceiling for direct PUT).

### Documentation deliverables

- `public/src/content/docs/guides/volumes.md` — **new**, full user guide (create, mount, data-plane, encryption, scoping).
- `public/src/content/docs/guides/nix-flakes.md` — extend with `volumeMounts` attrset reference.
- `public/src/content/docs/reference/cli-commands.md` — `mvmctl volume *` subcommands.
- `CLAUDE.md` — extend Security model section with an 8th claim if encryption-at-rest mandate becomes a CI-gated invariant.
- mvmd: `public/src/content/docs/guides/persistent-volumes.md` (existing) extended with `FilesystemVolume` parallel.
- mvmd: ADR explaining why `VolumeRecord` (block) and `FilesystemVolume` coexist as separate primitives.

## Out of scope (v1) — preserved as future-work backlog

Each item is a real follow-up candidate, not a "rejected" list. Captured here with what it is, why deferred, and the trigger that would pull it back in.

### B1 — Buckets as a separate primitive
- **What**: First-class `Bucket` resource (object storage exposed as its own API, distinct from `Volume`). PUT/GET/LIST/DELETE without filesystem semantics.
- **Why deferred**: e2b doesn't have it; we'd be inventing a new concept just to wrap S3 when `ObjectStoreBackend` already covers the underlying use case. mvmd Sprint 129 plans a "Managed Storage" track that's the natural home.
- **Trigger**: customer/use-case demand for object-storage-as-product (multi-tenant artifact stores, output capture systems) where treating it as a Volume is awkward.

### B2 — Cross-host multi-attach (NFS, CephFs backends)
- **What**: `NfsBackend` and `CephFsBackend` impls of `VolumeBackend`. True cross-host multi-attach so sandboxes on different physical hosts mount the same volume.
- **Why deferred**: significant design + ops work (NFS server provisioning, Ceph cluster ops, scheduler awareness). Wire format already reserves the variants.
- **Trigger**: fleet-mode workloads need same-volume access across hosts; OR triplication / HA durability becomes a hard requirement.

### B3 — Mountable provider-backed volumes (FUSE bridge)
- **What**: Promote `ObjectStoreBackend` from data-plane-only to mountable via `mountpoint-s3` (or `goofys` / `rclone mount`) FUSE-on-host → virtiofsd export.
- **Why deferred**: object-store ↔ POSIX semantics gap (renames, random writes, append) makes this a footgun for general dev iteration; read-only mount via `mountpoint-s3` is most-supported.
- **Trigger**: customer use case for read-mostly mount of S3 datasets directly into sandboxes (e.g., training data).

### B4 — Triplication / replication-factor enforcement
- **What**: `StorageClassPolicy.replication_factor` becomes load-bearing; `FilesystemVolume` placement honors it.
- **Why deferred**: only meaningful with cross-host backends (B2). Replication is a property of those backends, not of the Volume API itself.
- **Trigger**: B2 ships; first replicated backend is added.

### B5 — Hot attach/detach to running instances
- **What**: `mvmctl volume attach <name> --to-running <vm>` mounts a volume to an already-booted VM. Likewise detach without VM teardown.
- **Why deferred**: e2b mounts at sandbox-create only; not a parity item. Firecracker has limited hot-plug support for virtio-fs (varies by kernel).
- **Trigger**: workflow needs a long-lived sandbox to gain/lose volumes without restart.

### B6 — Cross-workspace ACL grants
- **What**: Explicit grant ("workspace A grants workspace B read-only access to volume X"). Sandbox in B can mount A's volume with the granted permission.
- **Why deferred**: full RBAC verb extensions, audit considerations, UI surface. v1's hard-deny is the safe default.
- **Trigger**: multi-team/multi-project workloads need shared dataset access without copying.

### B7 — Volume export / import (offline)
- **What**: `mvmctl volume export <name> > foo.tar.zst.enc` / `import` for offline backup, migration between orgs, disaster recovery seeding.
- **Why deferred**: snapshot+restore covers most disaster-recovery needs in v1. Cross-org migration is a less common operation.
- **Trigger**: customers need to seed a new org from another's data; or air-gapped backup workflows.

### B8 — Volume tags / metadata labels
- **What**: Arbitrary `Map<String, String>` on volumes for filtering, automation, billing-tag passthrough.
- **Why deferred**: common cloud feature, low risk to add later. Not blocking any v1 use case.
- **Trigger**: filtering, automation hooks, or billing systems need to slice on user-defined attributes.

### B9 — Soft-delete / trash
- **What**: `volume rm` moves to trash (recoverable for N days) before hard delete.
- **Why deferred**: hedge against operator error; v1 is `--force`-protected and immediate. Adds metadata + GC complexity.
- **Trigger**: real-world operator-error incident; OR compliance regime requires recoverable deletes.

### B10 — Read-mostly cache layer for `ObjectStoreBackend`
- **What**: Host-side LRU cache to avoid repeated provider GETs for hot objects. Pure performance optimization.
- **Why deferred**: correctness-first in v1; cache invalidation is a known footgun.
- **Trigger**: measured latency / cost pain on object-store-backed workloads.

### B11 — Webhook / event stream
- **What**: Emit `volume.created`, `volume.deleted`, `volume.snapshot.created`, etc. for automation hooks.
- **Why deferred**: integration plumbing; depends on mvmd's broader event-bus story.
- **Trigger**: customers need to trigger external workflows on volume events; OR mvmd ships a general event-bus.

### B12 — `data_disk` virtio-blk persistent disk (plan 38)
- **What**: Single-VM exclusive-attach virtio-blk disk for dev VMs (`mvm.toml [data_disk]` field).
- **Why deferred**: different shape from `Volume` (block + exclusive vs filesystem + multi-attach). Plan 38 is its own track.
- **Trigger**: dev-loop need for a writable persistent block disk independent from filesystem volumes.

### B13 — Scheduler volume-affinity
- **What**: mvmd scheduler considers volume placement when scheduling instances (co-locate instance with volume's host).
- **Why deferred**: filesystem mounts in v1 are local-only or network-FS; co-location matters mainly for cross-host backends (B2).
- **Trigger**: B2 ships; scheduler observes degraded performance from cross-host mounts.

### B14 — Per-volume LUKS dm-crypt for `LocalBackend`
- **What**: Per-volume LUKS-encrypted backing container, layered on top of host FDE.
- **Why deferred**: defense-in-depth, but adds maintenance + macOS-portability complexity. Host FDE is the v1 baseline.
- **Trigger**: threat model demands cryptographic isolation between volumes on the same host (e.g., shared compute with strong tenant isolation).

### B15 — Strong-consistency snapshots (quiesced writers)
- **What**: Snapshot guarantees no in-flight writes are partially captured. Requires writer quiesce protocol (signal mounted VMs to flush + pause writes).
- **Why deferred**: v1 best-effort is acceptable for most use cases; quiesce protocol is non-trivial.
- **Trigger**: workloads with strict transactional semantics (databases on volumes) where partial-write snapshots cause corruption.

### B16 — HSM / KMS-backed master keys
- **What**: Master keys live in an HSM or cloud KMS (AWS KMS, GCP KMS, HashiCorp Vault). mvmd never sees raw master key material.
- **Why deferred**: software-managed keys are sufficient for v1 production; HSM integration adds substantial integration surface per provider.
- **Trigger**: compliance requirement (FIPS 140-2/3, SOC 2 Type II with key-custody control); OR enterprise customer demand.

### B17 — Compression / deduplication
- **What**: Per-blob compression (zstd) and content-addressed deduplication for volume contents.
- **Why deferred**: storage cost optimization; defeats client-side encryption (encrypted bytes don't dedupe). Tradeoff worth thinking about separately.
- **Trigger**: large-data workloads where storage cost dominates; willing to consider per-volume "dedup-friendly" mode (encryption mode change).

### B18 — Volume usage analytics / cost attribution
- **What**: Per-volume bytes-stored × time × tier accounting for billing rollups; per-org / per-workspace cost dashboards.
- **Why deferred**: requires billing system integration; metrics from v1 (`volume_bytes_used` gauge) are the foundation.
- **Trigger**: customer billing needs cost breakdown; OR FinOps initiative.

---

Items B1–B18 should be re-evaluated each sprint. When picking one up, capture the design in a focused spec (mvmd `specs/plans/NN-...md`) rather than expanding this plan retroactively.

## Critical files

**mvm — modify / add (post-D5):**
- `Cargo.toml` (workspace root) — add `crates/mvm-storage` workspace member.
- `crates/mvm-storage/` — **new crate, minimal**: `VolumeBackend` trait, `LocalBackend` impl, `make_backend()` constructor (returns clear error for non-local backends, redirecting to `--remote`). Deps: `tokio`, `bytes`, `async-trait`, `mvm-core`, `mvm-security`. **No `opendal`, no AEAD crates** (those land in mvmd Sprint 137 W2).
- `crates/mvm-cli/src/mvmd_client.rs` — **new module**: thin REST client for `--remote` operations using workspace `reqwest`.
- `src/lib.rs` (root facade) — re-export `mvm-storage` as `mvmctl::storage`.
- `crates/mvm-core/src/lib.rs` — re-export new volume module.
- `crates/mvm-core/src/volume.rs` — **new**: `OrgId`, `WorkspaceId`, `Volume`, `VolumeMount`, `VolumeName`, `VolumePath`, `VolumeEntry`, `VolumeError`, `VolumeBackendConfig`, `ObjectStoreSpec`, `WrappedKey`.
- `crates/mvm-cli/src/config.rs` — extend `~/.mvm/config.toml` schema with `[scope] org` / `workspace` defaults (`local` / `default`).
- `crates/mvm-runtime/Cargo.toml` — depend on `mvm-storage`.
- `crates/mvm-runtime/src/vm/volume_registry.rs` — **new**: persistence, lifecycle, virtiofsd spawn (uses `backend.local_export_path()`).
- `crates/mvm-runtime/src/vm/mod.rs` — wire up new module.
- `crates/mvm-cli/src/commands/vm/volume.rs` — **new**: CLI subcommand with `--backend {local,object-store}` and provider flags.
- `crates/mvm-cli/src/commands/doctor.rs` — extend with FDE check (FileVault on macOS, LUKS on Linux).
- `crates/mvm-cli/src/commands/vm/mod.rs` — register `volume` subcommand.
- `crates/mvm-cli/src/commands/vm/up.rs` — `--volume` flag.
- `crates/mvm-guest/src/vsock.rs` — add `MountVolume`/`UnmountVolume` verbs, drop `MountShare`/`UnmountShare`.
- `crates/mvm-guest/src/lib.rs` — replace `share` module with `volume` module.
- `crates/mvm-guest/src/volume.rs` — **new**: agent-side handler.
- `crates/mvm-security/src/lib.rs` — add `policy::VolumeNamePolicy`; extend `MountPathPolicy` deny-list with `/nix`, `/nix/store`, `/nix/var`, `/run/booted-system`, `/run/current-system`.
- `crates/mvm-build/` — `mkGuest` API extension for `volumeMounts` Nix attrset; emit into boot manifest.
- `public/src/content/docs/guides/nix-flakes.md` — document `volumeMounts` + reproducibility boundary.
- `crates/mvm-guest/fuzz/corpus/fuzz_guest_request/seed-volume-mount` — **new** fuzz seed.

**mvm — delete (committed code from PR #87 "feat(sandbox-sdk): foundation"):**
- `crates/mvm-cli/src/commands/vm/share.rs` (~197 LoC) — also remove `Commands::Share` registration in `crates/mvm-cli/src/commands/mod.rs` (lines 147 + 257).
- `crates/mvm-runtime/src/vm/share_registry.rs` (~269 LoC) — also remove `pub mod share_registry;` from `crates/mvm-runtime/src/vm/mod.rs:15`.
- `crates/mvm-guest/src/share.rs` (~390 LoC).
- vsock `GuestRequest::MountShare` / `GuestRequest::UnmountShare` variants and their `ShareResult` response (`crates/mvm-guest/src/vsock.rs:332-342, 449-463, 1804-1809, 2116-2117`).
- `crates/mvm-guest/src/bin/mvm-guest-agent.rs:1747-1755` agent-side dispatch.
- `crates/mvm-guest/fuzz/corpus/fuzz_guest_request/seed-share-mount` and `seed-share-unmount`.

These are committed via PR #87 (commit `c022a74`) on `feat/sprites-and-upstream-coordination`. The replacement (volume_registry, volume CLI, MountVolume/UnmountVolume verbs) reuses the share work's mechanics; this is a rename + scope-addition refactor, not a behavioural change.

**mvmd — modify:**
- `crates/mvmd-gateway/src/state.rs` — `FilesystemVolume` (with `wrapped_key`, scoping fields), `FilesystemVolumeMount`, `FilesystemVolumeSnapshot`, `FilesystemBackupPolicy`, `MasterKeyRef`, `WrappedKey`, `FilesystemVolumeQuota` (per-org + per-workspace).
- `crates/mvmd-gateway/src/routes/fs_volumes.rs` — **new**: routes for volumes, mounts, snapshots, backup policies, key rotation. Each route runs RBAC check before dispatch.
- `crates/mvmd-coordinator/src/state.rs` — StateStore methods; per-org master key access via existing secret store; backup-policy scheduler hook.
- `crates/mvmd-coordinator/src/backup_runner.rs` — **new** background task: cron-schedule snapshot creation per `FilesystemBackupPolicy`, prune per retention.
- `crates/mvmd-coordinator/src/key_rotation.rs` — **new** background task: 90-day master-key rotation; on-demand per-volume key rotation.
- `crates/mvmd-cli/src/commands/storage.rs` — `fs-volumes`, `fs-volumes snapshot`, `fs-volumes backup-policy`, `fs-volumes rotate-key`, `keys rotate-master` subcommands.
- `crates/mvmd-mcp/` — new MCP tools: `create_fs_volume`, `list_fs_volumes`, `attach_fs_volume`, `volume_upload`, `volume_download`, `volume_list_files`, `create_fs_volume_snapshot`, `restore_fs_volume_snapshot`.
- `crates/mvmd-client/` (Python + TS) — auto-generated types for all new wire types, regen from REST schema.
- `crates/mvmd-coordinator/src/auth.rs` (or wherever existing auth lives) — extend permission catalog with `fs_volume:*` verbs and role mappings.

**mvmd — create:**
- `specs/plans/24-filesystem-volumes-e2b-parity.md` — companion plan spec.

## Verification

1. **Unit / integration tests**:
   - `Volume`, `VolumeMount`, `VolumeBackendConfig`, `ObjectStoreSpec` serde roundtrip + `deny_unknown_fields` enforcement.
   - `VolumeName` validation: slashes rejected, dots rejected, length capped, reserved names rejected.
   - Volume registry: create dup → error; delete while mounted → error.
   - Path safety: `volume cp ../etc/passwd volume://foo/...` rejected; symlink escape rejected.
   - **Trait contract tests** in `mvm-storage`: a generic `assert_backend_contract<B: VolumeBackend>(b)` test fixture (put → get round-trip; list → entries; rename → both keys reflect; delete → not-found; concurrent put/get).
   - Run the contract suite against `LocalBackend` and against `ObjectStoreBackend` with `InMemory` (`memory://`) for fast CI; against `LocalFileSystem` (`file://`) for FS-path coverage.
   - Mount-eligibility: `LocalBackend.local_export_path()` is `Some`; `ObjectStoreBackend.local_export_path()` is `None`; mount API returns clear error for the latter.
2. **Live KVM smoke** (extend existing W3 fixture at `crates/mvm-runtime/src/vm/template/lifecycle.rs` and the plan-41 W3 fixture):
   - Boot dev VM with `--volume scratch:/mnt/scratch`; from inside guest, write `/mnt/scratch/file.txt`.
   - Tear down VM; reboot fresh VM; reattach `scratch`; verify file persists.
   - Boot a *second* VM mounting the same `scratch`; verify it sees the file (multi-attach proof).
   - **Scope isolation**: create volume `scratch` in `(org_a, ws_x)` and `(org_a, ws_y)`; verify they have separate contents and unrelated AEAD keys.
   - **Cross-workspace deny**: try to mount `(org_a, ws_x)/scratch` into an instance in `(org_a, ws_y)` → mvmd rejects.
3. **Standalone data plane**: `mvmctl volume write scratch foo.txt 'hi'` → `mvmctl volume read scratch foo.txt` matches; same against `--read-only` mount fails on guest write.
4. **CLI surface**: `tests/cli.rs` integration tests for help text and arg parsing on new `volume` subcommands.
5. **Security gates**:
   - Negative tests: denied volume names, denied guest paths (including `/nix*`), traversal attempts, oversized writes, symlink escape, duplicate guest_path mounts.
   - Mount flag verification: default `nosuid,nodev,noexec` present; `--allow-exec` flips noexec only.
   - Read-only enforcement at mount layer AND trait layer (defense in depth) — separate tests for each.
   - virtio-fs UID translation: agent uid 901 sees volume files as uid 901, not host uid.
   - Credential scrubbing: `Debug`/`Display` of `ObjectStoreSpec` does not leak secrets — regression test.
   - SSRF guard: reject `s3://...?endpoint=http://169.254.169.254/...` and similar; reject non-https/non-allowlisted schemes.
   - **Encryption-at-rest enforcement** (mandatory):
     - `mvmctl doctor` reports FDE state; `volume create --backend local` and `mvmctl up --volume` reject on non-FDE host.
     - `volume create --backend object-store` rejects when bucket SSE is disabled (test against MinIO with SSE off + on).
     - `EncryptedBackend<ObjectStoreBackend>` round-trips data correctly; **ciphertext-on-disk test**: snapshot the bytes written to the underlying `object_store` and assert no plaintext substring of the input appears.
     - Filename encryption test: putting two distinct files yields two distinct ciphertext keys; same filename → same ciphertext key (SIV determinism).
     - Key wrapping test: `wrapped_key` decrypts correctly with master key; corruption is detected (AEAD tag fails).
   - `cargo deny` and `cargo audit` clean (claim 7).
   - `prod-agent-no-exec` job stays green (volume handler must not pull in `do_exec` — claim 4).
   - vsock fuzz corpus extended with `MountVolume` / `UnmountVolume` seeds (claim 5).
6. **mvmd parity**:
   - REST endpoint integration tests for `fs-volumes` CRUD + data plane.
   - Multi-attach round-trip: create, attach to instance A, attach to instance B, both see same files.
   - `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` clean in both repos.
7. **RBAC tests**:
   - Each permission verb: positive (granted) + negative (denied) tests for every endpoint.
   - Cross-workspace mount denial: even with org-owner role.
   - Audit log records authorization decisions.
8. **Snapshot & backup tests**:
   - `LocalBackend` snapshot via FS-native (Btrfs/ZFS/APFS/XFS-reflink) and via hardlink fallback.
   - `ObjectStoreBackend` snapshot manifest creation + restore against MinIO with versioning enabled.
   - Snapshot of large volume → completes; size_bytes accurate.
   - Backup policy scheduling: cron triggers; retention prunes correctly.
   - Restore-to-new-name creates a parallel volume.
9. **Key rotation tests**:
   - Master-key rotation: pre/post snapshot of `wrapped_key.master_key_version`; old volumes still readable mid-rotation.
   - Per-volume key rotation: data still readable during rotation (mixed-key reads); after completion all blobs use new key.
   - `Revoked` master fails unwrap with a clear error.
   - Audit log records rotation start, completion, bytes processed.

## Implementation order (post-D5)

**mvm Sprint 46 (this plan):**
1. **mvm-core types** (`OrgId`, `WorkspaceId`, `Volume`, `VolumeName`, `VolumeBackendConfig`, `ObjectStoreSpec`, `WrappedKey`, …) + serde roundtrip tests. Shared wire format with mvmd.
2. **`mvm-storage` crate**: `VolumeBackend` trait + `LocalBackend` impl + generic contract test suite. Minimal deps.
3. mvm-runtime: `volume_registry.rs` replacing `share_registry.rs`; virtiofsd spawn helper.
4. mvm-cli: `volume create|ls|rm` subcommands (local only); `mvmctl up --volume` flag; `mvmd_client` module for `--remote` proxy.
5. mvm-guest: `MountVolume`/`UnmountVolume` vsock verbs + agent handler; replace `share` module; fuzz seeds.
6. mvm-security: `VolumeNamePolicy` + `MountPathPolicy` extension for `/nix*` paths.
7. mvm-cli `doctor`: FDE check (warn locally; mvmd-side enforces).
8. mvm-build: `mkGuest.volumeMounts` Nix attrset extension; nix-flakes.md docs.
9. Live KVM smoke (single-host multi-attach for `LocalBackend`); scope-isolation test.

**mvmd Sprint 137 (companion plan 29):**
10. Reconciliation decision (W1): extend `StorageBucket` with `BucketProvider::LocalVirtiofs` (recommended) vs. rename.
11. mvm-storage trait integration (W2): implement `ObjectStoreBackend` (wrapping `opendal::Operator`) and `EncryptedBackend<B>` (AES-256-GCM + AES-SIV + HKDF) of `mvmctl::storage::VolumeBackend`. Wire the bucket data-plane handler to dispatch via `make_backend`.
12. Org/workspace scoping (W3) — depends on Sprint 135 Phase 0010.
13. Snapshots (W4), backup policies (W5), key rotation (W6), RBAC verbs (W7) — incremental.
14. CLI / SDK / MCP surface (W8), ADR (W9), docs (W10).
10. Update `public/src/content/docs/` with volume guide; update CLAUDE.md security model if posture changes.
