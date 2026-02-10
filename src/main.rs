mod infra;
mod vm;

// Re-export modules at crate root so internal `use crate::module` paths still work.
pub use infra::{bootstrap, config, shell, ui, upgrade};
pub use vm::{firecracker, image, lima, microvm, network};

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
    Start {
        /// Path to a built .elf image file (omit for default Ubuntu microVM)
        image: Option<String>,
        /// Runtime config file (TOML) with defaults for resources and volumes
        #[arg(long)]
        config: Option<String>,
        /// Volume override (format: host_path:guest_mount:size). Repeatable.
        #[arg(long, short = 'v')]
        volume: Vec<String>,
        /// CPU cores
        #[arg(long, short = 'c')]
        cpus: Option<u32>,
        /// Memory in MB
        #[arg(long, short = 'm')]
        memory: Option<u32>,
    },
    /// Stop the running microVM and clean up
    Stop,
    /// SSH into a running microVM
    Ssh,
    /// Show status of Lima VM and microVM
    Status,
    /// Tear down Lima VM and all resources
    Destroy,
    /// Check for and install the latest version of mvm
    Upgrade {
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,
        /// Force reinstall even if already up to date
        #[arg(long)]
        force: bool,
    },
    /// Build a microVM image from a Mvmfile.toml config
    Build {
        /// Image name (built-in like "openclaw") or path to directory with Mvmfile.toml
        #[arg(default_value = ".")]
        path: String,
        /// Output path for the built .elf image
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Bootstrap => cmd_bootstrap(),
        Commands::Setup => cmd_setup(),
        Commands::Dev => cmd_dev(),
        Commands::Start {
            image,
            config,
            volume,
            cpus,
            memory,
        } => match image {
            Some(ref elf) => cmd_start_image(elf, config.as_deref(), &volume, cpus, memory),
            None => cmd_start(),
        },
        Commands::Stop => cmd_stop(),
        Commands::Ssh => cmd_ssh(),
        Commands::Status => cmd_status(),
        Commands::Destroy => cmd_destroy(),
        Commands::Upgrade { check, force } => cmd_upgrade(check, force),
        Commands::Build { path, output } => cmd_build(&path, output.as_deref()),
    }
}

fn cmd_bootstrap() -> Result<()> {
    ui::info("Bootstrapping full environment...\n");

    bootstrap::check_homebrew()?;

    ui::info("\nInstalling prerequisites...");
    bootstrap::ensure_lima()?;

    run_setup_steps()?;

    ui::success("\nBootstrap complete! Run 'mvm start' or 'mvm dev' to launch a microVM.");
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

    ui::success("\nSetup complete! Run 'mvm start' to launch a microVM.");
    Ok(())
}

fn cmd_dev() -> Result<()> {
    ui::info("Launching development environment...\n");

    // If limactl is missing, run full bootstrap first
    if which::which("limactl").is_err() {
        ui::info("Lima not found. Running bootstrap...\n");
        cmd_bootstrap()?;
        return microvm::start();
    }

    // Check Lima VM status
    let lima_status = lima::get_status()?;
    match lima_status {
        lima::LimaStatus::NotFound => {
            ui::info("Lima VM not found. Running setup...\n");
            run_setup_steps()?;
            return microvm::start();
        }
        lima::LimaStatus::Stopped => {
            ui::info("Lima VM is stopped. Starting...");
            lima::start()?;
        }
        lima::LimaStatus::Running => {}
    }

    // Check if Firecracker is installed
    if !firecracker::is_installed()? {
        ui::info("Firecracker not installed. Running setup steps...\n");
        firecracker::install()?;
        firecracker::download_assets()?;
        firecracker::prepare_rootfs()?;
        firecracker::write_state()?;
        return microvm::start();
    }

    // If microVM is already running, just reconnect or recover
    if firecracker::is_running()? {
        if microvm::is_ssh_reachable()? {
            ui::info("MicroVM is already running. Connecting...\n");
            return microvm::ssh();
        }
        ui::warn("Firecracker running but microVM not reachable.");
        ui::info("Stopping and restarting...");
        microvm::stop()?;
    }

    microvm::start()
}

/// Core setup steps shared between cmd_setup() and cmd_bootstrap().
fn run_setup_steps() -> Result<()> {
    let lima_yaml = config::render_lima_yaml()?;
    ui::info(&format!(
        "Using rendered Lima config: {}",
        lima_yaml.path().display()
    ));

    ui::step(1, 4, "Setting up Lima VM...");
    lima::ensure_running(lima_yaml.path())?;

    ui::step(2, 4, "Installing Firecracker...");
    firecracker::install()?;

    ui::step(3, 4, "Downloading kernel and rootfs...");
    firecracker::download_assets()?;

    // Validate squashfs integrity before extraction (catches partial downloads)
    if !firecracker::validate_rootfs_squashfs()? {
        ui::warn("Downloaded rootfs is corrupted. Re-downloading...");
        shell::run_in_vm(&format!(
            "rm -f {dir}/ubuntu-*.squashfs.upstream",
            dir = config::MICROVM_DIR,
        ))?;
        firecracker::download_assets()?;
    }

    ui::step(4, 4, "Preparing root filesystem...");
    firecracker::prepare_rootfs()?;

    firecracker::write_state()?;
    Ok(())
}

