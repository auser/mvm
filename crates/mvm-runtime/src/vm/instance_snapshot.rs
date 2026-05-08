//! Instance-snapshot store — A4 of the e2b parity plan.
//!
//! `mvmctl pause <vm>` quiesces the running VM, asks Firecracker
//! for a snapshot, seals the bytes with the W4 HMAC envelope (now
//! including a monotonic per-instance epoch — G5), and stops the
//! VM. `mvmctl resume <vm>` verifies the envelope, asks Firecracker
//! to load the snapshot, then re-establishes vsock auth via
//! `PostRestore`. This module owns the disk layout + seal/verify
//! helpers; the actual Firecracker quiesce/load lives behind a
//! `SnapshotIO` trait so the substrate is fully unit-testable
//! without a live KVM host.
//!
//! # On-disk layout
//!
//! ```text
//! ~/.mvm/instances/<vm-name>/
//!     snapshot/
//!         vmstate.bin       (Firecracker VM state, mode 0600)
//!         mem.bin           (guest memory image, mode 0600)
//!         integrity.json    (HMAC sidecar, mode 0600)
//!         .epoch            (monotonic counter, mode 0600)
//! ```
//!
//! The directory itself is mode `0700` (consistent with the
//! existing `~/.mvm` discipline from W1.5). All snapshot files are
//! mode `0600` so a co-tenant on the same host can't read another
//! sandbox's memory image even if `~/.mvm/instances/` were ever
//! made world-readable by mistake.
//!
//! # What this module does NOT do (yet)
//!
//! - AES-GCM encryption of `mem.bin` (decision 2 / Sprint plan).
//!   The HMAC envelope guarantees integrity; confidentiality
//!   currently rests on the file mode + `~/.mvm` directory perms.
//!   The natural seam to add it is in `seal_instance_snapshot` /
//!   `verify_instance_snapshot` so callers don't change.
//! - Firecracker's actual `create_snapshot` / `load_snapshot` API
//!   calls. Those land in a follow-up chunk gated on a live KVM
//!   host; the `SnapshotIO` trait below is the seam.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use mvm_security::snapshot_hmac::{
    EpochStore, IntegritySidecar, MEM_FILENAME, SIDECAR_FILENAME, SnapshotFiles, VMSTATE_FILENAME,
    VerifyError, files_in, load_or_init_key, seal, verify,
};

/// Filename of the persistent epoch counter inside an
/// instance-snapshot dir. Hidden by default (`.epoch`) so a casual
/// `ls` doesn't show it next to the bin files.
pub const EPOCH_FILENAME: &str = ".epoch";

/// Returns the `~/.mvm/instances/<vm-name>/` directory. Doesn't
/// create it — callers that need to write into it use
/// `prepare_instance_snapshot_dir` instead.
pub fn instance_dir(vm_name: &str) -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_data_dir())
        .join("instances")
        .join(vm_name)
}

/// Returns `~/.mvm/instances/<vm-name>/snapshot/`.
pub fn snapshot_dir(vm_name: &str) -> PathBuf {
    instance_dir(vm_name).join("snapshot")
}

/// Create `<instance>/snapshot/` with mode 0700 if it doesn't
/// already exist. Returns the path. Idempotent.
pub fn prepare_instance_snapshot_dir(vm_name: &str) -> Result<PathBuf> {
    let dir = snapshot_dir(vm_name);
    ensure_dir_with_mode(&dir, 0o700)?;
    Ok(dir)
}

/// Convenience: build the canonical `SnapshotFiles` for a VM.
pub fn files_for(vm_name: &str) -> SnapshotFiles {
    files_in(&snapshot_dir(vm_name))
}

/// Trait the pause/resume orchestrator uses to talk to Firecracker.
/// Production wires `FirecrackerIO`; tests use `MemoryIO` which
/// just writes canned bytes into the snapshot files.
pub trait SnapshotIO {
    /// Quiesce the running VM, write `vmstate.bin` + `mem.bin` into
    /// `dir`, leave the VM in a paused-and-shutdown state.
    fn create_snapshot(&self, dir: &Path) -> Result<()>;

    /// Load the bytes at `<dir>/{vmstate,mem}.bin` into a fresh VM
    /// and resume vCPUs. The agent's `PostRestore` path takes care
    /// of vsock auth re-establishment after this returns.
    fn load_snapshot(&self, dir: &Path) -> Result<()>;
}

