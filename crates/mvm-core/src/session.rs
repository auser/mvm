//! On-disk session registry. Plan 51.
//!
//! `mvmctl session {start, stop, set-timeout, kill, info}` operate on
//! per-session JSON records stored at
//! `~/.mvm/sessions/<session_id>.json`. The registry is the
//! authoritative source for every CLI verb — no in-process state, no
//! daemon. Each invocation reads + writes the file atomically.
//!
//! # Why flat-file
//!
//! mvmctl is a one-shot CLI. A long-running daemon owning an
//! in-memory `SessionMap` (like `mvm-mcp/src/session.rs`) doesn't fit
//! the lifecycle. Persisting per-session state to disk lets the next
//! `mvmctl` invocation pick up where the previous one left off
//! without coordination.
//!
//! # Scope (v1)
//!
//! This module ships the **bookkeeping layer** — record shapes, JSON
//! I/O, idle-timeout enforcement helpers. It does not boot or
//! tear down VMs; that integration lands in a follow-up. mvmforge's
//! `Session` class today calls these verbs for bookkeeping +
//! correlation; per-session VM materialisation is the substrate's
//! ADR-007 RunEntrypoint path against `mvmctl invoke`, separate
//! from session lifecycle.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Default per-session idle timeout, in seconds. Matches the MCP
/// session bookkeeping defaults (`mvm-mcp::DEFAULT_IDLE_SECS`) so
/// behavior is consistent across the two session surfaces.
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Lifecycle state. Verbs transition this and persist back to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Just created. No `mvmctl invoke` has run against this session.
    Created,
    /// One or more invokes have run.
    Running,
    /// Idle but resumable. Set when the supervisor's idle reaper
    /// flips the record after `idle_timeout_secs` of inactivity.
    Idle,
    /// Killed. `mvmctl session kill` sets this. In-flight invokes
    /// resolve as failures with `kind = "session-killed"`.
    Killed,
}

/// Wrapper-config mode (per ADR-0009). Fixed at session-start; the
/// dev-only `session exec` / `session run-code` verbs (Plan 52)
/// refuse to operate on a `Prod` session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Prod,
    Dev,
}

/// One row of the session registry. Persisted at
/// `~/.mvm/sessions/<session_id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub session_id: String,
    pub workload_id: String,
    pub status: SessionStatus,
    pub mode: SessionMode,
    /// RFC 3339 wall-clock.
    pub created_at: String,
    /// RFC 3339 wall-clock; updated on every `mvmctl invoke`. None
    /// until the first invoke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_invoke_at: Option<String>,
    /// Total successful invokes against this session.
    #[serde(default)]
    pub invoke_count: u64,
    pub idle_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl SessionRecord {
    /// Construct a fresh record. Caller persists via [`write`].
    pub fn new(
        session_id: impl Into<String>,
        workload_id: impl Into<String>,
        mode: SessionMode,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            workload_id: workload_id.into(),
            status: SessionStatus::Created,
            mode,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_invoke_at: None,
            invoke_count: 0,
            idle_timeout_secs: DEFAULT_IDLE_SECS,
            tags: BTreeMap::new(),
        }
    }
}

/// Compute the registry path for a session id under
/// `~/.mvm/sessions/<id>.json`.
pub fn session_path(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("sessions").join(format!("{session_id}.json"))
}

/// Compute the parent directory holding all session records.
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// Write a record atomically (write to temp + rename). Creates the
/// `sessions/` parent dir if missing. Mode 0700 on the dir, 0600 on
/// the file.
pub fn write(data_dir: &Path, record: &SessionRecord) -> Result<()> {
    let dir = sessions_dir(data_dir);
    fs::create_dir_all(&dir)
        .with_context(|| format!("create session registry dir: {}", dir.display()))?;
    set_dir_mode_0700(&dir)?;

    let path = session_path(data_dir, &record.session_id);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(record).context("serialize session record")?;
    fs::write(&tmp, &bytes).with_context(|| format!("write tmp: {}", tmp.display()))?;
    set_file_mode_0600(&tmp)?;
    fs::rename(&tmp, &path).with_context(|| format!("rename tmp -> {}", path.display()))?;
    Ok(())
}

