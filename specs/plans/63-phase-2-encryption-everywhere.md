# Plan 63 — Phase 2: encryption everywhere (volumes, snapshots, secrets) + key rotation

> Expands plan 60 §"Phase 2 — Encryption everywhere" with a concrete
> v2-side workplan that reconciles what's already shipped vs. what's
> left. Phase 2 prep substrate (`mvm-security::snapshot_crypto`,
> `mvm-security::keystore`) landed in commit `a9386f8` on 2026-05-11.

## State of play (2026-05-11)

### Already shipped — Phase 2 substrate

These were *thought* to be deferred in CHANGELOG.md but are actually
in `origin/main`:

- `mvm-core::domain::volume` carries the full wire-type surface:
  `OrgId`, `WorkspaceId`, `Volume`, `VolumeName`,
  `VolumeBackendConfig` (Local + ObjectStore variants), `ObjectStoreSpec`,
  `WrappedKey { master_key_version, wrapped, algorithm }`, `WrapAlgorithm`,
  `MasterKeyRef { org_id, version, created_at, state }`,
  `MasterKeyState { Active, Legacy, Revoked }`, `VolumeError`,
  `MountPolicy`, etc. — Sprint 49 / plan 45 §D5 work that landed
  before the cutover.
- `mvm-storage` ships the `VolumeBackend` trait + `LocalBackend` impl
  + the `contract::assert_backend_contract` generic test fixture used
  by both mvm and mvmd's impls.
- `mvm-security::snapshot_crypto` ships AES-256-GCM `encrypt`/`decrypt`
  over `&[u8]` with 10 tests including tampered-ct rejection and
  nonce-uniqueness (Phase 2 prep, commit `a9386f8`).
- `mvm-security::keystore` ships `KeyProvider` trait returning
  `Zeroizing<Vec<u8>>` + `EnvKeyProvider` + `validate_shell_id` +
  `hex_decode` (Phase 2 prep, commit `a9386f8`).
- `mvm-security::snapshot_hmac` ships HMAC-SHA256 over snapshot
  artifacts (W4 from plan 41, already wired into the FC + microsandbox
  start/restore paths).
- `mvm-cli::commands::storage` ships `mvmctl volume create/ls/rm`,
  `mvmctl up --volume <name>:<path>`, `volume_registry`,
  `MountVolume`/`UnmountVolume` vsock verbs.

### Convergence rule from plan 45 §D5 / Path C

`EncryptedBackend<B>` decorator + `ObjectStoreBackend` impl + AEAD/
AES-SIV/HKDF crypto code + `opendal` dep — **all live in mvmd, not
mvm**. mvm's `mvm-storage::make_backend` returns
`VolumeError::UnsupportedBackend` for `ObjectStore`, redirecting to
`mvmctl --remote` (proxies through mvmd).

This collapses the mvm-side Phase 2 scope to: **secrets handling +
key rotation + encryption-at-rest on local snapshots/volumes**. Wide
encryption-at-rest for remote volumes is mvmd's job.

### Remaining Phase 2 work — mvm-side

Six workstreams. Each is independently shippable on its own PR.

## W1 — `mvm-security::key_rotation` (5 days)

**Goal**: DEK re-wrap on KEK rotation; LUKS2 keyslot rotation;
snapshot KEK rolling. Pure crypto + key-management primitives sitting
on top of the substrate already shipped.

**Action**:

- New `mvm-security::key_rotation` module.
- `rewrap_dek(wrapped: &WrappedKey, old_master: &[u8], new_master:
  &[u8], new_version: u32) -> Result<WrappedKey>` — unwrap with old
  master, re-wrap with new master, update `master_key_version`.
  Algorithm-agnostic (the `WrapAlgorithm` enum dispatches; today only
  `Aes256GcmHkdfSha256` exists).
- `rotate_master_key(active_dir: &Path) -> Result<MasterKeyRef>` —
  generates a new master key version, marks the prior one `Legacy`,
  writes the new `MasterKeyRef` to the keystore. Idempotent on
  repeated invocations.
- `migrate_wrapped_keys(volumes: &[&Volume], from: u32, to: u32, …)`
  — walks every volume's `WrappedKey`, re-wraps to the new master,
  writes the updated record. Resumable: if interrupted halfway,
  re-running picks up where it left off (record-by-record commit).
- LUKS2 keyslot rotation: `rotate_luks_slot(device: &Path, old_passphrase:
  &[u8], new_passphrase: &[u8]) -> Result<()>` shelling out to
  `cryptsetup luksChangeKey`. Pure Rust LUKS isn't worth the closure
  weight — same call we'd make from a script.
