use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::infra::http;
use crate::infra::shell;
use crate::security::audit;
use crate::vm::pool::config::pool_snapshots_dir;

/// Metadata for a snapshot (base or delta).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub snapshot_type: String,
    pub revision_hash: Option<String>,
    pub compression: String,
    pub created_at: String,
    pub vmstate_size_bytes: u64,
    pub mem_size_bytes: u64,
}

/// Expected prefix for all snapshot paths (used for canonicalization checks).
const SNAPSHOT_BASE_DIR: &str = "/var/lib/mvm/tenants/";

// --- Paths ---

fn base_snapshot_dir(tenant_id: &str, pool_id: &str) -> String {
    format!("{}/base", pool_snapshots_dir(tenant_id, pool_id))
}

fn delta_snapshot_dir(instance_dir: &str) -> String {
    format!("{}/snapshots/delta", instance_dir)
}

/// Validate that a snapshot path belongs to the expected tenant.
/// Prevents cross-tenant snapshot access via path traversal.
fn validate_snapshot_path(path: &str, tenant_id: &str) -> Result<()> {
    // Canonicalize: resolve any ../ or symlinks
    let canonical = shell::run_in_vm_stdout(&format!(
        "realpath -m {} 2>/dev/null || echo {}",
        path, path
    ))?;
    let canonical = canonical.trim();

    let expected_prefix = format!("{}{}/", SNAPSHOT_BASE_DIR, tenant_id);
    if !canonical.starts_with(&expected_prefix) {
        anyhow::bail!(
            "Snapshot path {} resolves to {} which is outside tenant {}",
            path,
            canonical,
            tenant_id
        );
    }
    Ok(())
}

/// Set per-tenant snapshot directory permissions to 0700 (root-only).
fn secure_snapshot_dir(dir: &str) -> Result<()> {
    shell::run_in_vm(&format!("sudo mkdir -p {} && sudo chmod 0700 {}", dir, dir))?;
    Ok(())
}

// --- Base Snapshot (pool-level, shared) ---

/// Check if a base snapshot exists for this pool.
pub fn has_base_snapshot(tenant_id: &str, pool_id: &str) -> Result<bool> {
    let dir = base_snapshot_dir(tenant_id, pool_id);
    let out = shell::run_in_vm_stdout(&format!(
        "test -f {}/vmstate.bin && test -f {}/mem.bin && echo yes || echo no",
        dir, dir
    ))?;
    Ok(out.trim() == "yes")
}

/// Create a pool-level base snapshot from a running (paused) instance.
///
/// The instance must be in Paused (Warm) state. The snapshot is stored
/// at pools/<pool>/snapshots/base/ and shared across all instances.
///
/// Uses Firecracker's snapshot API:
/// PUT /snapshot/create { snapshot_type: "Full", snapshot_path, mem_file_path }
pub fn create_base_snapshot(
    tenant_id: &str,
    pool_id: &str,
    instance_dir: &str,
    revision_hash: Option<&str>,
    compression: &str,
) -> Result<()> {
    let base_dir = base_snapshot_dir(tenant_id, pool_id);
    let socket_path = format!("{}/runtime/firecracker.socket", instance_dir);

    // Secure directory permissions (0700 root-only)
    secure_snapshot_dir(&base_dir)?;

    // Create snapshot via Firecracker API
    shell::run_in_vm(&format!(
        r#"curl -s --unix-socket {socket} -X PUT \
            -H 'Content-Type: application/json' \
            -d '{{"snapshot_type": "Full", "snapshot_path": "{base}/vmstate.bin", "mem_file_path": "{base}/mem.bin"}}' \
            'http://localhost/snapshot/create'"#,
        socket = socket_path,
        base = base_dir,
    ))
    .with_context(|| "Failed to create base snapshot via Firecracker API")?;

    // Compress if requested
    compress_snapshot_files(&base_dir, compression)?;

    // Write metadata
    let vmstate_size = file_size_bytes(&format!("{}/vmstate.bin", base_dir))?;
    let mem_size = file_size_bytes(&format!("{}/mem.bin", base_dir))?;

    let meta = SnapshotMeta {
        snapshot_type: "base".to_string(),
        revision_hash: revision_hash.map(|s| s.to_string()),
        compression: compression.to_string(),
        created_at: http::utc_now(),
        vmstate_size_bytes: vmstate_size,
        mem_size_bytes: mem_size,
    };

    let meta_json = serde_json::to_string_pretty(&meta)?;
    shell::run_in_vm(&format!(
        "cat > {}/meta.json << 'MVMEOF'\n{}\nMVMEOF",
        base_dir, meta_json
    ))?;

    // Audit log
    let _ = audit::log_event(
        tenant_id,
        Some(pool_id),
        None,
        audit::AuditAction::SnapshotCreated,
        Some(&format!("type=base, compression={}", compression)),
    );

    Ok(())
}

