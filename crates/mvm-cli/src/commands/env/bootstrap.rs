//! `mvmctl bootstrap` — full environment setup from scratch.

use anyhow::Result;
use clap::Args as ClapArgs;

use crate::bootstrap;
use crate::ui;

use mvm_core::user_config::MvmConfig;

use super::Cli;
use super::setup::run_setup_steps;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Production mode (skip Homebrew, assume Linux with apt)
    #[arg(long)]
    pub production: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    run_steps(args.production)
}

/// Run the bootstrap steps — exposed so other commands (dev) can re-bootstrap
/// without going through the dispatcher.
pub(super) fn run_steps(production: bool) -> Result<()> {
    ui::info("Bootstrapping full environment...\n");

    if !production {
        bootstrap::check_package_manager()?;
    }

    // Plan-60 / ADR-013: dev mode is Apple-Container/libkrun, not
    // Lima — there's no Lima VM to provision here. The host-side
    // prerequisite hint (libkrun on macOS Intel) is the only legacy
    // bootstrap surface left; setup_steps below handles Firecracker
    // assets via `run_in_vm`, which the W8 direct-launch rewrite will
    // collapse into a host-only operation.
    bootstrap::hint_libkrun_if_useful();

    // Default sizing for the builder VM; CLI-level overrides ride the
    // setup path.
    run_setup_steps(false, 8, 16)?;

    ui::success("\nBootstrap complete! Run 'mvmctl dev' to enter the development environment.");
    Ok(())
}
