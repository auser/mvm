use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

// Import from the library crate
use mvm::infra::output::OutputFormat;
use mvm::infra::{bootstrap, config, output, shell, ui, upgrade};
use mvm::observability::logging::{self, LogFormat};
use mvm::vm::{bridge, naming, pool, tenant};
use mvm::vm::{firecracker, image, lima, microvm};

#[derive(Parser)]
#[command(
    name = "mvm",
    version,
    about = "Multi-tenant Firecracker microVM fleet manager"
)]
struct Cli {
    /// Output format: table, json, yaml
    #[arg(long, short = 'o', global = true, default_value = "table")]
    output: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // ---- Tenant management ----
    /// Manage tenants (security/quota/network boundaries)
    Tenant {
        #[command(subcommand)]
        action: TenantCmd,
    },

    // ---- Pool management ----
    /// Manage worker pools within tenants
    Pool {
        #[command(subcommand)]
        action: PoolCmd,
    },

    // ---- Instance operations ----
    /// Manage individual microVM instances
    Instance {
        #[command(subcommand)]
        action: InstanceCmd,
    },

    // ---- Agent ----
    /// Agent reconcile loop and daemon
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },

    // ---- Coordinator client ----
    /// Coordinator client for multi-node fleet management
    Coordinator {
        #[command(subcommand)]
        action: CoordinatorCmd,
    },

    // ---- Network ----
    /// Network verification and diagnostics
    Net {
        #[command(subcommand)]
        action: NetCmd,
    },

    // ---- Node ----
    /// Node information and statistics
    Node {
        #[command(subcommand)]
        action: NodeCmd,
    },

    // ---- Dev mode (UNCHANGED) ----
    /// Full environment setup from scratch
    Bootstrap {
        /// Production mode (skip Homebrew, assume Linux with apt)
        #[arg(long)]
        production: bool,
    },
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
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Tail audit events for a tenant
    Events {
        /// Tenant ID
        tenant: String,
        /// Number of recent events to show
        #[arg(long, short = 'n', default_value = "20")]
        last: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

// --- Tenant subcommands ---

#[derive(Subcommand)]
enum TenantCmd {
    /// Create a new tenant
    Create {
        /// Tenant ID (lowercase alphanumeric + hyphens)
        id: String,
        /// Coordinator-assigned network ID (0-4095, cluster-unique)
        #[arg(long)]
        net_id: u16,
        /// Coordinator-assigned IPv4 subnet (CIDR), e.g. "10.240.3.0/24"
        #[arg(long)]
        subnet: String,
        /// Maximum vCPUs across all instances
        #[arg(long, default_value = "16")]
        max_vcpus: u32,
        /// Maximum memory in MiB across all instances
        #[arg(long, default_value = "32768")]
        max_mem: u64,
        /// Maximum concurrently running instances
        #[arg(long, default_value = "8")]
        max_running: u32,
        /// Maximum warm instances
        #[arg(long, default_value = "4")]
        max_warm: u32,
    },
    /// List all tenants on this node
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show tenant details
    Info {
        /// Tenant ID
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Destroy a tenant and all its resources
    Destroy {
        /// Tenant ID
        id: String,
        /// Skip confirmation
        #[arg(long)]
        force: bool,
        /// Also wipe persistent volumes
        #[arg(long)]
        wipe_volumes: bool,
    },
    /// Set tenant secrets from a file
    Secrets {
        #[command(subcommand)]
        action: TenantSecretsCmd,
    },
}

#[derive(Subcommand)]
enum TenantSecretsCmd {
    /// Set secrets from a JSON file
    Set {
        /// Tenant ID
        id: String,
        /// Path to secrets JSON file
        #[arg(long)]
        from_file: String,
    },
    /// Rotate secrets (bump epoch)
    Rotate {
        /// Tenant ID
        id: String,
    },
}

// --- Pool subcommands ---

#[derive(Subcommand)]
enum PoolCmd {
    /// Create a new pool within a tenant
    Create {
        /// Pool path: <tenant>/<pool>
        path: String,
        /// Nix flake reference
        #[arg(long)]
        flake: String,
        /// Guest profile: minimal, baseline, python
        #[arg(long)]
        profile: String,
        /// vCPUs per instance
        #[arg(long)]
        cpus: u8,
        /// Memory per instance (MiB)
        #[arg(long)]
        mem: u32,
        /// Data disk per instance (MiB)
        #[arg(long, default_value = "0")]
        data_disk: u32,
    },
    /// List pools in a tenant
    List {
        /// Tenant ID
        tenant: String,
        #[arg(long)]
        json: bool,
    },
    /// Show pool details
    Info {
        /// Pool path: <tenant>/<pool>
        path: String,
        #[arg(long)]
        json: bool,
    },
    /// Build pool artifacts (ephemeral Firecracker builder VM)
    Build {
        /// Pool path: <tenant>/<pool>
        path: String,
        /// Build timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Scale pool desired counts
    Scale {
        /// Pool path: <tenant>/<pool>
        path: String,
        /// Desired running instances
        #[arg(long)]
        running: Option<u32>,
        /// Desired warm instances
        #[arg(long)]
        warm: Option<u32>,
        /// Desired sleeping instances
        #[arg(long)]
        sleeping: Option<u32>,
    },
    /// Destroy a pool and all its instances
    Destroy {
        /// Pool path: <tenant>/<pool>
        path: String,
        /// Skip confirmation
        #[arg(long)]
        force: bool,
    },
    /// Clean up old build revisions for a pool
    Gc {
        /// Pool path: <tenant>/<pool>
        path: String,
        /// Number of revisions to keep
        #[arg(long, default_value = "2")]
        keep: usize,
    },
}

// --- Instance subcommands ---

#[derive(Subcommand)]
enum InstanceCmd {
    /// Create a new instance in a pool
    Create {
        /// Pool path: <tenant>/<pool>
        path: String,
    },
    /// List instances
    List {
        /// Filter by tenant
        #[arg(long)]
        tenant: Option<String>,
        /// Filter by pool (requires --tenant)
        #[arg(long)]
        pool: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Start an instance
    Start {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
    /// Stop an instance
    Stop {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
    /// Pause vCPUs (Running → Warm)
    Warm {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
    /// Snapshot and shutdown (Warm → Sleeping)
    Sleep {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
        /// Skip guest prep ACK, force snapshot
        #[arg(long)]
        force: bool,
    },
    /// Restore from snapshot (Sleeping → Running)
    Wake {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
    /// SSH into a running instance
    #[command(name = "ssh")]
    Ssh {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
    /// Show instance stats
    Stats {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
        #[arg(long)]
        json: bool,
    },
    /// Destroy an instance
    Destroy {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
        /// Also wipe persistent volumes
        #[arg(long)]
        wipe_volumes: bool,
    },
    /// View instance logs
    Logs {
        /// Instance path: <tenant>/<pool>/<instance>
        path: String,
    },
}

// --- Agent subcommands ---

#[derive(Subcommand)]
enum AgentCmd {
    /// Run a single reconcile pass against a desired state file
    Reconcile {
        /// Path to desired state JSON
        #[arg(long)]
        desired: String,
        /// Destroy tenants/pools not in desired state
        #[arg(long)]
        prune: bool,
    },
    /// Start the agent daemon (reconcile loop + QUIC API)
    Serve {
        /// Reconcile interval in seconds
        #[arg(long, default_value = "30")]
        interval_secs: u64,
        /// Path to desired state file (alternative to QUIC push)
        #[arg(long)]
        desired: Option<String>,
        /// Listen address for QUIC API
        #[arg(long)]
        listen: Option<String>,
    },
    /// Generate desired state JSON from existing tenants and pools
    Desired {
        /// Write to file instead of stdout
        #[arg(long)]
        file: Option<String>,
        /// Node identifier
        #[arg(long, default_value = "local")]
        node_id: String,
    },
    /// Manage mTLS certificates for agent communication
    Certs {
        #[command(subcommand)]
        action: AgentCertsCmd,
    },
}

#[derive(Subcommand)]
enum AgentCertsCmd {
    /// Initialize with a CA certificate
    Init {
        /// Path to CA certificate PEM file (omit for self-signed dev CA)
        #[arg(long)]
        ca: Option<String>,
    },
    /// Rotate the node certificate
    Rotate,
    /// Show certificate status
    Status {
        #[arg(long)]
        json: bool,
    },
}

// --- Coordinator subcommands ---

#[derive(Subcommand)]
enum CoordinatorCmd {
    /// Push desired state to a remote node
    Push {
        /// Path to desired state JSON file
        #[arg(long)]
        desired: String,
        /// Remote node address (host:port)
        #[arg(long)]
        node: String,
    },
    /// Query node status
    Status {
        /// Remote node address (host:port)
        #[arg(long)]
        node: String,
    },
    /// List instances on a remote node
    ListInstances {
        /// Remote node address (host:port)
        #[arg(long)]
        node: String,
        /// Tenant ID to filter by
        #[arg(long)]
        tenant: String,
        /// Optional pool ID filter
        #[arg(long)]
        pool: Option<String>,
    },
    /// Wake a sleeping instance on a remote node
    Wake {
        /// Remote node address (host:port)
        #[arg(long)]
        node: String,
        /// Tenant ID
        #[arg(long)]
        tenant: String,
        /// Pool ID
        #[arg(long)]
        pool: String,
        /// Instance ID
        #[arg(long)]
        instance: String,
    },
}

// --- Network subcommands ---

#[derive(Subcommand)]
enum NetCmd {
    /// Verify network configuration for all tenants
    Verify {
        #[arg(long)]
        json: bool,
    },
}

// --- Node subcommands ---

#[derive(Subcommand)]
enum NodeCmd {
    /// Show node information
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Show aggregate node statistics
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Show disk usage report
    Disk {
        #[arg(long)]
        json: bool,
    },
    /// Run garbage collection across all pools
    Gc {
        /// Number of revisions to keep per pool
        #[arg(long, default_value = "2")]
        keep: usize,
    },
}

// ============================================================================
// Command dispatch
// ============================================================================

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging: JSON for daemon mode, human-readable for CLI
    let log_format = match &cli.command {
        Commands::Agent {
            action: AgentCmd::Serve { .. },
        } => LogFormat::Json,
        _ => LogFormat::Human,
    };
    logging::init(log_format);

    let out_fmt = OutputFormat::from_str_arg(&cli.output);

    match cli.command {
        // --- Dev mode (unchanged) ---
        Commands::Bootstrap { production } => cmd_bootstrap(production),
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
        Commands::Completions { shell } => cmd_completions(shell),
        Commands::Events { tenant, last, json } => cmd_events(&tenant, last, json),

        // --- Multi-tenant ---
        Commands::Tenant { action } => cmd_tenant(action, out_fmt),
        Commands::Pool { action } => cmd_pool(action, out_fmt),
        Commands::Instance { action } => cmd_instance(action, out_fmt),
        Commands::Agent { action } => cmd_agent(action),
        Commands::Coordinator { action } => cmd_coordinator(action, out_fmt),
        Commands::Net { action } => cmd_net(action, out_fmt),
        Commands::Node { action } => cmd_node(action, out_fmt),
    }
}

// ============================================================================
// Dev mode handlers (unchanged except bootstrap)
// ============================================================================

fn cmd_bootstrap(production: bool) -> Result<()> {
    ui::info("Bootstrapping full environment...\n");

    if !production {
        bootstrap::check_package_manager()?;
    }

    ui::info("\nInstalling prerequisites...");
    bootstrap::ensure_lima()?;

    run_setup_steps()?;

    ui::success("\nBootstrap complete! Run 'mvm start' or 'mvm dev' to launch a microVM.");
    Ok(())
}

fn cmd_setup() -> Result<()> {
    if !bootstrap::is_lima_required() {
        // Native Linux — just install FC directly
        run_setup_steps()?;
        ui::success("\nSetup complete! Run 'mvm start' to launch a microVM.");
        return Ok(());
    }

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

    if bootstrap::is_lima_required() {
        // macOS or Linux without KVM — need Lima
        if which::which("limactl").is_err() {
            ui::info("Lima not found. Running bootstrap...\n");
            cmd_bootstrap(false)?;
            return microvm::start();
        }

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
    }

    if !firecracker::is_installed()? {
        ui::info("Firecracker not installed. Running setup steps...\n");
        firecracker::install()?;
        firecracker::download_assets()?;
        firecracker::prepare_rootfs()?;
        firecracker::write_state()?;
        return microvm::start();
    }

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

fn run_setup_steps() -> Result<()> {
    if bootstrap::is_lima_required() {
        let lima_yaml = config::render_lima_yaml()?;
        ui::info(&format!(
            "Using rendered Lima config: {}",
            lima_yaml.path().display()
        ));

        ui::step(1, 4, "Setting up Lima VM...");
        lima::ensure_running(lima_yaml.path())?;
    } else {
        ui::step(1, 4, "Native Linux detected — skipping Lima VM setup.");
    }

    ui::step(2, 4, "Installing Firecracker...");
    firecracker::install()?;

    ui::step(3, 4, "Downloading kernel and rootfs...");
    firecracker::download_assets()?;

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

fn cmd_start_image(
    elf_path: &str,
    config_path: Option<&str>,
    volumes: &[String],
    cpus: Option<u32>,
    memory: Option<u32>,
) -> Result<()> {
    lima::require_running()?;

    let rt_config = match config_path {
        Some(p) => image::parse_runtime_config(p)?,
        None => image::RuntimeConfig::default(),
    };

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

    ui::status_line("Platform:", &mvm::infra::platform::current().to_string());

    if bootstrap::is_lima_required() {
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
    } else {
        ui::status_line("Lima VM:", "Not required (native KVM)");
    }

    if firecracker::is_running()? {
        let pid = shell::run_in_vm_stdout("cat ~/microvm/.fc-pid 2>/dev/null || echo '?'")
            .unwrap_or_else(|_| "?".to_string());
        ui::status_line("Firecracker:", &format!("Running (PID {})", pid));
    } else {
        if firecracker::is_installed()? {
            ui::status_line("Firecracker:", "Installed, not running");
        } else {
            ui::status_line("Firecracker:", "Not installed");
        }
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

fn cmd_completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "mvm", &mut std::io::stdout());
    Ok(())
}

fn cmd_events(tenant_id: &str, last: usize, json: bool) -> Result<()> {
    let entries = mvm::security::audit::read_audit_log(tenant_id, last)?;

    if entries.is_empty() {
        ui::info(&format!("No audit events for tenant '{}'.", tenant_id));
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for entry in &entries {
            println!(
                "{} [{}] {:?}{}{}",
                entry.timestamp,
                entry.tenant_id,
                entry.action,
                entry
                    .pool_id
                    .as_ref()
                    .map(|p| format!(" pool={}", p))
                    .unwrap_or_default(),
                entry
                    .instance_id
                    .as_ref()
                    .map(|i| format!(" instance={}", i))
                    .unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_destroy() -> Result<()> {
    let status = lima::get_status()?;

    if matches!(status, lima::LimaStatus::NotFound) {
        ui::info("Nothing to destroy. Lima VM does not exist.");
        return Ok(());
    }

    if matches!(status, lima::LimaStatus::Running) && firecracker::is_running()? {
        microvm::stop()?;
    }

    if !ui::confirm("This will delete the Lima VM and all microVM data. Continue?") {
        ui::info("Cancelled.");
        return Ok(());
    }

    ui::info("Destroying Lima VM...");
    lima::destroy()?;
    ui::success("Destroyed.");
    Ok(())
}

// ============================================================================
// Multi-tenant command handlers
// ============================================================================

fn cmd_tenant(action: TenantCmd, out_fmt: OutputFormat) -> Result<()> {
    use mvm::infra::display::{TenantInfo, TenantRow};
    use tenant::config::{TenantNet, TenantQuota};

    match action {
        TenantCmd::Create {
            id,
            net_id,
            subnet,
            max_vcpus,
            max_mem,
            max_running,
            max_warm,
        } => {
            naming::validate_id(&id, "Tenant")?;

            // Derive gateway from subnet (first usable IP)
            let gateway = derive_gateway(&subnet)?;
            let net = TenantNet::new(net_id, &subnet, &gateway);
            let quotas = TenantQuota {
                max_vcpus,
                max_mem_mib: max_mem,
                max_running,
                max_warm,
                ..TenantQuota::default()
            };

            let config = tenant::lifecycle::tenant_create(&id, net, quotas)?;
            ui::success(&format!("Tenant '{}' created.", config.tenant_id));
            ui::info(&format!(
                "  Network: {} (bridge: {})",
                config.net.ipv4_subnet, config.net.bridge_name
            ));
            Ok(())
        }
        TenantCmd::List { json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let tenant_ids = tenant::lifecycle::tenant_list()?;

            if fmt == OutputFormat::Table && tenant_ids.is_empty() {
                ui::info("No tenants found.");
                return Ok(());
            }

            let mut rows = Vec::new();
            for tid in &tenant_ids {
                if let Ok(config) = tenant::lifecycle::tenant_load(tid) {
                    rows.push(TenantRow {
                        tenant_id: config.tenant_id,
                        subnet: config.net.ipv4_subnet,
                        bridge: config.net.bridge_name,
                        max_vcpus: config.quotas.max_vcpus,
                        max_mem_mib: config.quotas.max_mem_mib,
                    });
                } else {
                    rows.push(TenantRow {
                        tenant_id: tid.clone(),
                        subnet: "?".to_string(),
                        bridge: "?".to_string(),
                        max_vcpus: 0,
                        max_mem_mib: 0,
                    });
                }
            }
            output::render_list(&rows, fmt);
            Ok(())
        }
        TenantCmd::Info { id, json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let config = tenant::lifecycle::tenant_load(&id)?;
            let info = TenantInfo {
                tenant_id: config.tenant_id,
                subnet: config.net.ipv4_subnet,
                gateway: config.net.gateway_ip,
                bridge: config.net.bridge_name,
                net_id: config.net.tenant_net_id,
                max_vcpus: config.quotas.max_vcpus,
                max_mem_mib: config.quotas.max_mem_mib,
                max_running: config.quotas.max_running,
                max_warm: config.quotas.max_warm,
                created_at: config.created_at,
            };
            output::render_one(&info, fmt);
            Ok(())
        }
        TenantCmd::Destroy {
            id,
            force,
            wipe_volumes,
        } => {
            if !force && !ui::confirm(&format!("Destroy tenant '{}' and all its resources?", id)) {
                ui::info("Cancelled.");
                return Ok(());
            }
            // Tear down bridge before destroying tenant
            if let Ok(config) = tenant::lifecycle::tenant_load(&id) {
                let _ = bridge::destroy_tenant_bridge(&config.net);
            }
            tenant::lifecycle::tenant_destroy(&id, wipe_volumes)?;
            ui::success(&format!("Tenant '{}' destroyed.", id));
            Ok(())
        }
        TenantCmd::Secrets { action } => match action {
            TenantSecretsCmd::Set { id, from_file } => {
                tenant::secrets::secrets_set(&id, &from_file)?;
                ui::success(&format!("Secrets set for tenant '{}'.", id));
                Ok(())
            }
            TenantSecretsCmd::Rotate { id } => {
                tenant::secrets::secrets_rotate(&id)?;
                ui::success(&format!("Secrets rotated for tenant '{}'.", id));
                Ok(())
            }
        },
    }
}

fn cmd_pool(action: PoolCmd, out_fmt: OutputFormat) -> Result<()> {
    use mvm::infra::display::{PoolInfo, PoolRow};

    match action {
        PoolCmd::Create {
            path,
            flake,
            profile,
            cpus,
            mem,
            data_disk,
        } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            let resources = pool::config::InstanceResources {
                vcpus: cpus,
                mem_mib: mem,
                data_disk_mib: data_disk,
            };
            let spec =
                pool::lifecycle::pool_create(tenant_id, pool_id, &flake, &profile, resources)?;
            ui::success(&format!(
                "Pool '{}/{}' created.",
                spec.tenant_id, spec.pool_id
            ));
            Ok(())
        }
        PoolCmd::List { tenant, json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let pool_ids = pool::lifecycle::pool_list(&tenant)?;

            if fmt == OutputFormat::Table && pool_ids.is_empty() {
                ui::info(&format!("No pools found for tenant '{}'.", tenant));
                return Ok(());
            }

            let mut rows = Vec::new();
            for pid in &pool_ids {
                if let Ok(spec) = pool::lifecycle::pool_load(&tenant, pid) {
                    rows.push(PoolRow {
                        pool_path: format!("{}/{}", tenant, pid),
                        profile: spec.profile,
                        vcpus: spec.instance_resources.vcpus,
                        mem_mib: spec.instance_resources.mem_mib,
                        desired_running: spec.desired_counts.running,
                        desired_warm: spec.desired_counts.warm,
                        desired_sleeping: spec.desired_counts.sleeping,
                    });
                }
            }
            output::render_list(&rows, fmt);
            Ok(())
        }
        PoolCmd::Info { path, json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            let spec = pool::lifecycle::pool_load(tenant_id, pool_id)?;
            let info = PoolInfo {
                pool_path: format!("{}/{}", spec.tenant_id, spec.pool_id),
                flake_ref: spec.flake_ref,
                profile: spec.profile,
                vcpus: spec.instance_resources.vcpus,
                mem_mib: spec.instance_resources.mem_mib,
                data_disk_mib: spec.instance_resources.data_disk_mib,
                desired_running: spec.desired_counts.running,
                desired_warm: spec.desired_counts.warm,
                desired_sleeping: spec.desired_counts.sleeping,
                seccomp_policy: spec.seccomp_policy,
            };
            output::render_one(&info, fmt);
            Ok(())
        }
        PoolCmd::Build { path, timeout } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            pool::build::pool_build(tenant_id, pool_id, timeout)
        }
        PoolCmd::Scale {
            path,
            running,
            warm,
            sleeping,
        } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            pool::lifecycle::pool_scale(tenant_id, pool_id, running, warm, sleeping)?;
            ui::success(&format!("Pool '{}' scaled.", path));
            Ok(())
        }
        PoolCmd::Destroy { path, force } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            pool::lifecycle::pool_destroy(tenant_id, pool_id, force)?;
            ui::success(&format!("Pool '{}' destroyed.", path));
            Ok(())
        }
        PoolCmd::Gc { path, keep } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            let removed = mvm::vm::disk_manager::cleanup_old_revisions(tenant_id, pool_id, keep)?;
            if removed > 0 {
                ui::success(&format!(
                    "Cleaned up {} old revisions for '{}'.",
                    removed, path
                ));
            } else {
                ui::info(&format!("No old revisions to clean up for '{}'.", path));
            }
            Ok(())
        }
    }
}

fn cmd_instance(action: InstanceCmd, out_fmt: OutputFormat) -> Result<()> {
    use mvm::infra::display::{InstanceInfo, InstanceRow};
    use mvm::vm::instance::lifecycle as inst;

    match action {
        InstanceCmd::Create { path } => {
            let (t, p) = naming::parse_pool_path(&path)?;
            let instance_id = inst::instance_create(t, p)?;
            ui::success(&format!(
                "Instance '{}' created in {}/{}.",
                instance_id, t, p
            ));
            Ok(())
        }
        InstanceCmd::List { tenant, pool, json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let tenants = match &tenant {
                Some(t) => vec![t.clone()],
                None => tenant::lifecycle::tenant_list()?,
            };

            let mut all_states = Vec::new();
            for tid in &tenants {
                let pools = match &pool {
                    Some(p) => vec![p.clone()],
                    None => pool::lifecycle::pool_list(tid)?,
                };
                for pid in &pools {
                    if let Ok(states) = inst::instance_list(tid, pid) {
                        all_states.extend(states);
                    }
                }
            }

            if fmt == OutputFormat::Table && all_states.is_empty() {
                ui::info("No instances found.");
                return Ok(());
            }

            let rows: Vec<InstanceRow> = all_states
                .iter()
                .map(|s| InstanceRow {
                    instance_path: format!("{}/{}/{}", s.tenant_id, s.pool_id, s.instance_id),
                    status: s.status.to_string(),
                    guest_ip: s.net.guest_ip.clone(),
                    tap_dev: s.net.tap_dev.clone(),
                    pid: s
                        .firecracker_pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                })
                .collect();
            output::render_list(&rows, fmt);
            Ok(())
        }
        InstanceCmd::Start { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_start(t, p, i)?;
            ui::success(&format!("Instance '{}' started.", path));
            Ok(())
        }
        InstanceCmd::Stop { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_stop(t, p, i)?;
            ui::success(&format!("Instance '{}' stopped.", path));
            Ok(())
        }
        InstanceCmd::Warm { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_warm(t, p, i)?;
            ui::success(&format!("Instance '{}' paused (warm).", path));
            Ok(())
        }
        InstanceCmd::Sleep { path, force } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_sleep(t, p, i, force)
        }
        InstanceCmd::Wake { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_wake(t, p, i)
        }
        InstanceCmd::Ssh { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_ssh(t, p, i)
        }
        InstanceCmd::Stats { path, json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let (t, p, i) = naming::parse_instance_path(&path)?;
            let state = inst::instance_list(t, p)?
                .into_iter()
                .find(|s| s.instance_id == i)
                .ok_or_else(|| anyhow::anyhow!("Instance not found: {}", path))?;

            let info = InstanceInfo {
                instance_path: format!("{}/{}/{}", t, p, i),
                status: state.status.to_string(),
                guest_ip: state.net.guest_ip.clone(),
                tap_dev: state.net.tap_dev.clone(),
                mac: state.net.mac.clone(),
                pid: state
                    .firecracker_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                revision: state.revision_hash.unwrap_or_else(|| "-".to_string()),
                last_started: state.last_started_at.unwrap_or_else(|| "-".to_string()),
                last_stopped: state.last_stopped_at.unwrap_or_else(|| "-".to_string()),
            };
            output::render_one(&info, fmt);
            Ok(())
        }
        InstanceCmd::Destroy { path, wipe_volumes } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            inst::instance_destroy(t, p, i, wipe_volumes)?;
            ui::success(&format!("Instance '{}' destroyed.", path));
            Ok(())
        }
        InstanceCmd::Logs { path } => {
            let (t, p, i) = naming::parse_instance_path(&path)?;
            let logs = inst::instance_logs(t, p, i)?;
            println!("{}", logs);
            Ok(())
        }
    }
}

fn cmd_agent(action: AgentCmd) -> Result<()> {
    use mvm::security::certs;

    match action {
        AgentCmd::Reconcile { desired, prune } => mvm::agent::reconcile(&desired, prune),
        AgentCmd::Desired { file, node_id } => {
            let desired = mvm::agent::generate_desired(&node_id)?;
            let json = serde_json::to_string_pretty(&desired)?;
            match file {
                Some(path) => {
                    std::fs::write(&path, &json)?;
                    ui::success(&format!("Desired state written to {}", path));
                }
                None => println!("{}", json),
            }
            Ok(())
        }
        AgentCmd::Serve {
            interval_secs,
            desired,
            listen,
        } => mvm::agent::serve(interval_secs, desired.as_deref(), listen.as_deref()),
        AgentCmd::Certs { action } => match action {
            AgentCertsCmd::Init { ca } => {
                match ca {
                    Some(ca_path) => {
                        certs::init_ca(&ca_path)?;
                        ui::success("CA certificate initialized.");
                    }
                    None => {
                        // Generate self-signed dev CA + node cert
                        let node_id = format!(
                            "mvm-{}",
                            uuid::Uuid::new_v4()
                                .to_string()
                                .split('-')
                                .next()
                                .unwrap_or("dev")
                        );
                        let paths = certs::generate_self_signed(&node_id)?;
                        ui::success(&format!(
                            "Self-signed certificates generated for '{}'.",
                            node_id
                        ));
                        ui::info(&format!("  CA:   {}", paths.ca_cert));
                        ui::info(&format!("  Cert: {}", paths.node_cert));
                        ui::info(&format!("  Key:  {}", paths.node_key));
                    }
                }
                Ok(())
            }
            AgentCertsCmd::Rotate => {
                let node_id =
                    shell::run_in_vm_stdout("cat /var/lib/mvm/node_id 2>/dev/null || echo mvm-dev")
                        .unwrap_or_else(|_| "mvm-dev".to_string());
                let paths = certs::rotate_certs(node_id.trim())?;
                ui::success("Certificates rotated.");
                ui::info(&format!("  Cert: {}", paths.node_cert));
                Ok(())
            }
            AgentCertsCmd::Status { json } => certs::show_status(json),
        },
    }
}

fn cmd_net(action: NetCmd, out_fmt: OutputFormat) -> Result<()> {
    match action {
        NetCmd::Verify { json } => {
            let fmt = if json { OutputFormat::Json } else { out_fmt };
            let tenants = tenant::lifecycle::tenant_list()?;
            let mut reports = Vec::new();
            let mut all_issues = Vec::new();

            for tid in &tenants {
                if let Ok(config) = tenant::lifecycle::tenant_load(tid) {
                    let report = bridge::full_bridge_report(tid, &config.net)?;
                    for issue in &report.issues {
                        all_issues.push(format!("{}: {}", tid, issue));
                    }
                    reports.push(report);
                }
            }

            if fmt != OutputFormat::Table {
                // JSON/YAML: always output structured data
                match fmt {
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&reports)?);
                    }
                    OutputFormat::Yaml => {
                        println!("{}", serde_yaml::to_string(&reports)?);
                    }
                    _ => unreachable!(),
                }
            } else if all_issues.is_empty() {
                ui::success("All tenant networks verified.");
                for r in &reports {
                    ui::info(&format!(
                        "  {} ({}) — bridge: {}, TAPs: {}",
                        r.tenant_id,
                        r.subnet,
                        if r.bridge_up { "UP" } else { "DOWN" },
                        r.tap_devices.len(),
                    ));
                }
            } else {
                ui::warn("Network issues found:");
                for issue in &all_issues {
                    ui::error(&format!("  {}", issue));
                }
            }
            Ok(())
        }
    }
}

fn cmd_node(action: NodeCmd, out_fmt: OutputFormat) -> Result<()> {
    let _ = out_fmt; // node commands already handle json flag internally
    match action {
        NodeCmd::Info { json } => mvm::node::info(json),
        NodeCmd::Stats { json } => mvm::node::stats(json),
        NodeCmd::Disk { json } => {
            let report = mvm::vm::disk_manager::disk_usage_report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Total disk usage: {} bytes", report.total_bytes);
                for t in &report.tenants {
                    println!("  Tenant '{}': {} bytes", t.tenant_id, t.total_bytes);
                    for p in &t.pools {
                        println!(
                            "    Pool '{}': artifacts={}, instances={}, total={}",
                            p.pool_id, p.artifacts_bytes, p.instances_bytes, p.total_bytes
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCmd::Gc { keep } => {
            let tenant_ids = mvm::vm::tenant::lifecycle::tenant_list()?;
            let mut total_removed = 0u32;
            for tid in &tenant_ids {
                if let Ok(pool_ids) = mvm::vm::pool::lifecycle::pool_list(tid) {
                    for pid in &pool_ids {
                        match mvm::vm::disk_manager::cleanup_old_revisions(tid, pid, keep) {
                            Ok(n) => total_removed += n,
                            Err(e) => ui::warn(&format!("GC failed for {}/{}: {}", tid, pid, e)),
                        }
                    }
                }
            }
            if total_removed > 0 {
                ui::success(&format!(
                    "Removed {} old revisions across all pools.",
                    total_removed
                ));
            } else {
                ui::info("No old revisions to clean up.");
            }
            Ok(())
        }
    }
}

fn cmd_coordinator(action: CoordinatorCmd, _out_fmt: OutputFormat) -> Result<()> {
    use mvm::agent::{AgentRequest, AgentResponse, DesiredState};
    use mvm::coordinator::client::{CoordinatorClient, run_coordinator_command};

    match action {
        CoordinatorCmd::Push { desired, node } => {
            let json = std::fs::read_to_string(&desired)
                .with_context(|| format!("Failed to read desired state file: {}", desired))?;
            let state: DesiredState = serde_json::from_str(&json)
                .with_context(|| "Failed to parse desired state JSON")?;

            let addr: std::net::SocketAddr = node
                .parse()
                .with_context(|| format!("Invalid node address: {}", node))?;

            run_coordinator_command(async {
                let client = CoordinatorClient::new()?;
                let response = client.send(addr, &AgentRequest::Reconcile(state)).await?;
                match response {
                    AgentResponse::ReconcileResult(report) => {
                        ui::success(&format!(
                            "Reconcile pushed to {}. Instances: +{} started, {} errors",
                            node,
                            report.instances_started,
                            report.errors.len()
                        ));
                        if !report.errors.is_empty() {
                            for err in &report.errors {
                                ui::error(&format!("  {}", err));
                            }
                        }
                    }
                    AgentResponse::Error { code, message } => {
                        ui::error(&format!("Node error ({}): {}", code, message));
                    }
                    _ => {
                        ui::warn("Unexpected response type from node.");
                    }
                }
                Ok(())
            })
        }
        CoordinatorCmd::Status { node } => {
            let addr: std::net::SocketAddr = node
                .parse()
                .with_context(|| format!("Invalid node address: {}", node))?;

            run_coordinator_command(async {
                let client = CoordinatorClient::new()?;
                let response = client.send(addr, &AgentRequest::NodeInfo).await?;
                match response {
                    AgentResponse::NodeInfo(info) => {
                        println!("{}", serde_json::to_string_pretty(&info)?);
                    }
                    AgentResponse::Error { code, message } => {
                        ui::error(&format!("Node error ({}): {}", code, message));
                    }
                    _ => {
                        ui::warn("Unexpected response type from node.");
                    }
                }
                Ok(())
            })
        }
        CoordinatorCmd::ListInstances { node, tenant, pool } => {
            let addr: std::net::SocketAddr = node
                .parse()
                .with_context(|| format!("Invalid node address: {}", node))?;

            run_coordinator_command(async {
                let client = CoordinatorClient::new()?;
                let response = client
                    .send(
                        addr,
                        &AgentRequest::InstanceList {
                            tenant_id: tenant.clone(),
                            pool_id: pool,
                        },
                    )
                    .await?;
                match response {
                    AgentResponse::InstanceList(instances) => {
                        if instances.is_empty() {
                            ui::info(&format!("No instances found for tenant '{}'.", tenant));
                        } else {
                            println!("{}", serde_json::to_string_pretty(&instances)?);
                        }
                    }
                    AgentResponse::Error { code, message } => {
                        ui::error(&format!("Node error ({}): {}", code, message));
                    }
                    _ => {
                        ui::warn("Unexpected response type from node.");
                    }
                }
                Ok(())
            })
        }
        CoordinatorCmd::Wake {
            node,
            tenant,
            pool,
            instance,
        } => {
            let addr: std::net::SocketAddr = node
                .parse()
                .with_context(|| format!("Invalid node address: {}", node))?;

            run_coordinator_command(async {
                let client = CoordinatorClient::new()?;
                let response = client
                    .send(
                        addr,
                        &AgentRequest::WakeInstance {
                            tenant_id: tenant,
                            pool_id: pool,
                            instance_id: instance.clone(),
                        },
                    )
                    .await?;
                match response {
                    AgentResponse::WakeResult { success } => {
                        if success {
                            ui::success(&format!("Instance '{}' woken.", instance));
                        } else {
                            ui::error(&format!("Failed to wake instance '{}'.", instance));
                        }
                    }
                    AgentResponse::Error { code, message } => {
                        ui::error(&format!("Node error ({}): {}", code, message));
                    }
                    _ => {
                        ui::warn("Unexpected response type from node.");
                    }
                }
                Ok(())
            })
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Derive gateway IP from a CIDR subnet (first usable IP, typically .1).
fn derive_gateway(subnet: &str) -> Result<String> {
    let parts: Vec<&str> = subnet.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid CIDR subnet: {}", subnet);
    }
    let octets: Vec<&str> = parts[0].split('.').collect();
    if octets.len() != 4 {
        anyhow::bail!("Invalid IPv4 address in subnet: {}", subnet);
    }
    // Replace last octet with 1
    Ok(format!("{}.{}.{}.1", octets[0], octets[1], octets[2]))
}
