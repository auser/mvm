use anyhow::{Context, Result};

use super::artifacts;
use super::config::{ArtifactPaths, BuildRevision, pool_artifacts_dir, pool_dir};
use super::lifecycle::pool_load;
use crate::infra::config::ARCH;
use crate::infra::http;
use crate::infra::shell;
use crate::infra::ui;
use crate::vm::bridge;
use crate::vm::instance::fc_config::{
    BootSource, Drive, FcConfig, MachineConfig, NetworkInterface,
};
use crate::vm::instance::net;
use crate::vm::instance::state::InstanceNet;
use crate::vm::naming;
use crate::vm::tenant::config::{TenantNet, tenant_ssh_key_path};
use crate::vm::tenant::lifecycle::tenant_load;

/// Base directory for builder infrastructure.
const BUILDER_DIR: &str = "/var/lib/mvm/builder";

/// Builder VM resource defaults.
const BUILDER_VCPUS: u8 = 4;
const BUILDER_MEM_MIB: u32 = 4096;

/// IP offset reserved for the builder VM within each tenant subnet.
const BUILDER_IP_OFFSET: u8 = 2;

/// Default build timeout in seconds (30 minutes).
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// Build artifacts for a pool using an ephemeral Firecracker builder microVM.
///
/// The build uses the pool's `flake_ref` and `profile` to evaluate:
///   `nix build <flake_ref>#packages.<system>.tenant-<profile>`
///
/// `flake_ref` can be any valid Nix flake reference:
///   - Built-in:  `./nix` or path to mvm's nix/ directory
///   - Local:     `/path/to/user-flake`
///   - Remote:    `github:org/repo`, `github:org/repo?rev=abc123`
pub fn pool_build(tenant_id: &str, pool_id: &str, timeout_secs: Option<u64>) -> Result<()> {
    let timeout = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let spec = pool_load(tenant_id, pool_id)?;
    let tenant = tenant_load(tenant_id)?;

    ui::info(&format!(
        "Building {}/{} (flake: {}, profile: {})",
        tenant_id, pool_id, spec.flake_ref, spec.profile
    ));

    // Step 1: Ensure builder artifacts exist (kernel + rootfs for the builder VM)
    ensure_builder_artifacts()?;

    // Step 2: Ensure tenant bridge is up
    bridge::ensure_tenant_bridge(&tenant.net)?;

    // Step 3: Create a unique build ID for this run
    let build_id = naming::generate_instance_id().replace("i-", "b-");
    let build_run_dir = format!("{}/run/{}", BUILDER_DIR, build_id);

    shell::run_in_vm(&format!("mkdir -p {}", build_run_dir))?;

    ui::info(&format!("Build ID: {}", build_id));

    // Step 4: Boot ephemeral builder VM
    let builder_net = builder_instance_net(&tenant.net);
    let builder_pid = boot_builder(&build_run_dir, &builder_net, &tenant.net)?;

    // Step 5: Wait for builder to be ready, then run the build
    let result = run_nix_build(
        &builder_net.guest_ip,
        &tenant_ssh_key_path(tenant_id),
        &spec.flake_ref,
        &spec.role,
        &spec.profile,
        timeout,
    );

    // Step 6: Whether build succeeded or failed, extract artifacts if possible,
    // then always tear down the builder
    let build_result = match result {
        Ok(nix_output_path) => {
            ui::info("Build completed, extracting artifacts...");
            extract_artifacts(
                &builder_net.guest_ip,
                &tenant_ssh_key_path(tenant_id),
                &nix_output_path,
                tenant_id,
                pool_id,
            )
        }
        Err(e) => Err(e),
    };

    // Step 7: Always tear down builder
    teardown_builder(builder_pid, &builder_net, &build_run_dir)?;

    // Propagate build result after cleanup
    let revision_hash = build_result?;

    // Step 8: Record revision and update symlink
    let revision = BuildRevision {
        revision_hash: revision_hash.clone(),
        flake_ref: spec.flake_ref.clone(),
        flake_lock_hash: revision_hash.clone(), // TODO: extract actual flake.lock hash
        artifact_paths: ArtifactPaths {
            vmlinux: "vmlinux".to_string(),
            rootfs: "rootfs.ext4".to_string(),
            fc_base_config: "fc-base.json".to_string(),
        },
        built_at: http::utc_now(),
    };

    artifacts::record_revision(tenant_id, pool_id, &revision)?;
    record_build_history(tenant_id, pool_id, &revision)?;

    ui::success(&format!(
        "Build complete: {}/{} revision {}",
        tenant_id, pool_id, revision_hash
    ));

    Ok(())
}

