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

/// Create a fresh secrets disk from the tenant's secrets.json (recreated each run).
pub fn create_secrets_disk(instance_dir: &str, secrets_json_path: &str) -> Result<String> {
    let path = format!("{}/volumes/secrets.ext4", instance_dir);
    shell::run_in_vm(&format!(
        r#"
        mkdir -p {dir}/volumes
        truncate -s 16M {path}
        mkfs.ext4 -q {path}
        MOUNT_DIR=$(mktemp -d)
        sudo mount {path} $MOUNT_DIR
        sudo cp {secrets} $MOUNT_DIR/secrets.json 2>/dev/null || true
        sudo umount $MOUNT_DIR
        rmdir $MOUNT_DIR
        "#,
        dir = instance_dir,
        path = path,
        secrets = secrets_json_path,
    ))?;
    Ok(path)
}
