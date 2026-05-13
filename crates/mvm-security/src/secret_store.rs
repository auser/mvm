//! Plan 63 W4 — tenant secret storage.
//!
//! Operators provision tenant-scoped secrets (API tokens, OAuth
//! refresh tokens, webhook signing secrets, etc.) via `mvmctl
//! secret put`. The supervisor surfaces them inside a workload's
//! sandbox via `/run/mvm-secrets/<name>` at admission time (plan 37
//! §12 / `mvm-supervisor::keystore::SecretGrant`).
//!
//! ## Storage backends
//!
//! - [`FileSecretStore`] — primary; works on every supported host.
//!   Each secret lives at `<base>/<tenant>/<name>` with mode 0600,
//!   parent dirs mode 0700. Enumeration is a directory scan.
//! - [`KeyringSecretStore`] — used when the OS-native keystore is
//!   reachable. Each secret lives at the keyring entry
//!   `(service="mvm-secrets", user="<tenant>:<name>")`. An index
//!   sidecar at `<base>/<tenant>.json` mirrors the names so `ls`
//!   doesn't depend on backend-specific enumeration (the `keyring`
//!   crate's enumeration is uneven across backends).
//!
//! [`default_secret_store`] auto-picks Keyring if reachable, else
//! File. Set `MVM_SECRET_STORE_BACKEND=file` to pin the file
//! backend (escape hatch for hosts where the keyring's
//! reachability probe lies — Linux CI runners with `libsecret`
//! headers but no live `secret-service` daemon are the canonical
//! case).
//!
//! ## Why two backends
//!
//! `KeyringSecretStore` stores values inside macOS Keychain / Linux
//! Secret Service / Windows Credential Manager — the OS keystore
//! is the strongest at-rest protection available on a non-attested
//! host. But CI Linux runners typically have no D-Bus session;
//! `FileSecretStore` is the dependable fallback that works
//! everywhere.
//!
//! ## What this module does NOT do
//!
//! - **Inject secrets into VMs.** That's the supervisor's
//!   `KeystoreReleaser` (plan-37 §12.2). This module is the
//!   operator-facing CRUD surface; the supervisor pulls from it at
//!   admission.
//! - **Encrypt-at-rest for `FileSecretStore`.** v0 stores raw bytes
//!   at mode 0600. Encryption layers on once the keyring is the
//!   primary backend or operators opt into a master-key wrapping
//!   scheme (W5-adjacent). Documented in ADR-039.
//! - **Multi-host replication.** Single-host only; mvmd's secret
//!   service handles fleets.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretBox};

use crate::keystore::validate_shell_id;

/// Service name for OS-native keyring entries. Distinct from
/// `mvm_security::keystore::KEYRING_SERVICE` so per-tenant master
/// keys (W3) and per-name tenant secrets (W4) don't collide.
pub const KEYRING_SERVICE: &str = "mvm-secrets";

/// Default base dir for [`FileSecretStore`]:
/// `~/.mvm/secrets/`. Per-tenant subdir mode 0700, per-secret file
/// mode 0600.
pub fn default_secrets_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("$HOME unset; cannot locate ~/.mvm/secrets/")?;
    Ok(PathBuf::from(home).join(".mvm").join("secrets"))
}

/// Multi-key tenant-scoped secret store. Separate from
/// [`crate::keystore::KeyProvider`] (which is single-key, tenant-
/// scoped, used for the per-tenant *master DEK* — plan 63 W3).
pub trait SecretStore: Send + Sync {
    /// Store `value` under `(tenant, name)`. Overwrites any
    /// existing value silently — operators rotating a token want
    /// `put` to be a no-fuss replace.
    fn put(&self, tenant: &str, name: &str, value: &SecretBox<String>) -> Result<()>;

    /// Resolve the value stored at `(tenant, name)`. Fails if no
    /// entry exists.
    fn get(&self, tenant: &str, name: &str) -> Result<SecretBox<String>>;

    /// Remove `(tenant, name)`. Fails if the entry doesn't exist —
    /// callers that want "idempotent rm" should check `list` first.
    fn delete(&self, tenant: &str, name: &str) -> Result<()>;

    /// List every secret name stored for `tenant`. Returns an empty
    /// vec when the tenant has no secrets. Does **not** return
    /// values — values never leave the store except via `get`.
    fn list(&self, tenant: &str) -> Result<Vec<String>>;
}

/// File-backed secret store. The primary cross-platform impl —
/// works on every host because it depends only on the local
/// filesystem.
///
/// Layout: `<base>/<tenant>/<name>`. Per-secret file mode 0600;
/// per-tenant dir mode 0700. The base dir is created lazily on
/// first write.
pub struct FileSecretStore {
    base: PathBuf,
}

