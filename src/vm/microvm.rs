use anyhow::Result;

use super::{firecracker, lima, network};
use crate::infra::config::*;
use crate::infra::shell::{replace_process, run_in_vm, run_in_vm_stdout, run_in_vm_visible};
use crate::infra::ui;

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

    // Determine boot args
    let kernel_boot_args = "keep_bootcon console=ttyS0 reboot=k panic=1";

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

/// Wait for SSH to become available on the microVM.
fn wait_for_ssh(ssh_key: &str) -> Result<()> {
    ui::info("Waiting for microVM to boot...");
    let script = format!(
        r#"
        sleep 5
        echo "[mvm] Waiting for SSH (up to 30s)..."
        for i in $(seq 1 30); do
            if ssh -i {dir}/{key} \
                -o ConnectTimeout=2 -o StrictHostKeyChecking=no \
                -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
                root@{guest_ip} true 2>/dev/null; then
                echo "[mvm] SSH is ready!"
                exit 0
            fi
            printf "."
            sleep 1
        done
        echo ""
        echo "[mvm] ERROR: SSH not available after 30 seconds." >&2
        exit 1
        "#,
        dir = MICROVM_DIR,
        key = ssh_key,
        guest_ip = GUEST_IP,
    );
    run_in_vm_visible(&script)
}

/// Configure networking inside the microVM guest.
fn configure_guest_network(ssh_key: &str) -> Result<()> {
    ui::info("Configuring guest networking...");
    let ssh_opts = format!(
        "-i {dir}/{key} -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
        dir = MICROVM_DIR,
        key = ssh_key,
    );
    run_in_vm(&format!(
        r#"
        ssh {opts} root@{guest} "ip route add default via {tap_ip} dev eth0" 2>/dev/null || true
        ssh {opts} root@{guest} "echo 'nameserver 8.8.8.8' > /etc/resolv.conf" 2>/dev/null || true
        "#,
        opts = ssh_opts,
        guest = GUEST_IP,
        tap_ip = TAP_IP,
    ))?;
    Ok(())
}

/// Full start sequence: network, firecracker, configure, boot, SSH.
pub fn start() -> Result<()> {
    lima::require_running()?;

    // Check if already running
    if firecracker::is_running()? {
        ui::info("Firecracker is already running.");
        if is_ssh_reachable()? {
            ui::info("MicroVM is running. Connecting...");
            return ssh();
        } else {
            anyhow::bail!(
                "Firecracker running but microVM not reachable. Run 'mvm stop' then 'mvm start'."
            );
        }
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

    // Wait for SSH
    wait_for_ssh(&state.ssh_key)?;

    // Configure guest networking
    configure_guest_network(&state.ssh_key)?;

    ui::banner(&[
        "MicroVM is running!",
        "",
        "Use 'mvm ssh' to reconnect after exiting.",
        "Use 'mvm stop' to shut down the microVM.",
    ]);

    // Drop into interactive SSH
    ssh()
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
        "#,
        dir = MICROVM_DIR,
        socket = API_SOCKET,
    ))?;

    // Tear down networking
    network::teardown()?;

    ui::success("MicroVM stopped.");
    Ok(())
}

/// SSH into the running microVM.
pub fn ssh() -> Result<()> {
    lima::require_running()?;

    let state = read_state_or_discover()?;
    let ssh_cmd = format!(
        "ssh -i {dir}/{key} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -t root@{guest}",
        dir = MICROVM_DIR,
        key = state.ssh_key,
        guest = GUEST_IP,
    );

    replace_process("limactl", &["shell", VM_NAME, "bash", "-c", &ssh_cmd])
}

/// Check if the microVM is reachable via SSH.
pub fn is_ssh_reachable() -> Result<bool> {
    let output = run_in_vm(&format!(
        r#"ssh -i {dir}/*.id_rsa \
            -o ConnectTimeout=2 -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
            root@{guest} true 2>/dev/null"#,
        dir = MICROVM_DIR,
        guest = GUEST_IP,
    ))?;
    Ok(output.status.success())
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
