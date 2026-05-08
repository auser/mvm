use clap::Subcommand;

pub(crate) mod info;
pub(crate) mod run_sandbox;

pub use info::InfoCommand;
pub use run_sandbox::RunSandboxCommand;

#[derive(Subcommand)]
pub enum Commands {
    Info(InfoCommand),
    RunSandbox(RunSandboxCommand),
}
