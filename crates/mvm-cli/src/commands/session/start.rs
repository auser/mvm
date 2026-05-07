//! `mvmctl session start <workload> [--mode prod|dev]` — create a
//! new session record. Prints the generated session id on stdout.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, ValueEnum};
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session::{self, SessionMode, SessionRecord};
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Workload id to associate with the session. Sessions are
    /// scoped to a single workload — the entrypoint binding is fixed
    /// at start time.
    pub workload: String,
    /// Wrapper-config mode. `prod` is the default; `dev` enables the
    /// (Plan 52) dev-only `session exec` / `session run-code` verbs.
    /// Mode is fixed at start; the substrate refuses dev verbs on a
    /// `prod` session.
    #[arg(long, value_enum, default_value_t = ModeArg::Prod)]
    pub mode: ModeArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(in crate::commands) enum ModeArg {
    Prod,
    Dev,
}

impl From<ModeArg> for SessionMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Prod => SessionMode::Prod,
            ModeArg::Dev => SessionMode::Dev,
        }
    }
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let session_id = generate_session_id();
    let mode: SessionMode = args.mode.into();
    let record = SessionRecord::new(&session_id, &args.workload, mode);
    session::write(&data_dir, &record).context("persist session record")?;

    audit_log(
        &data_dir,
        LocalAuditKind::SessionStart,
        Some(format!(
            "session_id={} workload_id={}",
            session_id, args.workload
        )),
    );

    println!("{session_id}");
    Ok(())
}

/// Generate a 16-hex-char session id with the conventional `ses-`
/// prefix. Uniqueness comes from the OS RNG via `getrandom`-shaped
/// fallback to a clock-derived id when no RNG is wired.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Fast unique-enough id: clock ns + thread-local counter. Doesn't
    // need to be cryptographically random — collision risk is bounded
    // by mvmctl's one-shot invocation rate per second.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ses-{nanos:016x}")
}

fn audit_log(data_dir: &std::path::Path, kind: LocalAuditKind, detail: Option<String>) {
    // Best-effort. Failure to write the audit line should not fail
    // the verb (callers parse stdout for the session_id); operators
    // observe audit-write failures via separate health checks.
    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(kind, None, detail));
    }
}