/// Pause + seal one VM's snapshot. Returns the sealed sidecar so
/// callers can record what they sealed.
///
/// 1. Ensure the snapshot dir exists (mode 0700).
/// 2. Ask the IO impl to write `vmstate.bin` + `mem.bin`.
/// 3. Tighten file modes to 0600.
/// 4. Bump the per-instance epoch counter.
/// 5. Seal the HMAC envelope with the new epoch.
pub fn pause_and_seal<IO: SnapshotIO>(vm_name: &str, io: &IO) -> Result<IntegritySidecar> {
    let dir = prepare_instance_snapshot_dir(vm_name)?;
    io.create_snapshot(&dir)
        .with_context(|| format!("Firecracker create_snapshot({})", dir.display()))?;
    tighten_snapshot_file_modes(&dir)?;

    let key_path =
        mvm_security::snapshot_hmac::default_key_path(Path::new(&mvm_core::config::mvm_data_dir()));
    let key = load_or_init_key(&key_path)
        .with_context(|| format!("loading HMAC key {}", key_path.display()))?;
    let files = files_in(&dir);
    let mvmctl_version = env!("CARGO_PKG_VERSION");

    let store = EpochStore::new(dir.join(EPOCH_FILENAME));
    let next_epoch = store
        .next()
        .with_context(|| format!("advancing epoch counter for {}", dir.display()))?;

    let sidecar = seal(&dir, &files, next_epoch, mvmctl_version, &key)
        .with_context(|| format!("sealing instance snapshot at {}", dir.display()))?;
    Ok(sidecar)
}

/// Verify + load one VM's snapshot. Honours
/// `MVM_ALLOW_STALE_SNAPSHOT=1` for both the version-mismatch and
/// the epoch-rollback branches; refuses both by default.
///
/// Returns the verified sidecar so the caller can audit it before
/// resuming Firecracker.
pub fn verify_and_resume<IO: SnapshotIO>(vm_name: &str, io: &IO) -> Result<IntegritySidecar> {
    let dir = snapshot_dir(vm_name);
    if !dir.exists() {
        bail!(
            "no instance snapshot directory at {} — pause the VM first",
            dir.display()
        );
    }
    let key_path =
        mvm_security::snapshot_hmac::default_key_path(Path::new(&mvm_core::config::mvm_data_dir()));
    let key = load_or_init_key(&key_path)
        .with_context(|| format!("loading HMAC key {}", key_path.display()))?;
    let files = files_in(&dir);
    let mvmctl_version = env!("CARGO_PKG_VERSION");
    let allow_stale = std::env::var("MVM_ALLOW_STALE_SNAPSHOT").as_deref() == Ok("1");

    let store = EpochStore::new(dir.join(EPOCH_FILENAME));
    let min_epoch = store.load();

    let sidecar = match verify(&dir, &files, min_epoch, mvmctl_version, &key, allow_stale) {
        Ok(s) => s,
        Err(e) => return Err(map_verify_error(e, &dir)),
    };

    io.load_snapshot(&dir)
        .with_context(|| format!("Firecracker load_snapshot({})", dir.display()))?;
    Ok(sidecar)
}

/// Drop the on-disk snapshot files + sidecar + epoch counter for
/// one VM. The instance directory itself stays so other state
/// (e.g. forwarded-port records) isn't disturbed. Returns `true` if
/// anything was removed.
pub fn delete_instance_snapshot(vm_name: &str) -> Result<bool> {
    let dir = snapshot_dir(vm_name);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(true)
}

/// One row of the snapshot listing. Cheap value type so callers
/// can render it however they want (table, JSON, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSnapshotEntry {
    pub vm_name: String,
    pub vmstate_size_bytes: u64,
    pub mem_size_bytes: u64,
    /// `Some(s)` when an integrity sidecar exists and parses;
    /// `None` when the snapshot is unsealed (legacy or
    /// in-progress).
    pub sidecar: Option<IntegritySidecar>,
}