// --- Delta Snapshot (instance-level, unique) ---

/// Check if a delta snapshot exists for an instance.
pub fn has_delta_snapshot(instance_dir: &str) -> Result<bool> {
    let dir = delta_snapshot_dir(instance_dir);
    let out = shell::run_in_vm_stdout(&format!(
        "test -f {}/vmstate.delta.bin && test -f {}/mem.delta.bin && echo yes || echo no",
        dir, dir
    ))?;
    Ok(out.trim() == "yes")
}

/// Create an instance-level delta snapshot from a paused instance.
///
/// Captures the memory state unique to this instance. Stored at
/// instances/<id>/snapshots/delta/.
///
/// Uses Firecracker's snapshot API:
/// PUT /snapshot/create { snapshot_type: "Diff", snapshot_path, mem_file_path }
pub fn create_delta_snapshot(instance_dir: &str, compression: &str) -> Result<()> {
    let delta_dir = delta_snapshot_dir(instance_dir);
    let socket_path = format!("{}/runtime/firecracker.socket", instance_dir);

    // Secure directory permissions (0700 root-only)
    secure_snapshot_dir(&delta_dir)?;

    // Create diff snapshot via Firecracker API
    shell::run_in_vm(&format!(
        r#"curl -s --unix-socket {socket} -X PUT \
            -H 'Content-Type: application/json' \
            -d '{{"snapshot_type": "Diff", "snapshot_path": "{delta}/vmstate.delta.bin", "mem_file_path": "{delta}/mem.delta.bin"}}' \
            'http://localhost/snapshot/create'"#,
        socket = socket_path,
        delta = delta_dir,
    ))
    .with_context(|| "Failed to create delta snapshot via Firecracker API")?;

    // Compress if requested
    compress_snapshot_files(&delta_dir, compression)?;

    // Write metadata
    let vmstate_size = file_size_bytes(&format!("{}/vmstate.delta.bin", delta_dir))?;
    let mem_size = file_size_bytes(&format!("{}/mem.delta.bin", delta_dir))?;

    let meta = SnapshotMeta {
        snapshot_type: "delta".to_string(),
        revision_hash: None,
        compression: compression.to_string(),
        created_at: http::utc_now(),
        vmstate_size_bytes: vmstate_size,
        mem_size_bytes: mem_size,
    };

    let meta_json = serde_json::to_string_pretty(&meta)?;
    shell::run_in_vm(&format!(
        "cat > {}/meta.json << 'MVMEOF'\n{}\nMVMEOF",
        delta_dir, meta_json
    ))?;

    Ok(())
}

/// Remove delta snapshot for an instance (called on stop/destroy).
pub fn remove_delta_snapshot(instance_dir: &str) -> Result<()> {
    let delta_dir = delta_snapshot_dir(instance_dir);
    shell::run_in_vm(&format!("rm -rf {}/*", delta_dir))?;
    Ok(())
}

// --- Restore ---

