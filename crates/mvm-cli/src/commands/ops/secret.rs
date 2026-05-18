//! Plan 63 W4 — `mvmctl secret put/get/ls/rm` CLI surface.
//!
//! Local CRUD for secret namespaces. Values never appear in
//! logs, error chains, or process listings — `put` accepts the
//! value via flag, stdin, or file; `get` writes to stdout only when
//! it's not a TTY (so a script using `$(mvmctl secret get …)`
//! works but an interactive `mvmctl secret get foo` doesn't dump
//! the value into the user's terminal).
//!
//! ## Audit
//!
//! Every put/get/delete/list emits one JSON line to
//! `~/.mvm/audit/secrets.jsonl` carrying
//! `(timestamp, action, namespace, name, outcome, pid, error?)`.
//! Values are never logged. The audit file is NOT chain-signed in
//! v0 — that's plan 64's territory (plan-64 audit chain covers
//! workload-admission events; secret events get their own stream).
//!
//! ## Backend choice
//!
//! Goes through [`secret_store::default_secret_store`]:
//! - `KeyringSecretStore` when the OS keystore is reachable.
//! - `FileSecretStore` everywhere else (CI Linux, headless hosts).
//!
//! Tests inject `FileSecretStore::with_dir` via [`run_with_store`].

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args as ClapArgs, Subcommand};
use mvm_plan::TenantId;
use mvm_security::secret_store::{self, SecretStore};
use mvm_supervisor::{EventCategory, FileAuditSigner, Recorder};
use secrecy::{ExposeSecret, SecretBox};

use mvm_core::user_config::MvmConfig;

