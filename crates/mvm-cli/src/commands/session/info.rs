//! `mvmctl session info <session-id>` — emit the session record as
//! JSON on stdout. Read-only; exempt from the audit-emit gate per
//! the `info.rs` filename convention in
//! `scripts/check-cli-audit-emit.sh`.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::session;
use mvm_core::user_config::MvmConfig;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    pub session_id: String,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let record = match session::read(&data_dir, &args.session_id)? {
        Some(r) => r,
        None => bail!("session not found: {}", args.session_id),
    };

    let json = serde_json::to_string_pretty(&record).context("serialize session record")?;
    println!("{json}");
    Ok(())
}