/// Read a record by id. Returns `None` if the file doesn't exist;
/// errors on malformed JSON or other I/O failures.
pub fn read(data_dir: &Path, session_id: &str) -> Result<Option<SessionRecord>> {
    let path = session_path(data_dir, session_id);
    match fs::read(&path) {
        Ok(bytes) => {
            let record: SessionRecord = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse session record: {}", path.display()))?;
            if record.session_id != session_id {
                bail!(
                    "session record id mismatch: file {} contains {:?} but expected {:?}",
                    path.display(),
                    record.session_id,
                    session_id
                );
            }
            Ok(Some(record))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read session record: {}", path.display())),
    }
}

/// Remove a session record. Returns `true` if it existed and was
/// removed; `false` if it didn't exist (idempotent).
pub fn remove(data_dir: &Path, session_id: &str) -> Result<bool> {
    let path = session_path(data_dir, session_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove session record: {}", path.display())),
    }
}

#[cfg(unix)]
fn set_dir_mode_0700(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_mode_0700(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode_0600(_: &Path) -> Result<()> {
    Ok(())
}

/// Clamp a user-supplied idle-timeout (seconds) to `[1, 86400]`. The
/// upper bound matches existing TTL bounds in
/// `mvm-security/src/policy/ttl.rs`.
pub fn clamp_idle_timeout(secs: u64) -> u64 {
    secs.clamp(1, 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_read_round_trips() {
        let tmp = TempDir::new().unwrap();
        let mut record = SessionRecord::new("ses-abc", "adder", SessionMode::Prod);
        record.idle_timeout_secs = 600;
        record.tags.insert("env".to_string(), "prod".to_string());

        write(tmp.path(), &record).unwrap();

        let read_back = read(tmp.path(), "ses-abc").unwrap().expect("exists");
        assert_eq!(read_back, record);
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let result = read(tmp.path(), "ses-nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn remove_existing_returns_true() {
        let tmp = TempDir::new().unwrap();
        let record = SessionRecord::new("ses-1", "wl-1", SessionMode::Dev);
        write(tmp.path(), &record).unwrap();

        assert!(remove(tmp.path(), "ses-1").unwrap());
        assert!(read(tmp.path(), "ses-1").unwrap().is_none());
    }

    #[test]
    fn remove_missing_returns_false_idempotent() {
        let tmp = TempDir::new().unwrap();
        assert!(!remove(tmp.path(), "ses-nonexistent").unwrap());
    }

    #[test]
    fn id_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        let record = SessionRecord::new("ses-1", "wl", SessionMode::Prod);
        write(tmp.path(), &record).unwrap();
        // Rename file to ses-2.json so the id-in-file mismatches the
        // path-derived id we'll request.
        fs::rename(
            session_path(tmp.path(), "ses-1"),
            session_path(tmp.path(), "ses-2"),
        )
        .unwrap();
        let err = read(tmp.path(), "ses-2").unwrap_err();
        assert!(err.to_string().contains("session record id mismatch"));
    }

    #[test]
    fn clamp_idle_timeout_enforces_bounds() {
        assert_eq!(clamp_idle_timeout(0), 1);
        assert_eq!(clamp_idle_timeout(1), 1);
        assert_eq!(clamp_idle_timeout(300), 300);
        assert_eq!(clamp_idle_timeout(86_400), 86_400);
        assert_eq!(clamp_idle_timeout(86_401), 86_400);
        assert_eq!(clamp_idle_timeout(u64::MAX), 86_400);
    }

    #[test]
    fn malformed_json_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ses-bad.json"), b"{not json}").unwrap();

        let err = read(tmp.path(), "ses-bad").unwrap_err();
        assert!(err.to_string().contains("parse session record"));
    }

    #[cfg(unix)]
    #[test]
    fn write_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let record = SessionRecord::new("ses-1", "wl", SessionMode::Prod);
        write(tmp.path(), &record).unwrap();

        let mode = fs::metadata(session_path(tmp.path(), "ses-1"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let dir_mode = fs::metadata(sessions_dir(tmp.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }
}
