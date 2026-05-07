//! `mvmctl session` — session lifecycle verbs. Plan 51 + Plan 52.
//!
//! Operates on the on-disk session registry at
//! `~/.mvm/sessions/<id>.json` (see `mvm_core::session`). Each verb
//! reads + writes the registry atomically; mvmctl is one-shot, no
//! daemon coordination.
//!
//! Verbs are bookkeeping-only in v1 — they create / mutate / remove
//! the JSON record but do not boot or tear down VMs. The per-session
//! VM materialization integration ships in a follow-up once the
//! runtime's session-VM lifecycle primitives land. Until then,
//! mvmforge's `Session` class uses these verbs for correlation and
//! `mvmctl invoke <id>` continues to dispatch one-off invokes against
//! the workload.

use anyhow::Result;
use clap::{Args as ClapArgs, Subcommand};

use super::Cli;
use mvm_core::user_config::MvmConfig;

mod attach;
mod exec;
mod info;
mod kill;
mod run_code;
mod set_timeout;
mod start;
mod stop;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: SessionAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum SessionAction {
    /// Create a new session for a workload.
    Start(start::Args),
    /// Remove an existing session record.
    Stop(stop::Args),
    /// Update a session's idle timeout (clamped to [1, 86400]).
    #[command(name = "set-timeout")]
    SetTimeout(set_timeout::Args),
    /// Mark a session as killed.
    Kill(kill::Args),
    /// Print a session's record as JSON on stdout.
    Info(info::Args),
    /// Re-attach a fresh client to an existing session.
    Attach(attach::Args),
    /// Run an ad-hoc command against a dev-mode session (refused on prod).
    Exec(exec::Args),
    /// Run an ad-hoc code snippet against a dev-mode session (refused on prod).
    #[command(name = "run-code")]
    RunCode(run_code::Args),
}

pub(in crate::commands) fn run(cli: &Cli, args: Args, cfg: &MvmConfig) -> Result<()> {
    match args.action {
        SessionAction::Start(a) => start::run(cli, a, cfg),
        SessionAction::Stop(a) => stop::run(cli, a, cfg),
        SessionAction::SetTimeout(a) => set_timeout::run(cli, a, cfg),
        SessionAction::Kill(a) => kill::run(cli, a, cfg),
        SessionAction::Info(a) => info::run(cli, a, cfg),
        SessionAction::Attach(a) => attach::run(cli, a, cfg),
        SessionAction::Exec(a) => exec::run(cli, a, cfg),
        SessionAction::RunCode(a) => run_code::run(cli, a, cfg),
    }
}