impl FileSecretStore {
    pub fn with_dir(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    fn tenant_dir(&self, tenant: &str) -> Result<PathBuf> {
        validate_shell_id(tenant)
            .with_context(|| format!("Invalid tenant_id for secret lookup: {tenant:?}"))?;
        Ok(self.base.join(tenant))
    }

    fn secret_path(&self, tenant: &str, name: &str) -> Result<PathBuf> {
        validate_shell_id(name).with_context(|| format!("Invalid secret name: {name:?}"))?;
        Ok(self.tenant_dir(tenant)?.join(name))
    }

    fn ensure_tenant_dir(&self, tenant: &str) -> Result<PathBuf> {
        let dir = self.tenant_dir(tenant)?;
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("creating tenant secret dir {}", dir.display()))?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", dir.display()))?;
        }
        Ok(dir)
    }
}

impl Default for FileSecretStore {
    fn default() -> Self {
        // Falls back to `./.mvm/secrets/` if $HOME is unset; callers
        // that care about absolute paths should construct via
        // `with_dir(default_secrets_dir()?)` and surface the env
        // error.
        let base = default_secrets_dir().unwrap_or_else(|_| PathBuf::from(".mvm").join("secrets"));
        Self::with_dir(base)
    }
}

impl SecretStore for FileSecretStore {
    fn put(&self, tenant: &str, name: &str, value: &SecretBox<String>) -> Result<()> {
        self.ensure_tenant_dir(tenant)?;
        let path = self.secret_path(tenant, name)?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(value.expose_secret().as_bytes())
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("atomic rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    fn get(&self, tenant: &str, name: &str) -> Result<SecretBox<String>> {
        let path = self.secret_path(tenant, name)?;
        let meta = fs::metadata(&path).with_context(|| {
            format!(
                "no secret '{name}' for tenant '{tenant}' (path {})",
                path.display()
            )
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            anyhow::bail!("secret {} has mode 0{mode:o}; require 0600", path.display());
        }
        let bytes =
            fs::read(&path).with_context(|| format!("reading secret {}", path.display()))?;
        let s = String::from_utf8(bytes)
            .with_context(|| format!("secret {} is not valid UTF-8", path.display()))?;
        Ok(SecretBox::new(Box::new(s)))
    }

    fn delete(&self, tenant: &str, name: &str) -> Result<()> {
        let path = self.secret_path(tenant, name)?;
        fs::remove_file(&path).with_context(|| format!("removing secret {}", path.display()))?;
        Ok(())
    }

    fn list(&self, tenant: &str) -> Result<Vec<String>> {
        let dir = self.tenant_dir(tenant)?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            // Skip the `.tmp` files our atomic-write helper may
            // leave behind on crash; they're not real secrets.
            if name.ends_with(".tmp") {
                continue;
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }
}

/// OS-native keystore backend. Each secret entry lives at
/// `(KEYRING_SERVICE, "<tenant>:<name>")`. Names are mirrored to a
/// sidecar JSON index at `<index_dir>/<tenant>.json` so [`list`]
/// returns a deterministic answer independent of backend-specific
/// enumeration behavior.
pub struct KeyringSecretStore {
    /// Where the per-tenant name-index JSON lives. v0 reuses the
    /// same `~/.mvm/secrets/` root as `FileSecretStore` — switching
    /// between backends is a configuration choice, not a layout
    /// migration.
    index_dir: PathBuf,
}

impl KeyringSecretStore {
    pub fn with_dir(index_dir: impl Into<PathBuf>) -> Self {
        Self {
            index_dir: index_dir.into(),
        }
    }

    fn entry(tenant: &str, name: &str) -> Result<keyring::Entry> {
        validate_shell_id(tenant)
            .with_context(|| format!("Invalid tenant for secret entry: {tenant:?}"))?;
        validate_shell_id(name).with_context(|| format!("Invalid secret name: {name:?}"))?;
        let user = format!("{tenant}:{name}");
        keyring::Entry::new(KEYRING_SERVICE, &user)
            .with_context(|| format!("opening keyring entry {KEYRING_SERVICE}:{user}"))
    }

    fn index_path(&self, tenant: &str) -> Result<PathBuf> {
        validate_shell_id(tenant)
            .with_context(|| format!("Invalid tenant for secret index: {tenant:?}"))?;
        Ok(self.index_dir.join(format!("{tenant}.json")))
    }

    fn load_index(&self, tenant: &str) -> Result<Vec<String>> {
        let path = self.index_path(tenant)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw =
            fs::read(&path).with_context(|| format!("reading secret index {}", path.display()))?;
        serde_json::from_slice(&raw)
            .with_context(|| format!("parsing secret index {}", path.display()))
    }

    fn save_index(&self, tenant: &str, names: &[String]) -> Result<()> {
        let path = self.index_path(tenant)?;
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(names).context("serialize index")?;
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("atomic rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        let base = default_secrets_dir().unwrap_or_else(|_| PathBuf::from(".mvm").join("secrets"));
        Self::with_dir(base)
    }
}

impl SecretStore for KeyringSecretStore {
    fn put(&self, tenant: &str, name: &str, value: &SecretBox<String>) -> Result<()> {
        let entry = Self::entry(tenant, name)?;
        entry
            .set_password(value.expose_secret())
            .with_context(|| format!("writing keyring entry for {tenant}:{name}"))?;
        // Update the index. Read-modify-write; if the entry write
        // succeeded but this fails the secret IS stored but won't
        // show up in `ls` until the next put — operators see this
        // via the error chain and can re-run.
        let mut names = self.load_index(tenant)?;
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
            names.sort();
            self.save_index(tenant, &names)?;
        }
        Ok(())
    }

    fn get(&self, tenant: &str, name: &str) -> Result<SecretBox<String>> {
        let entry = Self::entry(tenant, name)?;
        let value = entry
            .get_password()
            .with_context(|| format!("reading keyring entry for {tenant}:{name}"))?;
        Ok(SecretBox::new(Box::new(value)))
    }

    fn delete(&self, tenant: &str, name: &str) -> Result<()> {
        let entry = Self::entry(tenant, name)?;
        entry
            .delete_credential()
            .with_context(|| format!("deleting keyring entry for {tenant}:{name}"))?;
        let names = self.load_index(tenant)?;
        let pruned: Vec<String> = names.into_iter().filter(|n| n != name).collect();
        self.save_index(tenant, &pruned)?;
        Ok(())
    }

    fn list(&self, tenant: &str) -> Result<Vec<String>> {
        self.load_index(tenant)
    }
}

/// Env-var override for [`default_secret_store`]. Accepted values
/// (case-insensitive): `file`, `keyring`, `auto`. Anything else is
/// treated as `auto` with a `tracing::warn`. Documented in the
/// security model section of CLAUDE.md as the escape hatch for
/// hosts where the keyring backend is unreliable (CI Linux runners
/// without a Secret Service, headless servers, etc).
pub const BACKEND_ENV: &str = "MVM_SECRET_STORE_BACKEND";

/// Auto-pick the best available SecretStore for the current host,
/// honoring the [`BACKEND_ENV`] override.
///
/// Order (when env is `auto` or unset): KeyringSecretStore if the
/// OS keystore backend is reachable, else FileSecretStore. Mirrors
/// [`crate::keystore::default_provider`].
///
/// On a host whose keyring's `Entry::new` succeeds but `set_password`
/// later fails (Linux runner with `libsecret` headers but no
/// `secret-service` daemon), set `MVM_SECRET_STORE_BACKEND=file` to
/// pin the file backend up-front.
pub fn default_secret_store() -> Box<dyn SecretStore> {
    match std::env::var(BACKEND_ENV).ok().as_deref() {
        Some(v) if v.eq_ignore_ascii_case("file") => return Box::new(FileSecretStore::default()),
        Some(v) if v.eq_ignore_ascii_case("keyring") => {
            return Box::new(KeyringSecretStore::default());
        }
        Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("auto") => {
            tracing::warn!(
                value = v,
                env = BACKEND_ENV,
                "unrecognized secret-store backend; falling back to auto"
            );
        }
        _ => {}
    }
    if crate::keystore::KeyringProvider::backend_reachable() {
        return Box::new(KeyringSecretStore::default());
    }
    Box::new(FileSecretStore::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_value(s: &str) -> SecretBox<String> {
        SecretBox::new(Box::new(s.to_string()))
    }

    // ──────────────────────────────────────────────────────────────
    // FileSecretStore — exercised on every host
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn file_put_get_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store
            .put("acme", "api_token", &mk_value("supersecret-xyz"))
            .unwrap();
        let got = store.get("acme", "api_token").unwrap();
        assert_eq!(got.expose_secret(), "supersecret-xyz");
    }

    #[test]
    fn file_put_overwrites_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "k", &mk_value("v1")).unwrap();
        store.put("acme", "k", &mk_value("v2")).unwrap();
        assert_eq!(store.get("acme", "k").unwrap().expose_secret(), "v2");
    }

    #[test]
    fn file_get_returns_clear_error_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        let err = store.get("acme", "nope").unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("nope") || s.contains("acme"),
            "want context with tenant or name; got: {s}"
        );
    }

    #[test]
    fn file_get_refuses_world_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "k", &mk_value("v")).unwrap();
        let path = tmp.path().join("acme").join("k");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.get("acme", "k").unwrap_err();
        assert!(err.to_string().contains("0644"), "got: {}", err);
    }

    #[test]
    fn file_delete_removes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "k", &mk_value("v")).unwrap();
        store.delete("acme", "k").unwrap();
        assert!(store.get("acme", "k").is_err());
    }

    #[test]
    fn file_delete_errors_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        assert!(store.delete("acme", "missing").is_err());
    }

    #[test]
    fn file_list_returns_sorted_names() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "zeta", &mk_value("v")).unwrap();
        store.put("acme", "alpha", &mk_value("v")).unwrap();
        store.put("acme", "mike", &mk_value("v")).unwrap();
        let names = store.list("acme").unwrap();
        assert_eq!(names, vec!["alpha", "mike", "zeta"]);
    }

    #[test]
    fn file_list_returns_empty_for_unknown_tenant() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        assert!(store.list("nobody").unwrap().is_empty());
    }

    #[test]
    fn file_list_does_not_include_atomic_tmp_files() {
        // If a put crashes between create+rename, a stray .tmp may
        // be left behind. `ls` must not surface it as a real
        // secret — would confuse operators and break automation.
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        let dir = tmp.path().join("acme");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("real"), b"v").unwrap();
        fs::write(dir.join("stray.tmp"), b"v").unwrap();
        assert_eq!(store.list("acme").unwrap(), vec!["real"]);
    }

    #[test]
    fn file_rejects_unsafe_tenant_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        assert!(store.put("../../etc", "k", &mk_value("v")).is_err());
        assert!(store.put("acme;rm", "k", &mk_value("v")).is_err());
        assert!(store.list("../../etc").is_err());
    }

    #[test]
    fn file_rejects_unsafe_secret_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        // Name validation matches tenant validation —
        // shell-injection-style names must be refused.
        assert!(store.put("acme", "../../etc", &mk_value("v")).is_err());
        assert!(store.put("acme", "foo bar", &mk_value("v")).is_err());
    }

    #[test]
    fn file_put_creates_tenant_dir_at_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "k", &mk_value("v")).unwrap();
        let mode = fs::metadata(tmp.path().join("acme"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn file_put_creates_secret_at_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp.path());
        store.put("acme", "k", &mk_value("v")).unwrap();
        let mode = fs::metadata(tmp.path().join("acme").join("k"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // ──────────────────────────────────────────────────────────────
    // default_secret_store — backend selection
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn default_secret_store_returns_some_impl() {
        // Doesn't panic regardless of host keyring availability.
        let _store = default_secret_store();
    }

    /// End-to-end behavior of the env-var override is exercised by
    /// the integration tests in `tests/audit_emissions_live.rs` (the
    /// sandbox sets `MVM_SECRET_STORE_BACKEND=file` and asserts the
    /// `~/.mvm/audit/secrets.jsonl` shape, which only writes if the
    /// file backend actually took effect). The override threading
    /// goes through `std::env::var`, which is process-global; an
    /// in-process unit test would race with parallel tests that read
    /// `HOME` (we'd have to redirect HOME to observe a file write).
    /// Pinning happens at the CLI subprocess boundary instead.

    // ──────────────────────────────────────────────────────────────
    // KeyringSecretStore index — backend-independent tests
    //
    // Live keyring tests require a backend (D-Bus, Keychain, Cred
    // Mgr) that CI Linux runners often don't have. The substrate
    // tests here exercise the JSON index that powers `list` and
    // the validation guards on (tenant, name).
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn keyring_index_round_trips_through_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyringSecretStore::with_dir(tmp.path());
        store
            .save_index("acme", &["b".to_string(), "a".to_string()])
            .unwrap();
        let loaded = store.load_index("acme").unwrap();
        assert_eq!(loaded, vec!["b", "a"]);
    }

    #[test]
    fn keyring_index_is_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyringSecretStore::with_dir(tmp.path());
        assert!(store.load_index("nobody").unwrap().is_empty());
    }

    #[test]
    fn keyring_entry_validates_tenant_and_name() {
        // The shape validator runs before any keyring backend call,
        // so these reject without needing a live keystore.
        assert!(KeyringSecretStore::entry("../etc", "k").is_err());
        assert!(KeyringSecretStore::entry("acme", "../etc").is_err());
        assert!(KeyringSecretStore::entry("foo bar", "k").is_err());
        assert!(KeyringSecretStore::entry("acme", "").is_err());
    }
}