/// Walk `~/.mvm/instances/*/snapshot/` and report every snapshot
/// dir we find. Errors on a single entry don't fail the whole
/// listing — a VM with a broken sidecar still surfaces with
/// `sidecar = None` so the operator can investigate.
pub fn list_instance_snapshots() -> Result<Vec<InstanceSnapshotEntry>> {
    let root = PathBuf::from(mvm_core::config::mvm_data_dir()).join("instances");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("read_dir {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let vm_name = entry.file_name().to_string_lossy().into_owned();
        let snap = entry.path().join("snapshot");
        if !snap.is_dir() {
            continue;
        }
        let vmstate_size = std::fs::metadata(snap.join(VMSTATE_FILENAME))
            .map(|m| m.len())
            .unwrap_or(0);
        let mem_size = std::fs::metadata(snap.join(MEM_FILENAME))
            .map(|m| m.len())
            .unwrap_or(0);
        let sidecar = std::fs::read(snap.join(SIDECAR_FILENAME))
            .ok()
            .and_then(|raw| serde_json::from_slice::<IntegritySidecar>(&raw).ok());
        out.push(InstanceSnapshotEntry {
            vm_name,
            vmstate_size_bytes: vmstate_size,
            mem_size_bytes: mem_size,
            sidecar,
        });
    }
    out.sort_by(|a, b| a.vm_name.cmp(&b.vm_name));
    Ok(out)
}

// ============================================================================
// Helpers
// ============================================================================

fn ensure_dir_with_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create_dir_all {}", path.display()))?;
    }
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod {:o} {}", mode, path.display()))?;
    Ok(())
}

fn tighten_snapshot_file_modes(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in [VMSTATE_FILENAME, MEM_FILENAME] {
        let p = dir.join(name);
        if !p.exists() {
            continue;
        }
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&p, perms)
            .with_context(|| format!("chmod 0600 {}", p.display()))?;
    }
    Ok(())
}

fn map_verify_error(err: VerifyError, dir: &Path) -> anyhow::Error {
    match err {
        VerifyError::SidecarMissing { .. } => anyhow::anyhow!(
            "instance snapshot at {} has no integrity sidecar — refusing to resume \
             (a fresh `mvmctl pause` would seal one)",
            dir.display()
        ),
        VerifyError::EpochRollback { got, expected } => anyhow::anyhow!(
            "instance snapshot at {} appears to be a replayed older state \
             (sealed epoch {got}, persisted high-water {expected}). Set \
             MVM_ALLOW_STALE_SNAPSHOT=1 to override.",
            dir.display()
        ),
        VerifyError::TagMismatch => anyhow::anyhow!(
            "instance snapshot at {} failed HMAC verification — files have been \
             tampered or the host key changed. Refusing to resume.",
            dir.display()
        ),
        other => anyhow::anyhow!(
            "instance snapshot at {} failed verification: {other}",
            dir.display()
        ),
    }
}

// ============================================================================
// Test-only IO impls
// ============================================================================

/// `SnapshotIO` impl that just writes canned bytes. Used by the
/// unit tests below and by integration tests that want to exercise
/// the seal/verify flow without a live Firecracker.
pub struct CannedIO {
    pub vmstate_bytes: Vec<u8>,
    pub mem_bytes: Vec<u8>,
}

impl SnapshotIO for CannedIO {
    fn create_snapshot(&self, dir: &Path) -> Result<()> {
        std::fs::write(dir.join(VMSTATE_FILENAME), &self.vmstate_bytes)?;
        std::fs::write(dir.join(MEM_FILENAME), &self.mem_bytes)?;
        Ok(())
    }
    fn load_snapshot(&self, _dir: &Path) -> Result<()> {
        Ok(())
    }
}

/// `SnapshotIO` impl that talks to a live Firecracker over its
/// Unix socket. Pause shells out to `curl`'s `PATCH /vm` (state =
/// Paused) followed by `PUT /snapshot/create`; resume runs `PUT
/// /snapshot/load` then `PATCH /vm` (state = Resumed).
///
/// The socket path is taken from the running-VM lookup at call
/// time so a stale `mvmctl pause` against a vanished VM fails
/// cleanly with `socket does not exist` rather than mid-API.
pub struct FirecrackerIO {
    /// Absolute path to the live Firecracker control socket.
    pub socket_path: PathBuf,
}

impl FirecrackerIO {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    fn ensure_socket(&self) -> Result<()> {
        if !self.socket_path.exists() {
            bail!(
                "Firecracker socket {} does not exist — VM is not running",
                self.socket_path.display()
            );
        }
        Ok(())
    }
}

