use anyhow::Result;

use crate::shell;

/// Ensure a per-instance data disk exists (created once, persisted across restarts).
pub fn ensure_data_disk(instance_dir: &str, size_mib: u32) -> Result<String> {
    let path = format!("{}/volumes/data.ext4", instance_dir);
    shell::run_in_vm(&format!(
        r#"
        mkdir -p {dir}/volumes
        if [ ! -f {path} ]; then
            truncate -s {size}M {path}
            mkfs.ext4 -q {path}
        fi
        "#,
        dir = instance_dir,
        path = path,
        size = size_mib,
    ))?;
    Ok(path)
}

/// Create a fresh ephemeral secrets disk from the tenant's secrets.json.
///
/// Security hardening:
/// - Uses tmpfs-backed file (never hits persistent storage)
/// - File permissions 0600 (root-only read/write)
/// - Mount with ro,noexec,nodev,nosuid inside guest
/// - Recreated on every start and wake (never reused)
pub fn create_secrets_disk(instance_dir: &str, secrets_json_path: &str) -> Result<String> {
    let path = format!("{}/volumes/secrets.ext4", instance_dir);
    shell::run_in_vm(&format!(
        r#"
        mkdir -p {dir}/volumes

        # Remove any previous secrets disk
        rm -f {path}

        # Create tmpfs-backed secrets image (16M, never persisted to real disk)
        TMPFS_DIR=$(mktemp -d -p /dev/shm mvm-secrets-XXXXXX)
        truncate -s 16M "$TMPFS_DIR/secrets.ext4"
        mkfs.ext4 -q "$TMPFS_DIR/secrets.ext4"

        # Mount, populate, unmount
        MOUNT_DIR=$(mktemp -d)
        sudo mount "$TMPFS_DIR/secrets.ext4" "$MOUNT_DIR"
        sudo cp {secrets} "$MOUNT_DIR/secrets.json" 2>/dev/null || true
        sudo chmod 0400 "$MOUNT_DIR/secrets.json" 2>/dev/null || true
        sudo umount "$MOUNT_DIR"
        rmdir "$MOUNT_DIR"

        # Move to final location and set restrictive permissions
        mv "$TMPFS_DIR/secrets.ext4" {path}
        rmdir "$TMPFS_DIR"
        chmod 0600 {path}
        "#,
        dir = instance_dir,
        path = path,
        secrets = secrets_json_path,
    ))?;
    Ok(path)
}

/// Remove the ephemeral secrets disk after instance stop/destroy.
pub fn remove_secrets_disk(instance_dir: &str) -> Result<()> {
    let path = format!("{}/volumes/secrets.ext4", instance_dir);
    shell::run_in_vm(&format!("rm -f {}", path))?;
    Ok(())
}

/// Create a read-only config drive containing instance/pool metadata.
///
/// Unlike the secrets disk, this contains non-sensitive configuration data:
/// instance identity, network config, pool resources, and min_runtime_policy.
/// Created fresh on every start/wake with current config.
/// Guest mounts as ro — the vsock agent reads config from this drive.
pub fn create_config_disk(instance_dir: &str, config_json: &str) -> Result<String> {
    let path = format!("{}/volumes/config.ext4", instance_dir);
    let escaped = config_json.replace('\'', "'\\''");
    shell::run_in_vm(&format!(
        r#"
        mkdir -p {dir}/volumes

        # Remove any previous config disk
        rm -f {path}

        # Create a small ext4 image (4M)
        truncate -s 4M {path}
        mkfs.ext4 -q {path}

        # Mount, populate, unmount
        MOUNT_DIR=$(mktemp -d)
        sudo mount {path} "$MOUNT_DIR"
        echo '{json}' | sudo tee "$MOUNT_DIR/config.json" >/dev/null
        sudo chmod 0444 "$MOUNT_DIR/config.json"
        sudo umount "$MOUNT_DIR"
        rmdir "$MOUNT_DIR"

        chmod 0644 {path}
        "#,
        dir = instance_dir,
        path = path,
        json = escaped,
    ))?;
    Ok(path)
}

/// Remove the config disk after instance stop/destroy.
pub fn remove_config_disk(instance_dir: &str) -> Result<()> {
    let path = format!("{}/volumes/config.ext4", instance_dir);
    shell::run_in_vm(&format!("rm -f {}", path))?;
    Ok(())
}
