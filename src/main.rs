mod bootstrap;
mod config;
mod firecracker;
mod lima;
mod microvm;
mod network;
mod shell;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mvm",
    version,
    about = "Manage Firecracker microVMs on Apple Silicon via Lima"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Full environment setup from scratch (installs Lima via Homebrew, then runs setup)
    Bootstrap,
    /// Create Lima VM, install Firecracker, download kernel/rootfs (requires limactl)
    Setup,
    /// Launch into microVM, auto-bootstrapping if needed
    Dev,
    /// Start the microVM and drop into interactive SSH
    Start,
    /// Stop the running microVM and clean up
    Stop,
    /// SSH into a running microVM
    Ssh,
    /// Show status of Lima VM and microVM
    Status,
    /// Tear down Lima VM and all resources
    Destroy,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Bootstrap => cmd_bootstrap(),
        Commands::Setup => cmd_setup(),
        Commands::Dev => cmd_dev(),
        Commands::Start => cmd_start(),
        Commands::Stop => cmd_stop(),
        Commands::Ssh => cmd_ssh(),
        Commands::Status => cmd_status(),
        Commands::Destroy => cmd_destroy(),
    }
}

fn cmd_bootstrap() -> Result<()> {
    println!("[mvm] Bootstrapping full environment...\n");

    bootstrap::check_homebrew()?;

    println!("\n[mvm] Installing prerequisites...");
    bootstrap::ensure_lima()?;

    run_setup_steps()?;

    println!("\n[mvm] Bootstrap complete! Run 'mvm start' or 'mvm dev' to launch a microVM.");
    Ok(())
}

fn cmd_setup() -> Result<()> {
    which::which("limactl").map_err(|_| {
        anyhow::anyhow!(
            "'limactl' not found. Install Lima first: brew install lima\n\
             Or run 'mvm bootstrap' for full automatic setup."
        )
    })?;

    run_setup_steps()?;

    println!("\n[mvm] Setup complete! Run 'mvm start' to launch a microVM.");
    Ok(())
}

fn cmd_dev() -> Result<()> {
    println!("[mvm] Launching development environment...\n");

    // If limactl is missing, run full bootstrap first
    if which::which("limactl").is_err() {
        println!("[mvm] Lima not found. Running bootstrap...\n");
        cmd_bootstrap()?;
        return microvm::start();
    }

    // Check Lima VM status
    let lima_status = lima::get_status()?;
    match lima_status {
        lima::LimaStatus::NotFound => {
            println!("[mvm] Lima VM not found. Running setup...\n");
            run_setup_steps()?;
            return microvm::start();
        }
        lima::LimaStatus::Stopped => {
            println!("[mvm] Lima VM is stopped. Starting...");
            lima::start()?;
        }
        lima::LimaStatus::Running => {}
    }

    // Check if Firecracker is installed
    if !firecracker::is_installed()? {
        println!("[mvm] Firecracker not installed. Running setup steps...\n");
        firecracker::install()?;
        firecracker::download_assets()?;
        firecracker::prepare_rootfs()?;
        firecracker::write_state()?;
        return microvm::start();
    }

    // If microVM is already running, just reconnect or recover
    if firecracker::is_running()? {
        if microvm::is_ssh_reachable()? {
            println!("[mvm] MicroVM is already running. Connecting...\n");
            return microvm::ssh();
        }
        println!("[mvm] Firecracker running but microVM not reachable.");
        println!("[mvm] Stopping and restarting...");
        microvm::stop()?;
    }

    microvm::start()
}

/// Core setup steps shared between cmd_setup() and cmd_bootstrap().
fn run_setup_steps() -> Result<()> {
    let lima_yaml = config::render_lima_yaml()?;
    println!(
        "[mvm] Using rendered Lima config: {}",
        lima_yaml.path().display()
    );

    println!("\n[mvm] Step 1/4: Setting up Lima VM...");
    lima::ensure_running(lima_yaml.path())?;

    println!("\n[mvm] Step 2/4: Installing Firecracker...");
    firecracker::install()?;

    println!("\n[mvm] Step 3/4: Downloading kernel and rootfs...");
    firecracker::download_assets()?;

    println!("\n[mvm] Step 4/4: Preparing root filesystem...");
    firecracker::prepare_rootfs()?;

    firecracker::write_state()?;
    Ok(())
}

fn cmd_start() -> Result<()> {
    microvm::start()
}

fn cmd_stop() -> Result<()> {
    microvm::stop()
}

fn cmd_ssh() -> Result<()> {
    microvm::ssh()
}

fn cmd_status() -> Result<()> {
    println!("mvm status");
    println!("----------");

    let lima_status = lima::get_status()?;
    match lima_status {
        lima::LimaStatus::NotFound => {
            println!("Lima VM:      Not created (run 'mvm setup')");
            println!("Firecracker:  -");
            println!("MicroVM:      -");
            return Ok(());
        }
        lima::LimaStatus::Stopped => {
            println!("Lima VM:      Stopped");
            println!("Firecracker:  -");
            println!("MicroVM:      -");
            return Ok(());
        }
        lima::LimaStatus::Running => {
            println!("Lima VM:      Running");
        }
    }

    if firecracker::is_running()? {
        let pid = shell::run_in_vm_stdout("cat ~/microvm/.fc-pid 2>/dev/null || echo '?'")
            .unwrap_or_else(|_| "?".to_string());
        println!("Firecracker:  Running (PID {})", pid);
    } else {
        println!("Firecracker:  Not running");
        println!("MicroVM:      Not running");
        return Ok(());
    }

    if microvm::is_ssh_reachable()? {
        println!("MicroVM:      Running (SSH: root@{})", config::GUEST_IP);
    } else {
        println!("MicroVM:      Starting or unreachable");
    }

    Ok(())
}

fn cmd_destroy() -> Result<()> {
    let status = lima::get_status()?;

    if matches!(status, lima::LimaStatus::NotFound) {
        println!("[mvm] Nothing to destroy. Lima VM does not exist.");
        return Ok(());
    }

    // Stop microVM if running
    if matches!(status, lima::LimaStatus::Running) && firecracker::is_running()? {
        microvm::stop()?;
    }

    // Confirm
    eprint!("[mvm] This will delete the Lima VM and all microVM data. Continue? [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("[mvm] Cancelled.");
        return Ok(());
    }

    println!("[mvm] Destroying Lima VM...");
    lima::destroy()?;
    println!("[mvm] Destroyed.");
    Ok(())
}