/// Construct the InstanceNet for the builder VM (always uses IP offset 2).
fn builder_instance_net(tenant_net: &TenantNet) -> InstanceNet {
    let ip_offset = BUILDER_IP_OFFSET;
    let base_ip = &tenant_net.ipv4_subnet;

    // Parse base IP from subnet (e.g., "10.240.3.0/24" -> "10.240.3")
    let ip_parts: Vec<&str> = base_ip
        .split('/')
        .next()
        .unwrap_or("10.240.0.0")
        .split('.')
        .collect();
    let prefix = format!("{}.{}.{}", ip_parts[0], ip_parts[1], ip_parts[2]);

    let cidr_str = base_ip.split('/').nth(1).unwrap_or("24");
    let cidr: u8 = cidr_str.parse().unwrap_or(24);

    InstanceNet {
        tap_dev: naming::tap_name(tenant_net.tenant_net_id, ip_offset),
        mac: naming::mac_address(tenant_net.tenant_net_id, ip_offset),
        guest_ip: format!("{}.{}", prefix, ip_offset),
        gateway_ip: tenant_net.gateway_ip.clone(),
        cidr,
    }
}

/// Ensure the builder kernel and rootfs exist. Downloads on first use.
fn ensure_builder_artifacts() -> Result<()> {
    let kernel_path = format!("{}/vmlinux", BUILDER_DIR);
    let rootfs_path = format!("{}/rootfs.ext4", BUILDER_DIR);

    let exists = shell::run_in_vm_stdout(&format!(
        "test -f {} && test -f {} && echo yes || echo no",
        kernel_path, rootfs_path
    ))?;

    if exists.trim() == "yes" {
        ui::info("Builder artifacts found.");
        return Ok(());
    }

    ui::info("Downloading builder artifacts (first time only)...");
    shell::run_in_vm(&format!("mkdir -p {}", BUILDER_DIR))?;

    // Download kernel from Firecracker CI S3 bucket (same as dev mode)
    shell::run_in_vm_visible(&format!(
        r#"
        set -euo pipefail
        cd {dir}

        if [ ! -f vmlinux ]; then
            echo '[mvm] Downloading builder kernel...'
            latest_kernel_key=$(wget -q \
                "http://spec.ccfc.min.s3.amazonaws.com/?prefix=firecracker-ci/v1.13/{arch}/vmlinux-5.10&list-type=2" \
                -O - | grep -oP '(?<=<Key>)(firecracker-ci/v1.13/{arch}/vmlinux-5\.10\.[0-9]{{3}})(?=</Key>)')
            wget -q --show-progress -O vmlinux \
                "https://s3.amazonaws.com/spec.ccfc.min/$latest_kernel_key"
        fi

        if [ ! -f rootfs.ext4 ]; then
            echo '[mvm] Downloading builder rootfs...'
            latest_ubuntu_key=$(curl -s \
                "http://spec.ccfc.min.s3.amazonaws.com/?prefix=firecracker-ci/v1.13/{arch}/ubuntu-&list-type=2" \
                | grep -oP '(?<=<Key>)(firecracker-ci/v1.13/{arch}/ubuntu-[0-9]+\.[0-9]+\.squashfs)(?=</Key>)' \
                | sort -V | tail -1)

            wget -q --show-progress -O rootfs.squashfs \
                "https://s3.amazonaws.com/spec.ccfc.min/$latest_ubuntu_key"

            echo '[mvm] Preparing builder rootfs...'
            sudo unsquashfs -d squashfs-root rootfs.squashfs

            # Set up builder rootfs with SSH access and Nix-ready layout
            sudo mkdir -p squashfs-root/root/.ssh
            sudo mkdir -p squashfs-root/nix

            truncate -s 4G rootfs.ext4
            sudo mkfs.ext4 -d squashfs-root -F rootfs.ext4

            sudo rm -rf squashfs-root rootfs.squashfs
            echo '[mvm] Builder rootfs prepared.'
        fi
        "#,
        dir = BUILDER_DIR,
        arch = ARCH,
    ))?;

    ui::success("Builder artifacts ready.");
    Ok(())
}

