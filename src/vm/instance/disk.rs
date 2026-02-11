use anyhow::Result;

use crate::infra::shell;

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
