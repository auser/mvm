use std::io::IsTerminal;

use crate::{
    cli::{commands::Commands, lib::logs::LogArgs},
    utils::{init_tracing, styles},
};
use clap::Parser;
mod commands;
mod lib;

use shadow_rs::shadow;

shadow!(build);

#[derive(Parser)]
#[command(name = build::PROJECT_NAME, version = build::VERSION, about = format!("{} {}", build::PROJECT_NAME, build::VERSION), styles = styles())]
pub struct Cli {
    /// Log level for the CLI.
    #[command(flatten)]
    pub logs: LogArgs,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let log_level = cli.logs.use_level();
    let ansi = std::io::stderr().is_terminal();

    init_tracing(log_level, ansi);

    match &cli.command {
        Some(Commands::RunSandbox(command)) => commands::run_sandbox::run(command).await?,
        Some(Commands::Info(command)) => commands::info(command).await?,

        _ => {}
    }
    Ok(())
}
