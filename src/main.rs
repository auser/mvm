use anyhow::Result;
use clap::{Parser, Subcommand};

// Import from the library crate
use mvm::infra::{bootstrap, config, shell, ui, upgrade};
use mvm::vm::{bridge, naming, pool, tenant};
use mvm::vm::{firecracker, image, lima, microvm};

#[derive(Parser)]
#[command(
    name = "mvm",
    version,
    about = "Multi-tenant Firecracker microVM fleet manager"
)]
struct Cli {
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
}

// ============================================================================
// Command dispatch
// ============================================================================

fn main() -> Result<()> {
    let cli = Cli::parse();

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

        // --- Multi-tenant ---
        Commands::Tenant { action } => cmd_tenant(action),
        Commands::Pool { action } => cmd_pool(action),
        Commands::Instance { action } => cmd_instance(action),
        Commands::Agent { action } => cmd_agent(action),
        Commands::Net { action } => cmd_net(action),
        Commands::Node { action } => cmd_node(action),
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

fn cmd_tenant(action: TenantCmd) -> Result<()> {
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
            let tenants = tenant::lifecycle::tenant_list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tenants)?);
            } else if tenants.is_empty() {
                ui::info("No tenants found.");
            } else {
                for t in &tenants {
                    println!("  {}", t);
                }
            }
            Ok(())
        }
        TenantCmd::Info { id, json } => {
            let config = tenant::lifecycle::tenant_load(&id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&config)?);
            } else {
                ui::info(&format!("Tenant: {}", config.tenant_id));
                ui::info(&format!("  Subnet: {}", config.net.ipv4_subnet));
                ui::info(&format!("  Bridge: {}", config.net.bridge_name));
                ui::info(&format!(
                    "  Quotas: {} vCPUs, {} MiB, {} running, {} warm",
                    config.quotas.max_vcpus,
                    config.quotas.max_mem_mib,
                    config.quotas.max_running,
                    config.quotas.max_warm,
                ));
            }
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

fn cmd_pool(action: PoolCmd) -> Result<()> {
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
            let pools = pool::lifecycle::pool_list(&tenant)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pools)?);
            } else if pools.is_empty() {
                ui::info(&format!("No pools found for tenant '{}'.", tenant));
            } else {
                for p in &pools {
                    println!("  {}/{}", tenant, p);
                }
            }
            Ok(())
        }
        PoolCmd::Info { path, json } => {
            let (tenant_id, pool_id) = naming::parse_pool_path(&path)?;
            let spec = pool::lifecycle::pool_load(tenant_id, pool_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&spec)?);
            } else {
                ui::info(&format!("Pool: {}/{}", spec.tenant_id, spec.pool_id));
                ui::info(&format!("  Flake: {}", spec.flake_ref));
                ui::info(&format!("  Profile: {}", spec.profile));
                ui::info(&format!(
                    "  Resources: {} vCPUs, {} MiB",
                    spec.instance_resources.vcpus, spec.instance_resources.mem_mib
                ));
                ui::info(&format!(
                    "  Desired: {} running, {} warm, {} sleeping",
                    spec.desired_counts.running,
                    spec.desired_counts.warm,
                    spec.desired_counts.sleeping,
                ));
            }
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
    }
}

fn cmd_instance(action: InstanceCmd) -> Result<()> {
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

            if json {
                println!("{}", serde_json::to_string_pretty(&all_states)?);
            } else if all_states.is_empty() {
                ui::info("No instances found.");
            } else {
                for s in &all_states {
                    println!(
                        "  {}/{}/{}  status={}  ip={}  pid={}",
                        s.tenant_id,
                        s.pool_id,
                        s.instance_id,
                        s.status,
                        s.net.guest_ip,
                        s.firecracker_pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
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
            let (t, p, i) = naming::parse_instance_path(&path)?;
            let state = inst::instance_list(t, p)?
                .into_iter()
                .find(|s| s.instance_id == i)
                .ok_or_else(|| anyhow::anyhow!("Instance not found: {}", path))?;

            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                ui::info(&format!("Instance: {}/{}/{}", t, p, i));
                ui::info(&format!("  Status: {}", state.status));
                ui::info(&format!("  IP: {}", state.net.guest_ip));
                ui::info(&format!("  TAP: {}", state.net.tap_dev));
                ui::info(&format!("  MAC: {}", state.net.mac));
                if let Some(pid) = state.firecracker_pid {
                    ui::info(&format!("  FC PID: {}", pid));
                }
                if let Some(ref rev) = state.revision_hash {
                    ui::info(&format!("  Revision: {}", rev));
                }
                if let Some(ref ts) = state.last_started_at {
                    ui::info(&format!("  Last started: {}", ts));
                }
                if let Some(ref ts) = state.last_stopped_at {
                    ui::info(&format!("  Last stopped: {}", ts));
                }
            }
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

fn cmd_net(action: NetCmd) -> Result<()> {
    match action {
        NetCmd::Verify { json } => {
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

            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
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

fn cmd_node(action: NodeCmd) -> Result<()> {
    match action {
        NodeCmd::Info { json } => mvm::node::info(json),
        NodeCmd::Stats { json } => mvm::node::stats(json),
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