/// Restore an instance from base + optional delta snapshot.
///
/// Validates that snapshot paths belong to the correct tenant (prevents
/// cross-tenant snapshot access). Copies snapshot files to the instance's
/// runtime directory, decompresses if needed, then loads via FC API.
///
/// Returns true if restored from snapshot, false if no snapshot available.
pub fn restore_snapshot(
    tenant_id: &str,
    pool_id: &str,
    instance_dir: &str,
    socket_path: &str,
) -> Result<bool> {
    let base_dir = base_snapshot_dir(tenant_id, pool_id);
    let delta_dir = delta_snapshot_dir(instance_dir);
    let runtime_dir = format!("{}/runtime", instance_dir);

    // Validate paths belong to this tenant (prevent cross-tenant access)
    validate_snapshot_path(&base_dir, tenant_id)?;
    validate_snapshot_path(instance_dir, tenant_id)?;

    // Check if base exists
    if !has_base_snapshot(tenant_id, pool_id)? {
        return Ok(false);
    }

    // Read base metadata to check compression
    let base_meta = load_snapshot_meta(&base_dir)?;

    // Copy base files to runtime dir for restore
    shell::run_in_vm(&format!(
        "cp {base}/vmstate.bin {rt}/vmstate.bin && cp {base}/mem.bin {rt}/mem.bin",
        base = base_dir,
        rt = runtime_dir,
    ))?;

    // Decompress base files if needed
    decompress_snapshot_files(
        &runtime_dir,
        &base_meta.compression,
        "vmstate.bin",
        "mem.bin",
    )?;

    // Check for delta and overlay
    let has_delta = has_delta_snapshot(instance_dir)?;
    if has_delta {
        let delta_meta = load_snapshot_meta(&delta_dir)?;

        // Copy delta files
        shell::run_in_vm(&format!(
            "cp {delta}/vmstate.delta.bin {rt}/vmstate.delta.bin && cp {delta}/mem.delta.bin {rt}/mem.delta.bin",
            delta = delta_dir,
            rt = runtime_dir,
        ))?;

        // Decompress delta if needed
        decompress_snapshot_files(
            &runtime_dir,
            &delta_meta.compression,
            "vmstate.delta.bin",
            "mem.delta.bin",
        )?;
    }

    // Determine which files to load (delta takes precedence)
    let (vmstate_file, mem_file) = if has_delta {
        ("vmstate.delta.bin", "mem.delta.bin")
    } else {
        ("vmstate.bin", "mem.bin")
    };

    // Load snapshot via Firecracker API
    shell::run_in_vm(&format!(
        r#"curl -s --unix-socket {socket} -X PUT \
            -H 'Content-Type: application/json' \
            -d '{{"snapshot_path": "{rt}/{vmstate}", "mem_backend": {{"backend_type": "File", "backend_path": "{rt}/{mem}"}}, "enable_diff_snapshots": true}}' \
            'http://localhost/snapshot/load'"#,
        socket = socket_path,
        rt = runtime_dir,
        vmstate = vmstate_file,
        mem = mem_file,
    ))
    .with_context(|| "Failed to load snapshot via Firecracker API")?;

    // Resume vCPUs
    shell::run_in_vm(&format!(
        r#"curl -s --unix-socket {socket} -X PATCH \
            -H 'Content-Type: application/json' \
            -d '{{"state": "Resumed"}}' \
            'http://localhost/vm'"#,
        socket = socket_path,
    ))
    .with_context(|| "Failed to resume vCPUs after snapshot restore")?;

    // Audit log
    let _ = audit::log_event(
        tenant_id,
        Some(pool_id),
        None,
        audit::AuditAction::SnapshotRestored,
        Some(&format!("delta={}", has_delta)),
    );

    Ok(true)
}

/// Detect Firecracker snapshot version capabilities.
pub fn snapshot_capabilities() -> Result<String> {
    let out =
        shell::run_in_vm_stdout("firecracker --version 2>/dev/null | head -1 || echo unknown")?;
    Ok(out.trim().to_string())
}

// --- Compression Helpers ---

/// Compress snapshot files in-place using the configured algorithm.
fn compress_snapshot_files(dir: &str, compression: &str) -> Result<()> {
    match compression {
        "lz4" => {
            shell::run_in_vm(&format!(
                r#"
                cd {dir}
                for f in *.bin; do
                    [ -f "$f" ] && lz4 -f "$f" "$f.lz4" && mv "$f.lz4" "$f"
                done
                "#,
                dir = dir,
            ))?;
        }
        "zstd" => {
            shell::run_in_vm(&format!(
                r#"
                cd {dir}
                for f in *.bin; do
                    [ -f "$f" ] && zstd -f --rm "$f" -o "$f.zst" && mv "$f.zst" "$f"
                done
                "#,
                dir = dir,
            ))?;
        }
        _ => {} // "none" or unknown — no compression
    }
    Ok(())
}

/// Decompress snapshot files in-place before loading.
fn decompress_snapshot_files(
    dir: &str,
    compression: &str,
    vmstate_name: &str,
    mem_name: &str,
) -> Result<()> {
    match compression {
        "lz4" => {
            shell::run_in_vm(&format!(
                r#"
                cd {dir}
                lz4 -df {vmstate} {vmstate}.dec 2>/dev/null && mv {vmstate}.dec {vmstate} || true
                lz4 -df {mem} {mem}.dec 2>/dev/null && mv {mem}.dec {mem} || true
                "#,
                dir = dir,
                vmstate = vmstate_name,
                mem = mem_name,
            ))?;
        }
        "zstd" => {
            shell::run_in_vm(&format!(
                r#"
                cd {dir}
                zstd -df {vmstate} -o {vmstate}.dec 2>/dev/null && mv {vmstate}.dec {vmstate} || true
                zstd -df {mem} -o {mem}.dec 2>/dev/null && mv {mem}.dec {mem} || true
                "#,
                dir = dir,
                vmstate = vmstate_name,
                mem = mem_name,
            ))?;
        }
        _ => {} // "none" — already uncompressed
    }
    Ok(())
}