- Snapshot KEK rolling: a snapshot's HMAC key is one byte slot;
  rolling it means resealing the snapshot under the new key.
  `reseal_snapshot(snapshot_dir: &Path, old_key: &[u8], new_key: &[u8])
  -> Result<()>` — verifies under old, reseals under new, atomic
  rename. Shares the seal logic with `mvm_security::snapshot_hmac::seal`.

**Exit tests**:

- `rewrap_dek_round_trips_through_rotation` — wrap, rotate, unwrap;
  plaintext recoverable.
- `migrate_wrapped_keys_idempotent_on_interrupt` — kill after
  re-wrap-2-of-5, restart, all 5 land correctly without re-doing 1+2.
- `rotate_luks_slot_rejects_wrong_old_passphrase`.
- `reseal_snapshot_rejects_tampered_after_rotation`.
- Proptest over `rewrap_dek` (100 cases).

**Risk**: LUKS2 shell-out behavior across distros — pin
`cryptsetup --version` and feature-gate older versions out.

## W2 — `secrecy::SecretBox<T>` pass + CI lint (2 days)

**Status (2026-05-11)**: ✅ shipped. The xtask CI lint
(`check-no-display-on-secret-types`) shipped with the Phase 2 prep
commit `a9386f8` and runs as a workspace gate on every PR. The
wrapping-pass tail landed in commit `b9e4e64`:
`mvm_security::snapshot_hmac::load_or_init_key` returns
`SecretBox<[u8; HMAC_KEY_BYTES]>` and the legacy
`mvm::security::keystore::KeyProvider` migrated to
`SecretBox<Vec<u8>>` matching `mvm_security::keystore::KeyProvider`.
`WrappedKey.wrapped: Vec<u8>` reads stay deferred to W1's
`rewrap_dek` use sites — no consumer exists yet.

**Goal**: Every secret-carrying type wraps `secrecy::SecretBox<T>`
(or `secrecy::Secret<T>` for sized) so accidental `Debug`/`Display`
is a compile error.

**Action**:

- Add `secrecy = "0.10"` to workspace deps.
- Audit current secret-carrying surfaces and wrap them:
  - `KeyProvider::get_data_key` already returns `Zeroizing<Vec<u8>>` —
    swap to `SecretBox<Vec<u8>>` (compatible: SecretBox guarantees
    zeroize on drop).
  - `mvm_security::keystore::EnvKeyProvider` — wrap reads.
  - `mvm_security::snapshot_hmac::default_key_path` returns Vec; wrap
    callers' uses where the key bytes live.
  - Any `wrapped_key.wrapped: Vec<u8>` field reads — wrap at use site.
- New `xtask` subcommand `xtask check-no-display-on-secret-types`
  that uses `syn` to walk `crates/*/src/**/*.rs` and reject any
  `derive(Debug)` or `impl Display` on a type whose name contains
  `Secret|Key|Password|Token` (case-insensitive). Conservative
  heuristic; opt-out via `// allow(secret-debug): <why>` attribute.
- Wire the xtask into `.github/workflows/security.yml`.

**Exit tests**:

- `xtask check-no-display-on-secret-types` runs on every PR
- Manual audit confirms every `KeyProvider`, `WrappedKey`, snapshot
  HMAC key call site flows through `SecretBox`.
- Adding a test type `TestSecretKey { #[derive(Debug)] }` makes the
  xtask fail in a meaningful way.

## W3 — `keyring` integration + `FileKeyProvider` (3 days)

**Status (2026-05-11)**: ✅ shipped. `keyring = "3"` lifted into
workspace deps, `mvm_security::keystore` gained `FileKeyProvider`
+ `KeyringProvider` + `default_provider()` + `has_key()`, all
returning `SecretBox<Vec<u8>>` and exercised by 11 new unit tests
(file mode 0600/0400/0644 paths, wrong-length file rejection,
shell-id validation on both providers, default-provider auto-
detection). The macOS Keychain `Entry::new_with_target` and the
Linux/Windows fallback are both wired.

**Dead-code finding**: the W3 audit also surfaced that the legacy
`crates/mvm/src/security/keystore.rs` + the `crates/mvm/src/vm/{instance,
pool,tenant,bridge,disk_manager,sleep,hostd}/` trees are *orphaned*
— neither `crates/mvm/src/security/mod.rs` nor
`crates/mvm/src/vm/mod.rs` declares them, so they never get
compiled. The "consolidate consumers + delete legacy keystore" sub-
goal from W3's original framing is moot: the legacy is already
dead. Cleaning up the dead trees themselves is out of W3's scope —
opening a follow-up plan for the dead-tree sweep would be tidier
than expanding W3.

