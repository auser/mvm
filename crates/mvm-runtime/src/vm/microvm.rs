use anyhow::Result;

use super::{firecracker, lima, network};
use crate::config::*;
use crate::shell::{run_in_vm, run_in_vm_stdout, run_in_vm_visible};
use crate::ui;
use crate::vm::image::RuntimeVolume;

/// Resolve MICROVM_DIR (~) to an absolute path inside the Lima VM.
fn resolve_microvm_dir() -> Result<String> {
    run_in_vm_stdout(&format!("echo {}", MICROVM_DIR))
}

/// Start the Firecracker daemon inside the Lima VM (background).
fn start_firecracker_daemon(abs_dir: &str) -> Result<()> {
    ui::info("Starting Firecracker...");
    run_in_vm_visible(&format!(
        r#"
        mkdir -p {dir}
        sudo rm -f {socket}
        touch {dir}/firecracker.log
        sudo bash -c 'nohup setsid firecracker --api-sock {socket} --enable-pci \
            </dev/null >{dir}/firecracker.log 2>&1 &
            echo $! > {dir}/.fc-pid'

        echo "[mvm] Waiting for API socket..."
        for i in $(seq 1 30); do
            [ -S {socket} ] && break
            sleep 0.1
        done

        if [ ! -S {socket} ]; then
            echo "[mvm] ERROR: API socket did not appear." >&2
            exit 1
        fi
        echo "[mvm] Firecracker started."
        "#,
        socket = API_SOCKET,
        dir = abs_dir,
    ))
}

/// Send API PUT request to Firecracker (run inside the VM).
fn api_put(path: &str, data: &str) -> Result<()> {
    let script = format!(
        r#"
        response=$(sudo curl -s -w "\n%{{http_code}}" -X PUT --unix-socket {socket} \
            --data '{data}' "http://localhost{path}")
        code=$(echo "$response" | tail -1)
        body=$(echo "$response" | sed '$d')
        if [ "$code" -ge 400 ]; then
            echo "[mvm] ERROR: PUT {path} returned $code: $body" >&2
            exit 1
        fi
        "#,
        socket = API_SOCKET,
        path = path,
        data = data,
    );
    run_in_vm_visible(&script)
}

/// Configure the microVM via the Firecracker API.
fn configure_microvm(state: &MvmState, abs_dir: &str) -> Result<()> {
    ui::info("Configuring logger...");
    api_put(
        "/logger",
        &format!(
            r#"{{"log_path": "{dir}/firecracker.log", "level": "Debug", "show_level": true, "show_log_origin": true}}"#,
            dir = abs_dir,
        ),
    )?;

    let kernel_path = format!("{}/{}", abs_dir, state.kernel);
    let rootfs_path = format!("{}/{}", abs_dir, state.rootfs);

    // Use kernel cmdline IP params (no SSH-based guest network config)
    let kernel_boot_args = format!(
        "console=ttyS0 reboot=k panic=1 ip={guest}::{gateway}:255.255.255.252::eth0:off",
        guest = GUEST_IP,
        gateway = TAP_IP,
    );

    ui::info(&format!("Setting boot source: {}", state.kernel));
    api_put(
        "/boot-source",
        &format!(
            r#"{{"kernel_image_path": "{kernel}", "boot_args": "{args}"}}"#,
            kernel = kernel_path,
            args = kernel_boot_args,
        ),
    )?;

    ui::info(&format!("Setting rootfs: {}", state.rootfs));
    api_put(
        "/drives/rootfs",
        &format!(
            r#"{{"drive_id": "rootfs", "path_on_host": "{rootfs}", "is_root_device": true, "is_read_only": false}}"#,
            rootfs = rootfs_path,
        ),
    )?;

    ui::info("Setting network interface...");
    api_put(
        "/network-interfaces/net1",
        &format!(
            r#"{{"iface_id": "net1", "guest_mac": "{mac}", "host_dev_name": "{tap}"}}"#,
            mac = FC_MAC,
            tap = TAP_DEV,
        ),
    )?;

    Ok(())
}