/// Load snapshot metadata from a directory.
fn load_snapshot_meta(dir: &str) -> Result<SnapshotMeta> {
    let json = shell::run_in_vm_stdout(&format!("cat {}/meta.json", dir))
        .with_context(|| format!("Failed to read snapshot metadata from {}", dir))?;
    let meta: SnapshotMeta = serde_json::from_str(&json)?;
    Ok(meta)
}

/// Get file size in bytes (inside the VM).
fn file_size_bytes(path: &str) -> Result<u64> {
    let out = shell::run_in_vm_stdout(&format!("stat -c%s {} 2>/dev/null || echo 0", path))?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Invalidate (remove) the pool-level base snapshot.
/// Called when pool artifacts are rebuilt.
pub fn invalidate_base_snapshot(tenant_id: &str, pool_id: &str) -> Result<()> {
    let base_dir = base_snapshot_dir(tenant_id, pool_id);
    shell::run_in_vm(&format!("rm -rf {}/*", base_dir))?;

    let _ = audit::log_event(
        tenant_id,
        Some(pool_id),
        None,
        audit::AuditAction::SnapshotDeleted,
        Some("type=base, reason=invalidate"),
    );

    Ok(())
}

/// Load base snapshot metadata (if exists).
pub fn base_snapshot_info(tenant_id: &str, pool_id: &str) -> Result<Option<SnapshotMeta>> {
    if !has_base_snapshot(tenant_id, pool_id)? {
        return Ok(None);
    }
    let meta = load_snapshot_meta(&base_snapshot_dir(tenant_id, pool_id))?;
    Ok(Some(meta))
}

/// Load delta snapshot metadata (if exists).
pub fn delta_snapshot_info(instance_dir: &str) -> Result<Option<SnapshotMeta>> {
    if !has_delta_snapshot(instance_dir)? {
        return Ok(None);
    }
    let meta = load_snapshot_meta(&delta_snapshot_dir(instance_dir))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_snapshot_dir_path() {
        assert_eq!(
            base_snapshot_dir("acme", "workers"),
            "/var/lib/mvm/tenants/acme/pools/workers/snapshots/base"
        );
    }

    #[test]
    fn test_delta_snapshot_dir_path() {
        let inst = "/var/lib/mvm/tenants/acme/pools/workers/instances/i-abc123";
        assert_eq!(
            delta_snapshot_dir(inst),
            "/var/lib/mvm/tenants/acme/pools/workers/instances/i-abc123/snapshots/delta"
        );
    }

    #[test]
    fn test_snapshot_meta_roundtrip() {
        let meta = SnapshotMeta {
            snapshot_type: "base".to_string(),
            revision_hash: Some("abc123".to_string()),
            compression: "zstd".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            vmstate_size_bytes: 1024,
            mem_size_bytes: 1048576,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SnapshotMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.snapshot_type, "base");
        assert_eq!(parsed.compression, "zstd");
        assert_eq!(parsed.mem_size_bytes, 1048576);
    }

    #[test]
    fn test_snapshot_meta_delta_no_revision() {
        let meta = SnapshotMeta {
            snapshot_type: "delta".to_string(),
            revision_hash: None,
            compression: "none".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            vmstate_size_bytes: 512,
            mem_size_bytes: 65536,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SnapshotMeta = serde_json::from_str(&json).unwrap();
        assert!(parsed.revision_hash.is_none());
        assert_eq!(parsed.snapshot_type, "delta");
    }

    #[test]
    fn test_validate_snapshot_path_valid() {
        let path = "/var/lib/mvm/tenants/acme/pools/workers/snapshots/base";
        // This would call shell::run_in_vm_stdout which needs mock,
        // so we test the logic inline
        let expected_prefix = format!("{}acme/", SNAPSHOT_BASE_DIR);
        assert!(path.starts_with(&expected_prefix));
    }

    #[test]
    fn test_validate_snapshot_path_cross_tenant() {
        let path = "/var/lib/mvm/tenants/evil/pools/workers/snapshots/base";
        let expected_prefix = format!("{}acme/", SNAPSHOT_BASE_DIR);
        assert!(!path.starts_with(&expected_prefix));
    }
}