/// Boot an ephemeral Firecracker builder VM.
/// Returns the FC process PID.
fn boot_builder(run_dir: &str, builder_net: &InstanceNet, tenant_net: &TenantNet) -> Result<u32> {
    ui::info("Booting builder VM...");

    // Set up TAP device for builder
    net::setup_tap(builder_net, &tenant_net.bridge_name)?;

    // Generate FC config for builder
    let fc_config = FcConfig {
        boot_source: BootSource {
            kernel_image_path: format!("{}/vmlinux", BUILDER_DIR),
            boot_args: format!(
                "keep_bootcon console=ttyS0 reboot=k panic=1 pci=off \
                 ip={}::{}:255.255.255.0::eth0:off",
                builder_net.guest_ip, builder_net.gateway_ip,
            ),
        },
        drives: vec![Drive {
            drive_id: "rootfs".to_string(),
            path_on_host: format!("{}/rootfs.ext4", BUILDER_DIR),
            is_root_device: true,
            is_read_only: false,
        }],
        network_interfaces: vec![NetworkInterface {
            iface_id: "net1".to_string(),
            guest_mac: builder_net.mac.clone(),
            host_dev_name: builder_net.tap_dev.clone(),
        }],
        machine_config: MachineConfig {
            vcpu_count: BUILDER_VCPUS,
            mem_size_mib: BUILDER_MEM_MIB,
        },
        vsock: None, // Ephemeral build VMs don't need vsock
    };

    let config_json = serde_json::to_string_pretty(&fc_config)?;
    let config_path = format!("{}/fc-builder.json", run_dir);
    let socket_path = format!("{}/firecracker.socket", run_dir);
    let log_path = format!("{}/firecracker.log", run_dir);
    let pid_path = format!("{}/fc.pid", run_dir);

    // Write FC config
    shell::run_in_vm(&format!(
        "cat > {} << 'MVMEOF'\n{}\nMVMEOF",
        config_path, config_json
    ))?;

    // Launch Firecracker in background
    shell::run_in_vm(&format!(
        r#"
        rm -f {socket}
        firecracker \
            --api-sock {socket} \
            --config-file {config} \
            --log-path {log} \
            --level Info \
            &
        FC_PID=$!
        echo $FC_PID > {pid}
        "#,
        socket = socket_path,
        config = config_path,
        log = log_path,
        pid = pid_path,
    ))?;

    // Read the PID
    let pid_str = shell::run_in_vm_stdout(&format!("cat {}", pid_path))?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("Failed to parse builder PID: {:?}", pid_str))?;

    ui::info(&format!("Builder VM started (PID: {})", pid));

    // Wait for builder VM to be SSH-accessible
    ui::info("Waiting for builder VM to become ready...");
    shell::run_in_vm(&format!(
        r#"
        for i in $(seq 1 60); do
            if ssh -o StrictHostKeyChecking=no -o ConnectTimeout=2 \
                   -o BatchMode=yes -i /dev/null \
                   root@{ip} true 2>/dev/null; then
                echo "Builder ready after ${{i}}s"
                exit 0
            fi
            sleep 1
        done
        echo "Builder VM did not become ready in 60s" >&2
        exit 1
        "#,
        ip = builder_net.guest_ip,
    ))?;

    Ok(pid)
}

