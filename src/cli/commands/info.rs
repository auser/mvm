use clap::Args;

#[derive(Args, Debug, Clone, Default)]
pub struct InfoCommand {}

pub async fn run(_info: InfoCommand) -> anyhow::Result<()> {
    Ok(())
}