use super::Cli;
use crate::commands::vm::audit_chain::default_audit_dir;
use crate::commands::vm::host_signer;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: SecretAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum SecretAction {
    /// Store a local secret. Value source: `--value <V>` (inline,
    /// shell-history risk), `--value -` (read from stdin), or
    /// `--value-file <PATH>`. The value never appears in logs.
    Put {
        /// Name to store the secret under (alphanumeric + `_-`).
        name: String,
        /// Local namespace for this secret. Fleet tenant secrets are managed by mvmd.
        #[arg(long, default_value = "local")]
        tenant: String,
        /// Inline value. Pass `-` to read from stdin (preferred
        /// when scripting; avoids shell-history exposure).
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        /// Read value from a file on disk.
        #[arg(long)]
        value_file: Option<PathBuf>,
    },

    /// Retrieve a local secret. Writes the raw value to stdout
    /// (no trailing newline) only when stdout is not a TTY. Pass
    /// `--force` to bypass the TTY guard.
    Get {
        name: String,
        #[arg(long, default_value = "local")]
        tenant: String,
        /// Bypass the TTY guard. Use with care — surfaces the
        /// value on the user's terminal.
        #[arg(long)]
        force: bool,
    },

    /// List secret names stored for a tenant.
    Ls {
        #[arg(long, default_value = "local")]
        tenant: String,
    },

    /// Remove a tenant secret.
    Rm {
        name: String,
        #[arg(long, default_value = "local")]
        tenant: String,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let store = secret_store::default_secret_store();
    run_with_store(store.as_ref(), args)
}

/// Same dispatch as [`run`] but takes an injected store. Test
/// seam for `FileSecretStore::with_dir(<tempdir>)`.
pub(in crate::commands) fn run_with_store(store: &dyn SecretStore, args: Args) -> Result<()> {
    let audit = AuditLog::default()?.with_optional_recorder();
    match args.action {
        SecretAction::Put {
            name,
            tenant,
            value,
            value_file,
        } => cmd_put(store, &audit, tenant, name, value, value_file),
        SecretAction::Get {
            name,
            tenant,
            force,
        } => cmd_get(store, &audit, tenant, name, force),
        SecretAction::Ls { tenant } => cmd_ls(store, &audit, tenant),
        SecretAction::Rm { name, tenant } => cmd_rm(store, &audit, tenant, name),
    }
}

// ============================================================================
// Subcommand handlers
// ============================================================================

fn cmd_put(
    store: &dyn SecretStore,
    audit: &AuditLog,
    tenant: String,
    name: String,
    value: Option<String>,
    value_file: Option<PathBuf>,
) -> Result<()> {
    let result = (|| {
        let raw = resolve_value(value, value_file)?;
        let secret = SecretBox::new(Box::new(raw));
        store.put(&tenant, &name, &secret)
    })();
    audit.record("put", &tenant, &name, &result)?;
    result?;
    eprintln!("Stored secret '{name}' for tenant '{tenant}'.");
    Ok(())
}

fn cmd_get(
    store: &dyn SecretStore,
    audit: &AuditLog,
    tenant: String,
    name: String,
    force: bool,
) -> Result<()> {
    if std::io::stdout().is_terminal() && !force {
        let err: Result<()> = Err(anyhow::anyhow!(
            "refusing to print secret to an interactive terminal; \
             redirect stdout to a file/pipe or pass `--force`"
        ));
        audit.record("get", &tenant, &name, &err)?;
        return err;
    }
    let result = store.get(&tenant, &name);
    audit.record("get", &tenant, &name, &result.as_ref().map(|_| ()))?;
    let value = result?;
    // Stderr message names what's happening so a `$()` capture knows
    // what came through. The actual value goes to stdout, raw, no
    // trailing newline.
    eprintln!("Wrote secret '{name}' for tenant '{tenant}' to stdout.");
    std::io::stdout()
        .write_all(value.expose_secret().as_bytes())
        .context("writing secret to stdout")?;
    Ok(())
}

fn cmd_ls(store: &dyn SecretStore, audit: &AuditLog, tenant: String) -> Result<()> {
    let result = store.list(&tenant);
    audit.record("list", &tenant, "*", &result.as_ref().map(|_| ()))?;
    let names = result?;
    if names.is_empty() {
        eprintln!("No secrets stored for tenant '{tenant}'.");
        return Ok(());
    }
    for name in &names {
        // Names ONLY. Never values. Avoid even the names' lengths
        // being implicit value-length signals: the names are
        // user-chosen identifiers, not derived from values.
        println!("{name}");
    }
    Ok(())
}

fn cmd_rm(store: &dyn SecretStore, audit: &AuditLog, tenant: String, name: String) -> Result<()> {
    let result = store.delete(&tenant, &name);
    audit.record("delete", &tenant, &name, &result)?;
    result?;
    eprintln!("Removed secret '{name}' for tenant '{tenant}'.");
    Ok(())
}

// ============================================================================
// Value resolution — flag / stdin / file
// ============================================================================

fn resolve_value(value: Option<String>, value_file: Option<PathBuf>) -> Result<String> {
    match (value, value_file) {
        (Some(v), None) if v == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading secret value from stdin")?;
            // Strip a single trailing newline — `echo "x" | mvmctl
            // secret put …` shouldn't include the LF in the stored
            // value.
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            Ok(buf)
        }
        (Some(v), None) => Ok(v),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading secret value from {}", path.display())),
        (None, None) => {
            anyhow::bail!(
                "no value source — pass --value <V>, --value - (stdin), or --value-file <PATH>"
            )
        }
        (Some(_), Some(_)) => {
            // Clap should prevent this via conflicts_with, but
            // double-check at runtime in case the API drifts.
            anyhow::bail!("--value and --value-file are mutually exclusive")
        }
    }
}

// ============================================================================
// Audit log — minimal per-action JSONL stream
// ============================================================================

const AUDIT_FILENAME: &str = "secrets.jsonl";

/// Resolve `~/.mvm/audit/secrets.jsonl`. Falls back to a no-op log
/// when `$HOME` is unset (CI sandboxes, daemons without a home dir)
/// rather than failing the whole command — secret CRUD should keep
/// working even when the audit destination is unreachable.
///
/// Phase 4 (plan 60) dual-emit: when a [`Recorder`] is wired (via
/// [`AuditLog::with_optional_recorder`]), every successful action
/// also emits a chain-signed `secret.<verb>` entry through the
/// plan-64 audit stream. The original JSONL stream stays — operators
/// reading `~/.mvm/audit/secrets.jsonl` see the same shape they did
/// before plan 60; the Recorder is purely additive.
pub(crate) struct AuditLog {
    path: Option<PathBuf>,
    recorder: Option<Recorder>,
}

impl AuditLog {
    pub(crate) fn default() -> Result<Self> {
        let home = match std::env::var_os("HOME") {
            Some(h) => h,
            None => {
                return Ok(Self {
                    path: None,
                    recorder: None,
                });
            }
        };
        let dir = PathBuf::from(home).join(".mvm").join("audit");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating audit dir {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
        }
        Ok(Self {
            path: Some(dir.join(AUDIT_FILENAME)),
            recorder: None,
        })
    }

