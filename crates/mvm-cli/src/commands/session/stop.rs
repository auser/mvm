//! `mvmctl session stop <session-id>` — remove a session record.
//! Idempotent: returns 0 even if the record doesn't exist.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session;
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    pub session_id: String,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let removed = session::remove(&data_dir, &args.session_id).context("remove session record")?;

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionStop,
            None,
            Some(format!(
                "session_id={} removed={}",
                args.session_id, removed
            )),
        ));
    }

    Ok(())
}