/// Full start sequence: network, firecracker, configure, boot (headless).
///
/// MicroVMs never have SSH enabled. They run as headless workloads and
/// communicate via vsock. Use `mvm shell` to access the Lima VM environment.
pub fn start() -> Result<()> {
    lima::require_running()?;

    // Check if already running
    if firecracker::is_running()? {
        ui::info("Firecracker is already running.");
        ui::info("Use 'mvm stop' to shut down, then 'mvm start' to restart.");
        return Ok(());
    }

    // Read state file for asset paths
    let state = read_state_or_discover()?;

    // Resolve ~/microvm to absolute path so it works in both user and sudo contexts
    let abs_dir = resolve_microvm_dir()?;

    // Set up networking
    network::setup()?;

    // Start Firecracker daemon
    start_firecracker_daemon(&abs_dir)?;

    // Configure microVM
    configure_microvm(&state, &abs_dir)?;

    // Start the instance
    ui::info("Starting microVM...");
    std::thread::sleep(std::time::Duration::from_millis(15));
    api_put("/actions", r#"{"action_type": "InstanceStart"}"#)?;

    ui::banner(&[
        "MicroVM is running!",
        "",
        &format!("  Guest IP: {}", GUEST_IP),
        "",
        "Use 'mvm status' to check the microVM.",
        "Use 'mvm stop' to shut down the microVM.",
        "Use 'mvm shell' to access the Lima VM environment.",
    ]);

    Ok(())
}

/// Stop the microVM: kill Firecracker, clean up networking.
pub fn stop() -> Result<()> {
    lima::require_running()?;

    if !firecracker::is_running()? {
        ui::info("MicroVM is not running.");
        return Ok(());
    }

    ui::info("Stopping microVM...");

    // Try graceful shutdown via API
    let _ = run_in_vm(&format!(
        r#"sudo curl -s -X PUT --unix-socket {socket} \
            --data '{{"action_type": "SendCtrlAltDel"}}' \
            "http://localhost/actions" 2>/dev/null || true"#,
        socket = API_SOCKET,
    ));

    // Give it a moment, then force kill
    std::thread::sleep(std::time::Duration::from_secs(2));

    run_in_vm(&format!(
        r#"
        if [ -f {dir}/.fc-pid ]; then
            sudo kill $(cat {dir}/.fc-pid) 2>/dev/null || true
            rm -f {dir}/.fc-pid
        fi
        sudo pkill -x firecracker 2>/dev/null || true
        sudo rm -f {socket}
        rm -f {dir}/.mvm-run-info
        "#,
        dir = MICROVM_DIR,
        socket = API_SOCKET,
    ))?;

    // Tear down networking
    network::teardown()?;

    ui::success("MicroVM stopped.");
    Ok(())
}

/// Read the state file, or discover assets by listing files.
fn read_state_or_discover() -> Result<MvmState> {
    let json = run_in_vm_stdout(&format!(
        "cat {dir}/.mvm-state 2>/dev/null || echo 'null'",
        dir = MICROVM_DIR,
    ))?;

    if let Ok(state) = serde_json::from_str::<MvmState>(&json)
        && !state.kernel.is_empty()
        && !state.rootfs.is_empty()
        && !state.ssh_key.is_empty()
    {
        return Ok(state);
    }

    // Discover from files
    let kernel = run_in_vm_stdout(&format!(
        "cd {} && ls vmlinux-* 2>/dev/null | tail -1",
        MICROVM_DIR
    ))?;
    let rootfs = run_in_vm_stdout(&format!(
        "cd {} && ls *.ext4 2>/dev/null | tail -1",
        MICROVM_DIR
    ))?;
    let ssh_key = run_in_vm_stdout(&format!(
        "cd {} && ls *.id_rsa 2>/dev/null | tail -1",
        MICROVM_DIR
    ))?;

    if kernel.is_empty() || rootfs.is_empty() || ssh_key.is_empty() {
        anyhow::bail!(
            "Missing microVM assets in {}. Run 'mvm setup' first.\n  kernel={:?} rootfs={:?} ssh_key={:?}",
            MICROVM_DIR,
            kernel,
            rootfs,
            ssh_key,
        );
    }

    Ok(MvmState {
        kernel,
        rootfs,
        ssh_key,
        fc_pid: None,
    })
}

// ============================================================================
// Flake-based run: build from Nix flake artifacts, boot headless FC VM
// ============================================================================

/// Configuration for running a Firecracker VM from flake-built artifacts.
pub struct FlakeRunConfig {
    /// Absolute path to the kernel image inside the Lima VM.
    pub vmlinux_path: String,
    /// Absolute path to the root filesystem inside the Lima VM.
    pub rootfs_path: String,
    /// Nix store revision hash.
    pub revision_hash: String,
    /// Original flake reference (for display / status).
    pub flake_ref: String,
    /// Number of vCPUs.
    pub cpus: u32,
    /// Memory in MiB.
    pub memory: u32,
    /// Extra volumes to attach (mounted via config drive, not SSH).
    pub volumes: Vec<RuntimeVolume>,
}

/// Boot a Firecracker VM from flake-built artifacts (headless).
///
/// MicroVMs never have SSH enabled. They run as headless workloads and
/// communicate via vsock. Use `mvm shell` to access the Lima VM environment.
pub fn run_from_build(config: &FlakeRunConfig) -> Result<()> {
    lima::require_running()?;

    // Stop any existing FC instance
    if firecracker::is_running()? {
        ui::info("Stopping existing microVM...");
        stop()?;
    }

    // Set up TAP/NAT network (dev-mode 172.16.0.x)
    network::setup()?;

    // Use ~/microvm/ as the working dir so stop() can find .fc-pid
    let abs_dir = resolve_microvm_dir()?;

    // Start Firecracker daemon
    start_firecracker_daemon(&abs_dir)?;

    // Configure VM via Firecracker API
    configure_flake_microvm(config, &abs_dir)?;

    // Boot the instance
    ui::info("Starting microVM...");
    std::thread::sleep(std::time::Duration::from_millis(15));
    api_put("/actions", r#"{"action_type": "InstanceStart"}"#)?;

    // Persist run info for `mvm status`
    write_run_info(config)?;

    ui::banner(&[
        "MicroVM is running!",
        "",
        &format!("  Guest IP: {}", GUEST_IP),
        &format!("  Revision: {}", config.revision_hash),
        "",
        "Use 'mvm status' to check the microVM.",
        "Use 'mvm stop' to shut down the microVM.",
        "Use 'mvm shell' to access the Lima VM environment.",
    ]);

    Ok(())
}

/// Configure a flake-built microVM via the Firecracker API.
fn configure_flake_microvm(config: &FlakeRunConfig, abs_dir: &str) -> Result<()> {
    ui::info("Configuring logger...");
    api_put(
        "/logger",
        &format!(
            r#"{{"log_path": "{dir}/firecracker.log", "level": "Debug", "show_level": true, "show_log_origin": true}}"#,
            dir = abs_dir,
        ),
    )?;

    // Boot args with static IP configuration via kernel cmdline
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=1 ip={guest}::{gateway}:255.255.255.252::eth0:off",
        guest = GUEST_IP,
        gateway = TAP_IP,
    );

    ui::info(&format!("Setting boot source: {}", config.vmlinux_path));
    api_put(
        "/boot-source",
        &format!(
            r#"{{"kernel_image_path": "{kernel}", "boot_args": "{args}"}}"#,
            kernel = config.vmlinux_path,
            args = boot_args,
        ),
    )?;

    ui::info(&format!(
        "Setting machine config: {} vCPUs, {} MiB",
        config.cpus, config.memory
    ));
    api_put(
        "/machine-config",
        &format!(
            r#"{{"vcpu_count": {cpus}, "mem_size_mib": {mem}}}"#,
            cpus = config.cpus,
            mem = config.memory,
        ),
    )?;

    ui::info(&format!("Setting rootfs: {}", config.rootfs_path));
    api_put(
        "/drives/rootfs",
        &format!(
            r#"{{"drive_id": "rootfs", "path_on_host": "{rootfs}", "is_root_device": true, "is_read_only": false}}"#,
            rootfs = config.rootfs_path,
        ),
    )?;

    for (idx, vol) in config.volumes.iter().enumerate() {
        let drive_id = format!("vol{}", idx);
        ui::info(&format!(
            "Attaching volume {} -> {} (size {})",
            vol.host, vol.guest, vol.size
        ));
        api_put(
            &format!("/drives/{}", drive_id),
            &format!(
                r#"{{"drive_id": "{id}", "path_on_host": "{host}", "is_root_device": false, "is_read_only": false}}"#,
                id = drive_id,
                host = vol.host,
            ),
        )?;
    }

    ui::info("Setting network interface...");
    api_put(
        "/network-interfaces/net1",
        &format!(
            r#"{{"iface_id": "net1", "guest_mac": "{mac}", "host_dev_name": "{tap}"}}"#,
            mac = FC_MAC,
            tap = TAP_DEV,
        ),
    )?;

    Ok(())
}

/// Persist run info so `mvm status` can distinguish run modes.
fn write_run_info(config: &FlakeRunConfig) -> Result<()> {
    let info = RunInfo {
        mode: "flake".to_string(),
        revision: Some(config.revision_hash.clone()),
        flake_ref: Some(config.flake_ref.clone()),
        guest_user: String::new(),
        cpus: config.cpus,
        memory: config.memory,
    };
    let json = serde_json::to_string(&info)?;
    run_in_vm(&format!(
        "echo '{}' > {dir}/.mvm-run-info",
        json,
        dir = MICROVM_DIR,
    ))?;
    Ok(())
}

/// Read persisted run info (returns None if file doesn't exist).
pub fn read_run_info() -> Option<RunInfo> {
    let json = run_in_vm_stdout(&format!(
        "cat {dir}/.mvm-run-info 2>/dev/null || echo 'null'",
        dir = MICROVM_DIR,
    ))
    .ok()?;
    serde_json::from_str(&json).ok()
}