**Goal**: `KeyProvider` impls beyond `EnvKeyProvider` so v1 users can
provision keys via the OS-native keystore (macOS Keychain / Linux
Secret Service / Windows Credential Manager) or via key files.

**Action**:

- Re-enable `keyring = "3"` in workspace deps (commented out in v1's
  root `Cargo.toml`; check why).
- New `mvm_security::keystore::KeyringProvider` — reads keys via
  `keyring::Entry::new("mvm", &tenant_id).get_password()`. Wraps in
  `Zeroizing`/`SecretBox`. Per-tenant entry; uses `validate_shell_id`
  to prevent injection through the tenant_id.
- New `mvm_security::keystore::FileKeyProvider` — reads `/var/lib/mvm/keys/<tenant_id>.key`
  with `std::fs::read` (replaces v1's `xxd` shell-out, which depended
  on the now-gone Lima shell). Asserts file mode is `0600` or `0400`
  before reading; warns at WARN level if looser.
- `mvm_security::keystore::default_provider()` — auto-detects:
  `KeyringProvider` if available + a key for the tenant exists,
  otherwise `FileKeyProvider` if `/var/lib/mvm/keys/` is present,
  otherwise `EnvKeyProvider`. Same shape as v1's helper.
- macOS: use `keyring::Entry::with_target("mvm", "mvm-tenant-keys", &tenant_id)`
  per macOS Keychain's name-disambiguation conventions.

**Exit tests**:

- `keyring_provider_round_trips_through_os_keystore` (gated on
  presence of a writable keychain — skip cleanly on CI Linux without
  D-Bus / Secret Service).
- `file_key_provider_rejects_world_readable_keys` (mode 0644 fails
  closed; 0600 / 0400 passes).
- `default_provider_falls_through_to_env_when_nothing_else`.
- Cross-platform smoke: `cargo test -p mvm-security -- keystore::`
  passes on Linux, macOS, Windows. Windows uses Credential Manager
  via `keyring`'s default backend.

**Risk**: macOS Keychain UX divergence (interactive prompts for
unlock under some configurations). The trait abstracts this; smoke
tests stay gated per-platform.

## W4 — `mvmctl secret` CLI surface (2 days)

**Goal**: `mvmctl secret put api_token --tenant t1 --value $API_TOKEN`,
`mvmctl secret get api_token --tenant t1`, `mvmctl secret rm`,
`mvmctl secret ls --tenant t1`.

**Action**:

- New subcommand under `mvm-cli::commands::secret`:
  - `put` reads value from stdin if `--value -` or `--value-file`;
    refuses to log the value at any level.
  - `get` writes value to stdout *only* when stdout is not a TTY
    (refuses for interactive shells unless `--force`) and the user is
    the owner. Stderr message names what's happening so a script
    using `$()` knows what came through.
  - `ls` lists tenant-scoped key names (not values).
  - `rm` removes a single key.
- Plumbing: every subcommand resolves to `default_provider()` from W3.
- Audit log: every `put`/`rm`/`get` emits an entry into
  `~/.mvm/audit.log` carrying `(tenant, key_name, action, timestamp,
  outcome, pid)` — never the value itself.

**Exit tests**:

- `secret_put_refuses_to_log_value`.
- `secret_get_refuses_tty_without_force`.
- `secret_ls_does_not_leak_value_lengths`.
- `mvmctl secret put --tenant ../etc` is rejected with shell-injection
  error from `validate_shell_id`.
- `audit log carries every put and never the value`.

## W5 — `mvmctl snapshot save/load` AES-GCM wiring (3 days)

**Goal**: `mvmctl snapshot save <vm>` produces an AEAD-encrypted
snapshot bundle; `mvmctl snapshot load <bundle> --vm <name>` decrypts
and restores. Wraps the existing snapshot pipeline (already shipping
HMAC integrity from W4 of plan 41).

**Action**:

- Snapshot artifacts (`vmstate`, `mem`, sidecar metadata) are
  encrypted under a per-tenant DEK before sealing the HMAC.
- DEK comes from `WrappedKey` resolved via `KeyProvider`; HMAC key
  remains in `~/.mvm/snapshot.key` (advisory cache; bound to the
  bundle via length-prefix as today).
- `mvmctl snapshot save` workflow:
  1. Pause the VM (already exists)
  2. Capture `vmstate` + `mem` (already exists)
  3. **NEW**: AEAD-encrypt each artifact in 1 MiB chunks under the
     DEK (chunk-level integrity already covered by GCM's per-chunk
     tag; chunk-boundary HMAC chain via existing snapshot_hmac)
  4. Write `bundle.tar` (encrypted artifacts + manifest)
  5. Seal HMAC sidecar (existing path)
