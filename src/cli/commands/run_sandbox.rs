use clap::Args;

#[derive(Args, Debug, Clone, Default)]
pub struct RunSandboxCommand {
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(_command: RunSandboxCommand) {
    println!("Running...");
}