    /// Best-effort: attach a Recorder backed by the host signer's
    /// chain-signed audit stream. Failures (no `$HOME`, host signer
    /// not initialized, loose perms) leave the recorder unset and
    /// log a `tracing::warn` — secret CRUD continues to work; the
    /// extra chain-signed entry just doesn't land.
    pub(crate) fn with_optional_recorder(mut self) -> Self {
        match build_secret_recorder() {
            Ok(rec) => self.recorder = Some(rec),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "plan 60 Phase 4 secret recorder not wired; \
                     falling back to JSONL-only audit"
                );
            }
        }
        self
    }

    /// Test seam — write to an injected path.
    #[cfg(test)]
    pub(crate) fn with_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            recorder: None,
        }
    }

    /// Test seam — inject both a path and a pre-built Recorder for
    /// dual-emit testing.
    #[cfg(test)]
    pub(crate) fn with_path_and_recorder(path: PathBuf, recorder: Recorder) -> Self {
        Self {
            path: Some(path),
            recorder: Some(recorder),
        }
    }

    pub(crate) fn record<T, E>(
        &self,
        action: &str,
        tenant: &str,
        name: &str,
        outcome: &std::result::Result<T, E>,
    ) -> Result<()>
    where
        E: std::fmt::Display,
    {
        let (outcome_str, error) = match outcome {
            Ok(_) => ("ok", None),
            Err(e) => ("err", Some(e.to_string())),
        };
        if let Some(ref path) = self.path {
            let entry = serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "action": action,
                "tenant": tenant,
                "name": name,
                "outcome": outcome_str,
                "pid": std::process::id(),
                "error": error,
            });
            let mut line = serde_json::to_vec(&entry).context("serialize audit entry")?;
            line.push(b'\n');
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .with_context(|| format!("opening audit log {}", path.display()))?;
            f.write_all(&line)
                .with_context(|| format!("writing audit entry to {}", path.display()))?;
        }
        // Phase 4 dual-emit through Recorder. Audit-side failures are
        // surfaced as warnings, not propagated — operator-secret CRUD
        // must not fail because the chain-signed stream is unreachable
        // (matches `audit_chain::AuditEmitter`'s posture).
        if let Some(ref rec) = self.recorder {
            emit_through_recorder(rec, action, tenant, name, outcome_str, error.as_deref());
        }
        Ok(())
    }
}

/// Build a Recorder backed by `FileAuditSigner` rooted at
/// `~/.mvm/audit/`. The host signing key is read via
/// `host_signer::load_or_init`; the default tenant the recorder uses
/// for the unbound entry's required `tenant` field is `"local"` (matches
/// the secret command's default tenant). The per-action tenant is
/// captured in the entry's labels.
fn build_secret_recorder() -> Result<Recorder> {
    let signer = host_signer::load_or_init().context("loading host signer for secret recorder")?;
    let audit_dir = default_audit_dir()?;
    let file_signer = FileAuditSigner::open(signer.signing, &audit_dir)
        .with_context(|| format!("opening FileAuditSigner at {}", audit_dir.display()))?;
    Ok(Recorder::new(
        Arc::new(file_signer),
        TenantId("local".to_string()),
    ))
}