- `mvmctl snapshot load` is the inverse: verify HMAC → decrypt → restore.
- Compatibility: snapshots taken under v2 carry a `version: 2` field
  in the manifest; v1-shape snapshots (HMAC-only, unencrypted) refuse
  to load unless `--unencrypted` is passed (one-time migration escape).

**Exit tests**:

- `snapshot_save_then_load_round_trip` — full cycle, file persists
  intact in guest.
- `snapshot_load_rejects_v1_shape_without_unencrypted_flag`.
- `snapshot_load_rejects_tampered_artifact` (already covered by
  HMAC; new test pins the AEAD layer too).
- `snapshot_load_rejects_wrong_dek` (authentication tag mismatch).

**Risk**: Performance — 1 MiB AEAD chunking adds ~10% overhead on
small snapshots, ~3% on large. Acceptable; record the cost in the
ADR.

## W6 — ADR-039: "Encryption substrate" + CHANGELOG entry (1 day)

**Goal**: Single ADR documents (a) what mvm vs. mvmd encrypts,
(b) which algorithms are pinned and why (AES-256-GCM, HKDF-SHA256,
no AES-SIV in mvm — that's mvmd's territory per plan 45 §D5),
(c) key-rotation invariants (resumable, idempotent on interrupt),
(d) the secrets-in-types contract (`SecretBox<T>`, no Debug/Display).

**Action**:

- Write `specs/adrs/039-encryption-substrate.md` against the ADR
  template.
- Add a "Phase 2 — encryption everywhere" entry to `CHANGELOG.md`
  under `[Unreleased]`.

## Phasing

W1–W3 are independent and can land in any order. W4 (CLI) depends on
W3 (keyring). W5 (snapshot encryption) depends on W1 (key_rotation
DEK shape) and W3 (provider). W6 (docs) closes the phase.

Suggested order: **W2 → W3 → W1 → W4 → W5 → W6** (smallest
surface area to largest; CLI lands after keystore so users have a
working `mvmctl secret put` from the first day they have keyring
integration).

Effort estimate: 16 days (W1 5, W2 2, W3 3, W4 2, W5 3, W6 1) — fits
in one 3-week sprint.

## Cross-repo dependencies

- `mvm-storage::EncryptedBackend<B>` lives in mvmd (plan 45 §D5).
  Phase 2 mvm-side **does not block** on mvmd — the local-volume
  encryption shape closes locally; remote-volume encryption layers
  on mvmd's side independently.
- `keyring = "3"` is the same major mvmd will use; coordinating dep
  bumps avoids dual-major-version warnings in `cargo deny`.

## Non-goals (explicit)

- **Object-store encryption.** Lives in mvmd per Path C.
- **TPM2 / SEV-SNP / TDX key attestation.** Plan 60 Phase 3 territory;
  this phase ships software-managed master keys.
- **End-to-end network encryption.** Plan 60 Phase 3 network
  isolation; orthogonal.
- **Encrypted-by-default for all snapshots.** v0.14 → v0.15 flip:
  `--unencrypted` opt-out exists for one release, then the default
  flips to "encrypted required" in v0.16. Migration guide for v1
  snapshots already in `MIGRATING-FROM-V1.md`.
- **Per-volume LUKS at the host level.** mvmd's job (`StorageBucket`
  ↔ filesystem volume convergence per plan 45 §D1).

## Success criteria

By Phase 2 close, the project can claim:

1. *Tenant data encryption keys can be rotated without downtime and
   without re-encrypting data* (W1 — DEK re-wrap on KEK rotation).
2. *Local snapshots are AEAD-encrypted at rest under a per-tenant
   DEK with HMAC integrity* (W5 + existing W4 of plan 41).
3. *Tenant secrets are managed through OS-native keystores on every
   supported host* (W3 — keyring on macOS / Linux / Windows).
4. *Secret-carrying types cannot accidentally log their contents*
   (W2 — `SecretBox<T>` + CI lint).
5. *`mvmctl secret put` is the prod-safe surface; values never
   appear in logs or shell history* (W4).
6. *The encryption substrate ADR fixes the trust boundary between
   mvm and mvmd, eliminating "where does this crypto live" ambiguity*
   (W6).

Closes the substrate piece of plan 60 Phase 2. Phase 3 (network
isolation) and Phase 4 (multi-tenant + release) are the next
sprints.