impl SnapshotIO for FirecrackerIO {
    fn create_snapshot(&self, dir: &Path) -> Result<()> {
        self.ensure_socket()?;
        // Pause vCPUs first (Firecracker requires a paused VM
        // before /snapshot/create). PATCH /vm.
        run_curl(&self.socket_path, "PATCH", "/vm", r#"{"state":"Paused"}"#)
            .with_context(|| "PATCH /vm Paused")?;

        let payload = format!(
            r#"{{"snapshot_type":"Full","snapshot_path":"{}/{}","mem_file_path":"{}/{}"}}"#,
            dir.display(),
            VMSTATE_FILENAME,
            dir.display(),
            MEM_FILENAME,
        );
        run_curl(&self.socket_path, "PUT", "/snapshot/create", &payload)
            .with_context(|| "PUT /snapshot/create")?;
        Ok(())
    }

    fn load_snapshot(&self, dir: &Path) -> Result<()> {
        self.ensure_socket()?;
        let payload = format!(
            r#"{{"snapshot_path":"{}/{}","mem_file_path":"{}/{}","resume_vm":true}}"#,
            dir.display(),
            VMSTATE_FILENAME,
            dir.display(),
            MEM_FILENAME,
        );
        run_curl(&self.socket_path, "PUT", "/snapshot/load", &payload)
            .with_context(|| "PUT /snapshot/load")?;
        Ok(())
    }
}

fn run_curl(socket: &Path, method: &str, endpoint: &str, body: &str) -> Result<()> {
    use std::process::Command;
    let url = format!("http://localhost{endpoint}");
    let out = Command::new("curl")
        .arg("--unix-socket")
        .arg(socket)
        .arg("-fsS")
        .arg("-X")
        .arg(method)
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(body)
        .arg(&url)
        .output()
        .with_context(|| format!("invoking curl for {method} {endpoint}"))?;
    if !out.status.success() {
        bail!(
            "{method} {endpoint} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Run a closure with `MVM_DATA_DIR` overridden to a tempdir
    /// so each test gets an isolated `~/.mvm/instances/...` tree.
    /// The override is restored on drop. Tests that touch the data
    /// dir take this guard; serialisation across tests is via
    /// `DATA_DIR_LOCK` since `set_var` is process-global.
    struct DataDirGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl DataDirGuard {
        fn new() -> Self {
            let lock = super::super::DATA_DIR_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev = std::env::var("MVM_DATA_DIR").ok();
            // SAFETY: the lock above serialises this set/restore
            // pair across the test binary; no other threads are
            // observing MVM_DATA_DIR while the guard is held.
            unsafe {
                std::env::set_var("MVM_DATA_DIR", tmp.path());
            }
            DataDirGuard {
                _guard: lock,
                prev,
                _tmp: tmp,
            }
        }
    }

    impl Drop for DataDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("MVM_DATA_DIR", v),
                    None => std::env::remove_var("MVM_DATA_DIR"),
                }
            }
        }
    }

    fn canned() -> CannedIO {
        CannedIO {
            vmstate_bytes: b"vmstate-bytes".to_vec(),
            mem_bytes: b"memory-image".to_vec(),
        }
    }

    #[test]
    fn snapshot_dir_lives_under_data_dir() {
        let _g = DataDirGuard::new();
        let dir = snapshot_dir("vm-1");
        assert!(dir.starts_with(mvm_core::config::mvm_data_dir()));
        assert!(dir.ends_with("instances/vm-1/snapshot"));
    }

    #[test]
    fn pause_and_seal_creates_files_with_mode_0600() {
        let _g = DataDirGuard::new();
        let sidecar = pause_and_seal("vm-1", &canned()).unwrap();
        assert_eq!(sidecar.epoch, 1);
        let dir = snapshot_dir("vm-1");
        for name in [VMSTATE_FILENAME, MEM_FILENAME, SIDECAR_FILENAME] {
            let p = dir.join(name);
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name} should be mode 0600");
        }
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "snapshot dir should be mode 0700");
    }

    #[test]
    fn pause_and_seal_advances_epoch() {
        let _g = DataDirGuard::new();
        let s1 = pause_and_seal("vm-1", &canned()).unwrap();
        let s2 = pause_and_seal("vm-1", &canned()).unwrap();
        let s3 = pause_and_seal("vm-1", &canned()).unwrap();
        assert_eq!(s1.epoch, 1);
        assert_eq!(s2.epoch, 2);
        assert_eq!(s3.epoch, 3);
    }

    #[test]
    fn verify_and_resume_accepts_freshly_sealed_snapshot() {
        let _g = DataDirGuard::new();
        let sealed = pause_and_seal("vm-1", &canned()).unwrap();
        let verified = verify_and_resume("vm-1", &canned()).unwrap();
        assert_eq!(verified, sealed);
    }

    #[test]
    fn verify_and_resume_rejects_tampered_mem() {
        let _g = DataDirGuard::new();
        pause_and_seal("vm-1", &canned()).unwrap();
        let mem_path = snapshot_dir("vm-1").join(MEM_FILENAME);
        let mut bytes = std::fs::read(&mem_path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&mem_path, &bytes).unwrap();
        let err = verify_and_resume("vm-1", &canned()).unwrap_err();
        assert!(
            err.to_string().contains("HMAC verification"),
            "expected HMAC mismatch, got {err}"
        );
    }

    #[test]
    fn verify_and_resume_rejects_replayed_older_envelope() {
        // Seal at epoch 1, copy the bytes aside, seal again at
        // epoch 2 (overwriting), then restore the epoch-1 files +
        // sidecar to disk and re-verify. The persistent epoch
        // counter still reads 2, so the verifier must refuse.
        let _g = DataDirGuard::new();
        let dir = snapshot_dir("vm-1");
        let _ = pause_and_seal("vm-1", &canned()).unwrap();
        let v1_vmstate = std::fs::read(dir.join(VMSTATE_FILENAME)).unwrap();
        let v1_mem = std::fs::read(dir.join(MEM_FILENAME)).unwrap();
        let v1_sidecar = std::fs::read(dir.join(SIDECAR_FILENAME)).unwrap();

        let _ = pause_and_seal("vm-1", &canned()).unwrap();
        // Roll the visible files back to the epoch-1 state, but
        // leave the persisted epoch counter at 2.
        std::fs::write(dir.join(VMSTATE_FILENAME), &v1_vmstate).unwrap();
        std::fs::write(dir.join(MEM_FILENAME), &v1_mem).unwrap();
        std::fs::write(dir.join(SIDECAR_FILENAME), &v1_sidecar).unwrap();

        let err = verify_and_resume("vm-1", &canned()).unwrap_err();
        assert!(
            err.to_string().contains("replayed"),
            "expected replay rejection, got {err}"
        );
    }

    #[test]
    fn verify_and_resume_errors_when_snapshot_dir_missing() {
        let _g = DataDirGuard::new();
        let err = verify_and_resume("nope", &canned()).unwrap_err();
        assert!(err.to_string().contains("no instance snapshot directory"));
    }

    #[test]
    fn delete_instance_snapshot_removes_files() {
        let _g = DataDirGuard::new();
        pause_and_seal("vm-1", &canned()).unwrap();
        assert!(delete_instance_snapshot("vm-1").unwrap());
        assert!(!snapshot_dir("vm-1").exists());
        // Idempotent — second delete returns false.
        assert!(!delete_instance_snapshot("vm-1").unwrap());
    }

    #[test]
    fn list_instance_snapshots_returns_each_sealed_vm() {
        let _g = DataDirGuard::new();
        pause_and_seal("alpha", &canned()).unwrap();
        pause_and_seal("beta", &canned()).unwrap();
        let entries = list_instance_snapshots().unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.vm_name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        for entry in &entries {
            let sidecar = entry.sidecar.as_ref().expect("sealed → sidecar parses");
            assert_eq!(sidecar.epoch, 1);
            assert!(entry.vmstate_size_bytes > 0);
            assert!(entry.mem_size_bytes > 0);
        }
    }

    #[test]
    fn list_handles_unsealed_snapshot_gracefully() {
        let _g = DataDirGuard::new();
        // Manually create an unsealed snapshot (vmstate + mem but
        // no integrity.json) — the listing should report it with
        // `sidecar = None` rather than failing.
        let dir = prepare_instance_snapshot_dir("ghost").unwrap();
        std::fs::write(dir.join(VMSTATE_FILENAME), b"vmstate").unwrap();
        std::fs::write(dir.join(MEM_FILENAME), b"mem").unwrap();
        let entries = list_instance_snapshots().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].vm_name, "ghost");
        assert!(entries[0].sidecar.is_none());
    }

    #[test]
    fn list_returns_empty_when_root_missing() {
        let _g = DataDirGuard::new();
        assert!(list_instance_snapshots().unwrap().is_empty());
    }
}