/// Construct the nix build attribute for a pool.
///
/// If `<flake_ref>/mvm-profiles.toml` exists (checked inside the builder VM),
/// uses `tenant-<role>-<profile>`. Otherwise falls back to legacy `tenant-<profile>`.
fn resolve_build_attribute(
    builder_ip: &str,
    ssh_key_path: &str,
    flake_ref: &str,
    role: &super::config::Role,
    profile: &str,
) -> String {
    let system = if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    };

    // Try to read mvm-profiles.toml from inside the builder VM
    let manifest_check = shell::run_in_vm_stdout(&format!(
        "ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
            -i {key} root@{ip} \
            'cat {flake}/mvm-profiles.toml 2>/dev/null || echo __NOT_FOUND__'",
        key = ssh_key_path,
        ip = builder_ip,
        flake = flake_ref,
    ));

    if let Ok(content) = manifest_check
        && !content.contains("__NOT_FOUND__")
        && let Ok(manifest) = super::nix_manifest::NixManifest::from_toml(&content)
        && manifest.resolve(role, profile).is_ok()
    {
        let attr = format!(
            "{}#packages.{}.tenant-{}-{}",
            flake_ref, system, role, profile
        );
        ui::info(&format!(
            "Manifest found, using role-aware attribute: {}",
            attr
        ));
        return attr;
    }

    // Fallback: legacy attribute without role
    let attr = format!("{}#packages.{}.tenant-{}", flake_ref, system, profile);
    ui::info(&format!(
        "No manifest found, using legacy attribute: {}",
        attr
    ));
    attr
}

/// Execute `nix build` inside the builder VM via SSH.
/// Returns the Nix output path on success.
fn run_nix_build(
    builder_ip: &str,
    ssh_key_path: &str,
    flake_ref: &str,
    role: &super::config::Role,
    profile: &str,
    timeout_secs: u64,
) -> Result<String> {
    let build_attr = resolve_build_attribute(builder_ip, ssh_key_path, flake_ref, role, profile);

    ui::info(&format!("Running: nix build {}", build_attr));

    // Run the build with timeout
    let output = shell::run_in_vm_stdout(&format!(
        r#"
        ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
            -i {key} root@{ip} \
            'timeout {timeout} nix build {attr} --no-link --print-out-paths 2>&1'
        "#,
        key = ssh_key_path,
        ip = builder_ip,
        timeout = timeout_secs,
        attr = build_attr,
    ))
    .with_context(|| format!("nix build failed for {}", build_attr))?;

    // The last non-empty line should be the output path
    let out_path = output
        .lines()
        .rev()
        .find(|l| l.starts_with("/nix/store/"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "nix build did not produce an output path. Output:\n{}",
                output
            )
        })?
        .to_string();

    ui::info(&format!("Build output: {}", out_path));
    Ok(out_path)
}

/// Extract build artifacts from the builder VM to the pool's revisions directory.
/// Returns the revision hash.
fn extract_artifacts(
    builder_ip: &str,
    ssh_key_path: &str,
    nix_output_path: &str,
    tenant_id: &str,
    pool_id: &str,
) -> Result<String> {
    // Compute revision hash from the Nix store path
    // Nix store paths look like: /nix/store/<hash>-<name>
    let revision_hash = nix_output_path
        .strip_prefix("/nix/store/")
        .and_then(|s| s.split('-').next())
        .unwrap_or("unknown")
        .to_string();

    let artifacts_dir = pool_artifacts_dir(tenant_id, pool_id);
    let rev_dir = format!("{}/revisions/{}", artifacts_dir, revision_hash);

    shell::run_in_vm(&format!("mkdir -p {}", rev_dir))?;

    // Copy kernel and rootfs from builder VM
    shell::run_in_vm_visible(&format!(
        r#"
        set -euo pipefail

        # List what the build produced
        CONTENTS=$(ssh -o StrictHostKeyChecking=no -i {key} root@{ip} \
            'ls -la {out_path}/ 2>/dev/null || echo "single-output"')
        echo "Build contents: $CONTENTS"

        # Copy kernel (try common output names)
        scp -o StrictHostKeyChecking=no -i {key} \
            root@{ip}:'{out_path}/kernel' {rev_dir}/vmlinux 2>/dev/null || \
        scp -o StrictHostKeyChecking=no -i {key} \
            root@{ip}:'{out_path}/vmlinux' {rev_dir}/vmlinux 2>/dev/null || \
            {{ echo 'ERROR: kernel not found in build output' >&2; exit 1; }}

        # Copy rootfs (try common output names)
        scp -o StrictHostKeyChecking=no -i {key} \
            root@{ip}:'{out_path}/rootfs' {rev_dir}/rootfs.ext4 2>/dev/null || \
        scp -o StrictHostKeyChecking=no -i {key} \
            root@{ip}:'{out_path}/rootfs.ext4' {rev_dir}/rootfs.ext4 2>/dev/null || \
            {{ echo 'ERROR: rootfs not found in build output' >&2; exit 1; }}

        # Generate base FC config placeholder
        cat > {rev_dir}/fc-base.json << 'FCCFGEOF'
        {{
            "note": "Base config from build. Overridden at instance start."
        }}
FCCFGEOF

        echo "Artifacts stored at {rev_dir}"
        ls -lh {rev_dir}/
        "#,
        key = ssh_key_path,
        ip = builder_ip,
        out_path = nix_output_path,
        rev_dir = rev_dir,
    ))?;

    Ok(revision_hash)
}

