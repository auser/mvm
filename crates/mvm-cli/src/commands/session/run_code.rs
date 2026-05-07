//! `mvmctl session run-code <session-id> <code>` — run an ad-hoc
//! code snippet against a dev-mode session. Plan 52 phase 4.
//!
//! Like `exec`, **refused on prod-mode sessions** at the substrate
//! layer. Code is logged as a SHA-256 hash, not the code itself —
//! user code may carry arbitrary content (including secrets) and
//! the audit log shouldn't be a side-channel.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::session::{self, SessionMode};
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    pub session_id: String,
    /// Code snippet to run. Language is inferred from the session's
    /// wrapper config. Pass `-` to read from stdin.
    pub code: String,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let record = match session::read(&data_dir, &args.session_id)? {
        Some(r) => r,
        None => bail!("session not found: {}", args.session_id),
    };

    if record.mode == SessionMode::Prod {
        bail!(
            "session {} is mode=prod; run-code is dev-only — start a session with --mode dev",
            args.session_id
        );
    }

    let code = if args.code == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("read code from stdin")?;
        buf
    } else {
        args.code.clone()
    };

    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    let code_hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::SessionRunCode,
            None,
            Some(format!(
                "session_id={} code_sha256={} code_bytes={}",
                args.session_id,
                code_hash,
                code.len()
            )),
        ));
    }

    eprintln!(
        "session run-code: bookkeeping-only in v1 (session_id={}, code_sha256={code_hash}, code_bytes={})",
        args.session_id,
        code.len()
    );
    Ok(())
}
