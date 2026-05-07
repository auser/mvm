//! `mvmctl session kill <session-id>` — flip a session's status to
//! `Killed`. Inflight invokes against the session resolve as
//! failures with `kind = "session-killed"` (Plan 52 fd-3 channel
//! delivers the structured envelope; until then, the `Killed`
//! status is the host-side signal).

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session::{self, SessionStatus};
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    pub session_id: String,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let mut record = match session::read(&data_dir, &args.session_id)? {
        Some(r) => r,
        None => bail!("session not found: {}", args.session_id),
    };

    record.status = SessionStatus::Killed;
    session::write(&data_dir, &record).context("persist session record")?;

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionKill,
            None,
            Some(format!("session_id={}", args.session_id)),
        ));
    }

    Ok(())
}