fn emit_through_recorder(
    recorder: &Recorder,
    action: &str,
    tenant: &str,
    name: &str,
    outcome: &str,
    error: Option<&str>,
) {
    let event = format!("secret.{action}");
    let mut extras: Vec<(String, String)> = vec![
        ("tenant".to_string(), tenant.to_string()),
        ("name".to_string(), name.to_string()),
        ("outcome".to_string(), outcome.to_string()),
        ("pid".to_string(), std::process::id().to_string()),
    ];
    if let Some(err) = error {
        extras.push(("error".to_string(), err.to_string()));
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "building tokio runtime for secret audit emit");
            return;
        }
    };
    if let Err(e) = rt.block_on(recorder.record_unbound(EventCategory::Secret, event, extras)) {
        tracing::warn!(
            error = %e,
            action = action,
            "Recorder dual-emit failed for secret event"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mvm_security::secret_store::FileSecretStore;

    fn temp_audit() -> (tempfile::TempDir, AuditLog) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("secrets.jsonl");
        (tmp, AuditLog::with_path(path))
    }

    fn read_audit(audit: &AuditLog) -> String {
        let path = audit.path.as_ref().expect("audit has path");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    // ──────────────────────────────────────────────────────────────
    // Audit invariants
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn audit_log_records_put_action_with_tenant_and_name() {
        let (_dir, audit) = temp_audit();
        let res: Result<()> = Ok(());
        audit.record("put", "acme", "api_token", &res).unwrap();
        let log = read_audit(&audit);
        assert!(log.contains("\"action\":\"put\""), "got: {log}");
        assert!(log.contains("\"tenant\":\"acme\""));
        assert!(log.contains("\"name\":\"api_token\""));
        assert!(log.contains("\"outcome\":\"ok\""));
    }

    #[test]
    fn audit_log_never_carries_value_field() {
        // The audit entry shape must not include any field named
        // `value` or similar — even on failure. If a future
        // refactor accidentally adds the value to the entry, this
        // test catches it.
        let (_dir, audit) = temp_audit();
        let res: Result<()> = Err(anyhow::anyhow!("boom"));
        audit.record("put", "acme", "tok", &res).unwrap();
        let log = read_audit(&audit);
        assert!(!log.contains("\"value\""));
        assert!(!log.contains("\"plaintext\""));
        assert!(log.contains("\"outcome\":\"err\""));
        assert!(log.contains("\"error\":\"boom\""));
    }

    #[test]
    fn audit_log_includes_pid() {
        let (_dir, audit) = temp_audit();
        let res: Result<()> = Ok(());
        audit.record("get", "acme", "tok", &res).unwrap();
        let log = read_audit(&audit);
        // pid is process id at time of record; just assert the
        // field exists with a numeric value.
        assert!(log.contains("\"pid\":"));
    }

    // ──────────────────────────────────────────────────────────────
    // Value resolution
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn resolve_value_inline_returns_value() {
        let v = resolve_value(Some("hello".into()), None).unwrap();
        assert_eq!(v, "hello");
    }

    #[test]
    fn resolve_value_file_reads_file_contents() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"from-file").unwrap();
        let v = resolve_value(None, Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(v, "from-file");
    }

    #[test]
    fn resolve_value_missing_returns_clear_error() {
        let err = resolve_value(None, None).unwrap_err();
        assert!(err.to_string().contains("no value source"), "got: {err}");
    }

    // ──────────────────────────────────────────────────────────────
    // Subcommand handlers — happy paths
    //
    // We can't easily test `cmd_get` against the TTY guard from a
    // unit test because stdout's terminal status is process-wide;
    // the guard's correctness is exercised through manual QA and
    // the predicates::cli integration tests in tests/cli.rs once
    // those land.
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn cmd_put_then_ls_shows_name() {
        let tmp_store = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp_store.path());
        let (_audit_dir, audit) = temp_audit();
        cmd_put(
            &store,
            &audit,
            "acme".into(),
            "api_token".into(),
            Some("secret-xyz".into()),
            None,
        )
        .unwrap();
        let names = store.list("acme").unwrap();
        assert_eq!(names, vec!["api_token"]);
    }

    #[test]
    fn cmd_rm_after_put_clears_name() {
        let tmp_store = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp_store.path());
        let (_audit_dir, audit) = temp_audit();
        cmd_put(
            &store,
            &audit,
            "acme".into(),
            "k".into(),
            Some("v".into()),
            None,
        )
        .unwrap();
        cmd_rm(&store, &audit, "acme".into(), "k".into()).unwrap();
        assert!(store.list("acme").unwrap().is_empty());
    }

    #[test]
    fn cmd_put_with_unsafe_tenant_id_records_audit_failure() {
        // Plan 63 W4 exit test: `mvmctl secret put --tenant ../etc`
        // must be rejected by validate_shell_id before the secret
        // hits disk, AND the audit log must capture the rejection.
        let tmp_store = tempfile::tempdir().unwrap();
        let store = FileSecretStore::with_dir(tmp_store.path());
        let (_audit_dir, audit) = temp_audit();
        let err = cmd_put(
            &store,
            &audit,
            "../etc".into(),
            "k".into(),
            Some("v".into()),
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Invalid tenant_id")
                || err.to_string().contains("alphanumeric")
        );
        let log = read_audit(&audit);
        assert!(log.contains("\"outcome\":\"err\""));
        assert!(log.contains("\"tenant\":\"../etc\""));
    }

    // ──────────────────────────────────────────────────────────────
    // Plan 60 Phase 4 — Recorder dual-emit
    //
    // When a Recorder is wired, AuditLog::record additionally emits
    // a chain-signed `secret.<verb>` entry through the unified
    // EventCategory::Secret stream. Existing JSONL contract is
    // unchanged.
    // ──────────────────────────────────────────────────────────────

    use mvm_supervisor::CapturingAuditSigner;

    fn temp_audit_with_recorder() -> (
        tempfile::TempDir,
        AuditLog,
        std::sync::Arc<CapturingAuditSigner>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let jsonl_path = tmp.path().join("secrets.jsonl");
        let signer = std::sync::Arc::new(CapturingAuditSigner::new());
        let recorder = Recorder::new(signer.clone(), TenantId("local".to_string()));
        (
            tmp,
            AuditLog::with_path_and_recorder(jsonl_path, recorder),
            signer,
        )
    }

    #[test]
    fn record_with_recorder_dual_emits_to_jsonl_and_chain() {
        // Both sinks fire on a single record() call: the original
        // JSONL stream keeps its shape, and the Recorder's
        // chain-signed envelope carries the secret.<verb> event.
        let (_dir, audit, signer) = temp_audit_with_recorder();
        let res: Result<()> = Ok(());
        audit.record("put", "acme", "api_token", &res).unwrap();

        // JSONL (plan-63 W4 stream) — preserved verbatim.
        let log = read_audit(&audit);
        assert!(log.contains("\"action\":\"put\""), "got: {log}");
        assert!(log.contains("\"tenant\":\"acme\""));

        // Recorder (plan-60 Phase 4 stream) — entry carries the
        // canonical `secret.put` event name and per-action tenant
        // in labels (the entry's `tenant` field is the recorder's
        // default).
        let entries = signer.entries();
        assert_eq!(entries.len(), 1, "expected one Recorder entry");
        assert_eq!(entries[0].event, "secret.put");
        assert_eq!(entries[0].labels.get("tenant"), Some(&"acme".to_string()));
        assert_eq!(
            entries[0].labels.get("name"),
            Some(&"api_token".to_string())
        );
        assert_eq!(entries[0].labels.get("outcome"), Some(&"ok".to_string()));
        // category label is injected by the Recorder substrate.
        assert_eq!(
            entries[0].labels.get("category"),
            Some(&"secret".to_string())
        );
        // No value leaks into the chain entry — same posture as
        // the JSONL stream.
        assert!(!entries[0].labels.values().any(|v| v.contains("plaintext")));
    }

    #[test]
    fn record_with_recorder_carries_error_label_on_failure() {
        let (_dir, audit, signer) = temp_audit_with_recorder();
        let res: Result<()> = Err(anyhow::anyhow!("boom"));
        audit.record("get", "acme", "tok", &res).unwrap();

        let entries = signer.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "secret.get");
        assert_eq!(entries[0].labels.get("outcome"), Some(&"err".to_string()));
        assert_eq!(entries[0].labels.get("error"), Some(&"boom".to_string()));
    }

    #[test]
    fn record_without_recorder_only_writes_jsonl() {
        // Default `temp_audit()` path leaves recorder=None. The
        // chain stream must NOT be touched.
        let (_dir, audit) = temp_audit();
        let res: Result<()> = Ok(());
        audit.record("put", "acme", "k", &res).unwrap();
        // Sanity: the JSONL still has the entry.
        assert!(read_audit(&audit).contains("\"action\":\"put\""));
        // No way to inspect a chain we never wired — the absence is
        // the contract. The signer field on AuditLog is None.
        assert!(audit.recorder.is_none());
    }

    #[test]
    fn record_with_recorder_emits_all_four_verbs() {
        let (_dir, audit, signer) = temp_audit_with_recorder();
        let ok: Result<()> = Ok(());
        audit.record("put", "acme", "k", &ok).unwrap();
        audit.record("get", "acme", "k", &ok).unwrap();
        audit.record("list", "acme", "*", &ok).unwrap();
        audit.record("delete", "acme", "k", &ok).unwrap();

        let events: Vec<String> = signer.entries().iter().map(|e| e.event.clone()).collect();
        assert_eq!(
            events,
            vec!["secret.put", "secret.get", "secret.list", "secret.delete"]
        );
    }
}
