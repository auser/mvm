//! `mvmctl session exec <session-id> [--] <command> [args...]` —
//! run an ad-hoc command against a dev-mode session. Plan 52
//! phase 4.
//!
//! **Refused on prod-mode sessions** at the substrate layer — never
//! gated on client-side checks alone. Production sessions never
//! grant exec capability; the only way to get exec is to start the
//! session with `--mode dev`, which is a deliberate choice operators
//! make once per session.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session::{self, SessionMode};
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    pub session_id: String,
    /// Command + args to run against the session. Use `--` before
    /// the command if any of its args look like flags.
    #[arg(trailing_var_arg = true, num_args = 1..)]
    pub command: Vec<String>,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let record = match session::read(&data_dir, &args.session_id)? {
        Some(r) => r,
        None => bail!("session not found: {}", args.session_id),
    };

    if record.mode == SessionMode::Prod {
        // Hard refusal at the substrate layer.
        bail!(
            "session {} is mode=prod; exec is dev-only — start a session with --mode dev",
            args.session_id
        );
    }

    // v1: bookkeeping only — record the intent. The actual
    // dispatch_in_session integration lands when the runtime's
    // session-VM lifecycle primitives ship.
    let argv_summary = if args.command.len() <= 8 {
        args.command.join(" ")
    } else {
        format!(
            "{} ...({} args)",
            args.command[..3].join(" "),
            args.command.len()
        )
    };

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionExec,
            None,
            Some(format!(
                "session_id={} argv={}",
                args.session_id, argv_summary
            )),
        ));
    }

    eprintln!(
        "session exec: bookkeeping-only in v1 (session_id={}, argv={argv_summary})",
        args.session_id
    );
    Ok(())
}