fn cmd_start() -> Result<()> {
    microvm::start()
}

/// Start a built .elf image inside the Lima VM.
fn cmd_start_image(
    elf_path: &str,
    config_path: Option<&str>,
    volumes: &[String],
    cpus: Option<u32>,
    memory: Option<u32>,
) -> Result<()> {
    lima::require_running()?;

    // Load runtime config if provided
    let rt_config = match config_path {
        Some(p) => image::parse_runtime_config(p)?,
        None => image::RuntimeConfig::default(),
    };

    // Merge: CLI flags > config file > baked defaults (baked defaults handled by ELF itself)
    let mut elf_args = Vec::new();

    let final_cpus = cpus.or(rt_config.cpus);
    let final_memory = memory.or(rt_config.memory);
    if let Some(c) = final_cpus {
        elf_args.push("--cpus".to_string());
        elf_args.push(c.to_string());
    }
    if let Some(m) = final_memory {
        elf_args.push("--memory".to_string());
        elf_args.push(m.to_string());
    }

    // CLI volumes take priority; fall back to config file volumes
    if !volumes.is_empty() {
        for v in volumes {
            elf_args.push("--volume".to_string());
            elf_args.push(v.clone());
        }
    } else {
        for v in &rt_config.volumes {
            elf_args.push("--volume".to_string());
            elf_args.push(format!("{}:{}:{}", v.host, v.guest, v.size));
        }
    }

    // Build the command to run inside Lima
    let args_str = elf_args
        .iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");

    let cmd = if args_str.is_empty() {
        elf_path.to_string()
    } else {
        format!("{} {}", elf_path, args_str)
    };

    ui::info(&format!("Starting image: {}", elf_path));
    shell::replace_process("limactl", &["shell", config::VM_NAME, "bash", "-c", &cmd])
}

/// Simple shell escaping for passing args.
fn shell_escape(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') || s.contains('"') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

fn cmd_stop() -> Result<()> {
    microvm::stop()
}

fn cmd_ssh() -> Result<()> {
    microvm::ssh()
}

fn cmd_status() -> Result<()> {
    ui::status_header();

    let lima_status = lima::get_status()?;
    match lima_status {
        lima::LimaStatus::NotFound => {
            ui::status_line("Lima VM:", "Not created (run 'mvm setup')");
            ui::status_line("Firecracker:", "-");
            ui::status_line("MicroVM:", "-");
            return Ok(());
        }
        lima::LimaStatus::Stopped => {
            ui::status_line("Lima VM:", "Stopped");
            ui::status_line("Firecracker:", "-");
            ui::status_line("MicroVM:", "-");
            return Ok(());
        }
        lima::LimaStatus::Running => {
            ui::status_line("Lima VM:", "Running");
        }
    }

    if firecracker::is_running()? {
        let pid = shell::run_in_vm_stdout("cat ~/microvm/.fc-pid 2>/dev/null || echo '?'")
            .unwrap_or_else(|_| "?".to_string());
        ui::status_line("Firecracker:", &format!("Running (PID {})", pid));
    } else {
        ui::status_line("Firecracker:", "Not running");
        ui::status_line("MicroVM:", "Not running");
        return Ok(());
    }

    if microvm::is_ssh_reachable()? {
        ui::status_line(
            "MicroVM:",
            &format!("Running (SSH: root@{})", config::GUEST_IP),
        );
    } else {
        ui::status_line("MicroVM:", "Starting or unreachable");
    }

    Ok(())
}

fn cmd_upgrade(check: bool, force: bool) -> Result<()> {
    upgrade::upgrade(check, force)
}

fn cmd_build(path: &str, output: Option<&str>) -> Result<()> {
    let elf_path = image::build(path, output)?;
    ui::success(&format!("\nImage ready: {}", elf_path));
    ui::info(&format!("Run with: mvm start {}", elf_path));
    Ok(())
}

fn cmd_destroy() -> Result<()> {
    let status = lima::get_status()?;

    if matches!(status, lima::LimaStatus::NotFound) {
        ui::info("Nothing to destroy. Lima VM does not exist.");
        return Ok(());
    }

    // Stop microVM if running
    if matches!(status, lima::LimaStatus::Running) && firecracker::is_running()? {
        microvm::stop()?;
    }

    // Confirm
    if !ui::confirm("This will delete the Lima VM and all microVM data. Continue?") {
        ui::info("Cancelled.");
        return Ok(());
    }

    ui::info("Destroying Lima VM...");
    lima::destroy()?;
    ui::success("Destroyed.");
    Ok(())
}
