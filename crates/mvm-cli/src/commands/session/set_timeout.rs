//! `mvmctl session set-timeout <secs> <session-id>` — update the
//! `idle_timeout_secs` of a session record. Clamps to `[1, 86400]`.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session::{self, clamp_idle_timeout};
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Idle timeout in seconds. Clamped to `[1, 86400]`.
    pub seconds: u64,
    pub session_id: String,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let mut record = match session::read(&data_dir, &args.session_id)? {
        Some(r) => r,
        None => bail!("session not found: {}", args.session_id),
    };

    let old = record.idle_timeout_secs;
    let new = clamp_idle_timeout(args.seconds);
    record.idle_timeout_secs = new;
    session::write(&data_dir, &record).context("persist session record")?;

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionSetTimeout,
            None,
            Some(format!(
                "session_id={} old_idle_timeout_secs={} new_idle_timeout_secs={}",
                args.session_id, old, new
            )),
        ));
    }

    Ok(())
}
