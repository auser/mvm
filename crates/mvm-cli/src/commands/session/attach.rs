//! `mvmctl session attach <session-id>` — re-attach a fresh client
//! to an existing session and dispatch invokes against it. Plan 52
//! phase 3.
//!
//! Trust model: session ids are trusted within the local-machine
//! substrate boundary — anyone with filesystem access to the session
//! registry already has equivalent privileges. Cross-host attach
//! requires authentication, which is mvmd's concern, not mvm's.
//!
//! v1 scope is bookkeeping: the verb verifies the session exists,
//! increments `invoke_count`, updates `last_invoke_at`, and emits a
//! `SessionAttach` audit record. The actual `dispatch_in_session`
//! runtime primitive (boot/dispatch on a warm VM) lands in a
//! follow-up alongside the per-session VM lifecycle integration.

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

    if record.status == SessionStatus::Killed {
        bail!("session {} is killed; cannot attach", args.session_id);
    }

    // Bookkeeping update — bump invoke_count and last_invoke_at to
    // reflect the attach. mvmforge's Session.attach() relies on this
    // shape to update its in-process counters consistently with the
    // substrate.
    record.invoke_count = record.invoke_count.saturating_add(1);
    record.last_invoke_at = Some(chrono::Utc::now().to_rfc3339());
    if record.status == SessionStatus::Created {
        record.status = SessionStatus::Running;
    }
    session::write(&data_dir, &record).context("persist session record")?;

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionAttach,
            None,
            Some(format!("session_id={}", args.session_id)),
        ));
    }

    Ok(())
}