/// Tear down the builder VM: kill FC process, remove TAP device, clean up run dir.
fn teardown_builder(pid: u32, builder_net: &InstanceNet, run_dir: &str) -> Result<()> {
    ui::info("Tearing down builder VM...");

    // Kill Firecracker process
    let _ = shell::run_in_vm(&format!(
        "kill {} 2>/dev/null || true; sleep 1; kill -9 {} 2>/dev/null || true",
        pid, pid
    ));

    // Remove TAP device
    let _ = net::teardown_tap(&builder_net.tap_dev);

    // Clean up run directory
    let _ = shell::run_in_vm(&format!("rm -rf {}", run_dir));

    Ok(())
}

/// Append a build revision to the pool's build history.
fn record_build_history(tenant_id: &str, pool_id: &str, revision: &BuildRevision) -> Result<()> {
    let history_path = format!("{}/build_history.json", pool_dir(tenant_id, pool_id));
    let json_entry = serde_json::to_string(revision)?;

    // Append to history file (create if missing, keep last 50 entries)
    shell::run_in_vm(&format!(
        r#"
        if [ -f {path} ]; then
            EXISTING=$(cat {path})
            echo "$EXISTING" | head -49 > {path}.tmp
            echo '{entry}' >> {path}.tmp
            mv {path}.tmp {path}
        else
            echo '{entry}' > {path}
        fi
        "#,
        path = history_path,
        entry = json_entry,
    ))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::tenant::config::TenantNet;

    #[test]
    fn test_builder_instance_net() {
        let tenant_net = TenantNet::new(3, "10.240.3.0/24", "10.240.3.1");
        let net = builder_instance_net(&tenant_net);

        assert_eq!(net.guest_ip, "10.240.3.2");
        assert_eq!(net.gateway_ip, "10.240.3.1");
        assert_eq!(net.tap_dev, "tn3i2");
        assert_eq!(net.cidr, 24);
        assert!(net.mac.starts_with("02:fc:"));
    }

    #[test]
    fn test_builder_instance_net_different_subnet() {
        let tenant_net = TenantNet::new(200, "10.240.200.0/24", "10.240.200.1");
        let net = builder_instance_net(&tenant_net);

        assert_eq!(net.guest_ip, "10.240.200.2");
        assert_eq!(net.gateway_ip, "10.240.200.1");
        assert_eq!(net.tap_dev, "tn200i2");
    }

    #[test]
    fn test_builder_constants() {
        assert_eq!(BUILDER_IP_OFFSET, 2);
        assert_eq!(BUILDER_VCPUS, 4);
        assert_eq!(BUILDER_MEM_MIB, 4096);
        assert_eq!(DEFAULT_TIMEOUT_SECS, 1800);
    }
}
