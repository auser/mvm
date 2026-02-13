use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "mvm-hostd", about = "mvm privileged executor daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the hostd daemon, listening on a Unix domain socket.
    Serve {
        /// Path to the Unix domain socket.
        #[arg(long, default_value = "/run/mvm/hostd.sock")]
        socket: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    match cli.command {
        Command::Serve { socket } => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(mvm_runtime::hostd::server::serve(Some(&socket)))
        }
    }
}
