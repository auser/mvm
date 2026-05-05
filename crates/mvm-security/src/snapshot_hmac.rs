//! Snapshot integrity via HMAC-SHA256. ADR-007 / plan 41 W4 / M9.
//!
//! Firecracker snapshots are a memory image plus a state file written
//! to disk. dm-verity (W3) protects rootfs *disk* reads, but a saved
//! snapshot's memory image is a separate trust path — anyone who can
//! write to the snapshot directory can swap it for arbitrary bytes
//! and cause `mvmctl` to resume into attacker-controlled state.
//!
//! This module locks that down by:
//!
//! 1. Generating a host-local HMAC key on first use (`~/.mvm/snapshot.key`,
//!    32 random bytes, mode 0600).
//! 2. Computing HMAC-SHA256 over the snapshot files plus a metadata
//!    record (mvmctl version, file lengths) at create time.
//! 3. Writing a sidecar `integrity.json` next to the snapshot
//!    atomically (`<file>.tmp` + fsync + rename).
//! 4. Recomputing and comparing on restore. Mismatch refuses resume.
//!
//! The sidecar format is structured (versioned JSON) rather than raw
//! bytes so we can extend it later (add fields, rotate algorithms)
//! without breaking compat — unknown fields are rejected today
//! (`deny_unknown_fields`) and a migration would bump
//! `schema_version`.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Length of the host-local snapshot HMAC key, in bytes. 32 bytes =
/// 256 bits = matches HMAC-SHA256's nominal key strength.
pub const HMAC_KEY_BYTES: usize = 32;

/// Filename of the snapshot files on disk. Templates use these
/// canonical names today (`vmstate.bin` and `mem.bin`).
pub const VMSTATE_FILENAME: &str = "vmstate.bin";
pub const MEM_FILENAME: &str = "mem.bin";

/// Default path of the host-local HMAC key relative to the data dir.
pub fn default_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("snapshot.key")
}

/// Convenience: build a [`SnapshotFiles`] for a snapshot directory
/// using the canonical filenames.
pub fn files_in(snap_dir: &Path) -> SnapshotFiles {
    SnapshotFiles {
        vmstate: snap_dir.join(VMSTATE_FILENAME),
        mem: snap_dir.join(MEM_FILENAME),
    }
}

/// Default sidecar filename written next to a snapshot's vmstate /
/// memory image files.
pub const SIDECAR_FILENAME: &str = "integrity.json";

/// Schema version of the sidecar JSON. Bump on any breaking change to
/// the structure or HMAC computation.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Files that get HMAC'd into a single snapshot integrity record.
/// Length-prefixing in the HMAC computation prevents a chosen-prefix
/// splice between the two files (otherwise an attacker could move
/// bytes from `vmstate.bin` into `mem.bin` without changing the
/// concatenation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFiles {
    pub vmstate: PathBuf,
    pub mem: PathBuf,
}

/// Sidecar `integrity.json` written next to the snapshot files. The
/// `tag_hex` is the HMAC over `(version, vmstate_len, vmstate_bytes,
/// mem_len, mem_bytes, mvmctl_version)` with explicit length prefixes
/// — see [`compute_tag`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegritySidecar {
    pub schema_version: u32,
    pub algorithm: String,
    pub vmstate_len: u64,
    pub mem_len: u64,
    pub mvmctl_version: String,
    /// HMAC-SHA256 tag, hex-encoded (lowercase, 64 chars).
    pub tag_hex: String,
}

impl IntegritySidecar {
    pub fn algorithm_label() -> &'static str {
        "HMAC-SHA256"
    }
}

/// Reasons verification can fail. The runtime maps these to clear
/// operator-facing errors.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("integrity sidecar {} missing or unreadable: {detail}", path.display())]
    SidecarMissing { path: PathBuf, detail: String },
    #[error("integrity sidecar {} could not be parsed: {detail}", path.display())]
    SidecarParse { path: PathBuf, detail: String },
    #[error("integrity sidecar schema_version={got} but agent only knows {known}")]
    SchemaMismatch { got: u32, known: u32 },
    #[error("integrity sidecar algorithm '{got}', expected '{expected}'")]
    AlgorithmMismatch { got: String, expected: String },
    #[error("snapshot file size mismatch: sidecar says {expected} bytes for {file}, found {got}")]
    SizeMismatch {
        file: &'static str,
        expected: u64,
        got: u64,
    },
    #[error(
        "snapshot was sealed by mvmctl '{sealed}', current is '{current}' \
         (set MVM_ALLOW_STALE_SNAPSHOT=1 to override)"
    )]
    VersionMismatch { sealed: String, current: String },
    #[error("HMAC tag mismatch — snapshot bytes have been tampered or the host key changed")]
    TagMismatch,
    #[error("I/O while reading snapshot files: {0}")]
    Io(String),
    #[error("HMAC tag in sidecar is not valid hex of expected length")]
    BadTagEncoding,
}

// ============================================================================
// Host key
// ============================================================================

/// Resolve `~/.mvm/snapshot.key` (or any equivalent path the caller
/// provides), creating it if missing with 32 random bytes and mode
/// 0600. Returns the key bytes. Idempotent.
pub fn load_or_init_key(path: &Path) -> Result<[u8; HMAC_KEY_BYTES]> {
    if let Some(parent) = path.parent() {
        // Create parent if missing. Don't enforce parent perms here —
        // `~/.mvm/` is owned by other code (config dir helper).
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent of {}", path.display()))?;
    }

    if !path.exists() {
        let mut buf = [0u8; HMAC_KEY_BYTES];
        rand::thread_rng().fill_bytes(&mut buf);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(&buf)
            .with_context(|| format!("writing {}", path.display()))?;
        f.sync_all().ok();
        return Ok(buf);
    }

    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        // Tighten perms in place rather than refuse — the user may
        // have created the dir themselves; we want to be self-healing.
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    if metadata.len() != HMAC_KEY_BYTES as u64 {
        bail!(
            "{} exists but is {} bytes (expected {}); refuse to use",
            path.display(),
            metadata.len(),
            HMAC_KEY_BYTES
        );
    }

    let mut buf = [0u8; HMAC_KEY_BYTES];
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    f.read_exact(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(buf)
}

// ============================================================================
// HMAC computation
// ============================================================================

/// Compute the snapshot integrity tag over the two snapshot files
/// plus the mvmctl version. The HMAC input is laid out as:
///
/// ```text
/// be_u32(SIDECAR_SCHEMA_VERSION)
/// be_u64(vmstate_len) || vmstate_bytes
/// be_u64(mem_len)     || mem_bytes
/// be_u32(version_str_len) || version_str
/// ```
///
/// Length prefixes prevent a chosen-prefix splice that moves bytes
/// between `vmstate` and `mem` without detection.
pub fn compute_tag(
    files: &SnapshotFiles,
    mvmctl_version: &str,
    key: &[u8; HMAC_KEY_BYTES],
) -> Result<(IntegritySidecar, [u8; 32])> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&SIDECAR_SCHEMA_VERSION.to_be_bytes());

    let vmstate_len = stream_into_mac(&mut mac, &files.vmstate)
        .with_context(|| format!("hashing {}", files.vmstate.display()))?;
    let mem_len = stream_into_mac(&mut mac, &files.mem)
        .with_context(|| format!("hashing {}", files.mem.display()))?;

    let version_bytes = mvmctl_version.as_bytes();
    let version_len: u32 = version_bytes
        .len()
        .try_into()
        .context("mvmctl version unreasonably long")?;
    mac.update(&version_len.to_be_bytes());
    mac.update(version_bytes);

    let tag = mac.finalize().into_bytes();
    let mut tag_arr = [0u8; 32];
    tag_arr.copy_from_slice(&tag);

    let sidecar = IntegritySidecar {
        schema_version: SIDECAR_SCHEMA_VERSION,
        algorithm: IntegritySidecar::algorithm_label().to_string(),
        vmstate_len,
        mem_len,
        mvmctl_version: mvmctl_version.to_string(),
        tag_hex: hex_encode(&tag_arr),
    };
    Ok((sidecar, tag_arr))
}

/// Stream a file's contents into the HMAC, length-prefixed with
/// `be_u64(file_len)`. Returns the length so the caller can record it
/// in the sidecar and compare on verify.
fn stream_into_mac(mac: &mut HmacSha256, path: &Path) -> Result<u64> {
    let metadata = std::fs::metadata(path)?;
    let len = metadata.len();
    mac.update(&len.to_be_bytes());

    let f = File::open(path)?;
    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut total: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        mac.update(&buf[..n]);
        total += n as u64;
    }
    if total != len {
        bail!(
            "{} changed size during hash (started {len}, read {total}) — concurrent writer?",
            path.display()
        );
    }
    Ok(len)
}

// ============================================================================
// Seal / verify the sidecar atomically
// ============================================================================

/// Compute and write the integrity sidecar atomically next to the
/// snapshot files.
pub fn seal(
    snap_dir: &Path,
    files: &SnapshotFiles,
    mvmctl_version: &str,
    key: &[u8; HMAC_KEY_BYTES],
) -> Result<IntegritySidecar> {
    let (sidecar, _tag) = compute_tag(files, mvmctl_version, key)?;
    let json = serde_json::to_vec_pretty(&sidecar).context("serialize sidecar")?;

    let final_path = snap_dir.join(SIDECAR_FILENAME);
    let tmp_path = snap_dir.join(format!("{SIDECAR_FILENAME}.tmp"));

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("open {} for write", tmp_path.display()))?;
        f.write_all(&json)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
    }

    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), final_path.display()))?;

    Ok(sidecar)
}

/// Verify the sidecar at `snap_dir/integrity.json`. Returns `Ok(())`
/// on a clean match. Errors are surfaced as [`VerifyError`] so the
/// caller can map to the operator-facing message (and decide whether
/// to refuse boot or honour `--allow-stale-snapshot`).
pub fn verify(
    snap_dir: &Path,
    files: &SnapshotFiles,
    mvmctl_version: &str,
    key: &[u8; HMAC_KEY_BYTES],
    allow_stale: bool,
) -> std::result::Result<IntegritySidecar, VerifyError> {
    let sidecar_path = snap_dir.join(SIDECAR_FILENAME);
    let raw = std::fs::read(&sidecar_path).map_err(|e| VerifyError::SidecarMissing {
        path: sidecar_path.clone(),
        detail: e.to_string(),
    })?;
    let sidecar: IntegritySidecar =
        serde_json::from_slice(&raw).map_err(|e| VerifyError::SidecarParse {
            path: sidecar_path.clone(),
            detail: e.to_string(),
        })?;

    if sidecar.schema_version != SIDECAR_SCHEMA_VERSION {
        return Err(VerifyError::SchemaMismatch {
            got: sidecar.schema_version,
            known: SIDECAR_SCHEMA_VERSION,
        });
    }
    if sidecar.algorithm != IntegritySidecar::algorithm_label() {
        return Err(VerifyError::AlgorithmMismatch {
            got: sidecar.algorithm.clone(),
            expected: IntegritySidecar::algorithm_label().to_string(),
        });
    }

    // Check sizes before streaming the full files — fast fail on
    // gross mismatch, and bounded I/O if the files have been replaced
    // with something larger.
    let vmstate_len = std::fs::metadata(&files.vmstate)
        .map_err(|e| VerifyError::Io(format!("stat {}: {e}", files.vmstate.display())))?
        .len();
    if vmstate_len != sidecar.vmstate_len {
        return Err(VerifyError::SizeMismatch {
            file: "vmstate.bin",
            expected: sidecar.vmstate_len,
            got: vmstate_len,
        });
    }
    let mem_len = std::fs::metadata(&files.mem)
        .map_err(|e| VerifyError::Io(format!("stat {}: {e}", files.mem.display())))?
        .len();
    if mem_len != sidecar.mem_len {
        return Err(VerifyError::SizeMismatch {
            file: "mem.bin",
            expected: sidecar.mem_len,
            got: mem_len,
        });
    }

    if !allow_stale && sidecar.mvmctl_version != mvmctl_version {
        return Err(VerifyError::VersionMismatch {
            sealed: sidecar.mvmctl_version.clone(),
            current: mvmctl_version.to_string(),
        });
    }

    let (recomputed, tag_bytes) = compute_tag(files, &sidecar.mvmctl_version, key)
        .map_err(|e| VerifyError::Io(e.to_string()))?;
    let _ = recomputed; // we return the original sidecar for the caller's audit

    let stored_bytes = hex_decode(&sidecar.tag_hex).ok_or(VerifyError::BadTagEncoding)?;
    if stored_bytes.len() != tag_bytes.len() || !constant_time_eq(&stored_bytes, &tag_bytes) {
        return Err(VerifyError::TagMismatch);
    }

    Ok(sidecar)
}

// ============================================================================
// Helpers
// ============================================================================

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_nibble(b >> 4));
        s.push(hex_nibble(b & 0x0f));
    }
    s
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble_from(bytes[i])?;
        let lo = nibble_from(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn nibble_from(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

/// Constant-time byte comparison. Avoids leaking match-prefix length
/// via timing — more thorough HMAC libraries do this internally, but
/// when comparing the stored tag against a recomputed one we go
/// through the bytes ourselves.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snap(dir: &Path) -> SnapshotFiles {
        let v = dir.join("vmstate.bin");
        let m = dir.join("mem.bin");
        std::fs::write(&v, b"vmstate-bytes-here").unwrap();
        std::fs::write(&m, b"memory-image-bytes").unwrap();
        SnapshotFiles { vmstate: v, mem: m }
    }

    #[test]
    fn test_load_or_init_creates_key_with_mode_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&path).unwrap();
        assert_eq!(key.len(), HMAC_KEY_BYTES);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be mode 0600");
    }

    #[test]
    fn test_load_or_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot.key");
        let k1 = load_or_init_key(&path).unwrap();
        let k2 = load_or_init_key(&path).unwrap();
        assert_eq!(k1, k2, "second call must return the same key");
    }

    #[test]
    fn test_load_or_init_tightens_loose_perms() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot.key");
        let mut data = [0u8; HMAC_KEY_BYTES];
        rand::thread_rng().fill_bytes(&mut data);
        std::fs::write(&path, data).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _ = load_or_init_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_load_or_init_rejects_wrong_size_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot.key");
        std::fs::write(&path, b"short").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = load_or_init_key(&path).unwrap_err();
        assert!(err.to_string().contains("expected 32"));
    }

    #[test]
    fn test_seal_then_verify_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        let sealed = seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        let verified = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap();
        assert_eq!(sealed, verified);
    }

    #[test]
    fn test_verify_rejects_tampered_vmstate() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        // Tamper: replace one byte but keep the same length.
        let mut bytes = std::fs::read(&files.vmstate).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&files.vmstate, &bytes).unwrap();

        let err = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap_err();
        assert!(matches!(err, VerifyError::TagMismatch));
    }

    #[test]
    fn test_verify_rejects_tampered_mem() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        let mut bytes = std::fs::read(&files.mem).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&files.mem, &bytes).unwrap();

        let err = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap_err();
        assert!(matches!(err, VerifyError::TagMismatch));
    }

    #[test]
    fn test_verify_detects_size_mismatch_fast_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        // Truncate vmstate — caught by the fast-fail size check
        // before we stream the file.
        std::fs::write(&files.vmstate, b"shorter").unwrap();

        let err = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap_err();
        match err {
            VerifyError::SizeMismatch { file, .. } => {
                assert_eq!(file, "vmstate.bin");
            }
            other => panic!("expected SizeMismatch, got {other}"),
        }
    }

    #[test]
    fn test_verify_rejects_wrong_key() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());

        let key_path = tmp.path().join("snapshot.key");
        let key1 = load_or_init_key(&key_path).unwrap();
        seal(tmp.path(), &files, "1.2.3", &key1).unwrap();

        // Replace the key on disk to simulate a fresh host or rotation.
        std::fs::remove_file(&key_path).unwrap();
        let key2 = load_or_init_key(&key_path).unwrap();
        assert_ne!(key1, key2);

        let err = verify(tmp.path(), &files, "1.2.3", &key2, false).unwrap_err();
        assert!(matches!(err, VerifyError::TagMismatch));
    }

    #[test]
    fn test_verify_version_mismatch_blocks_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        let err = verify(tmp.path(), &files, "1.2.4", &key, false).unwrap_err();
        match err {
            VerifyError::VersionMismatch { sealed, current } => {
                assert_eq!(sealed, "1.2.3");
                assert_eq!(current, "1.2.4");
            }
            other => panic!("expected VersionMismatch, got {other}"),
        }
    }

    #[test]
    fn test_verify_version_mismatch_allowed_with_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();

        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        // allow_stale=true must still recompute against the sealed
        // mvmctl_version (so the tag matches), even though the
        // current binary advertises something different.
        verify(tmp.path(), &files, "9.9.9", &key, true).expect("should accept");
    }

    #[test]
    fn test_verify_missing_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();
        // No seal — sidecar is absent.
        let err = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap_err();
        assert!(matches!(err, VerifyError::SidecarMissing { .. }));
    }

    #[test]
    fn test_verify_rejects_unknown_field() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();
        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        // Re-write the sidecar with an extra field.
        let path = tmp.path().join(SIDECAR_FILENAME);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["extra_field"] = serde_json::Value::Bool(true);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let err = verify(tmp.path(), &files, "1.2.3", &key, false).unwrap_err();
        assert!(matches!(err, VerifyError::SidecarParse { .. }));
    }

    #[test]
    fn test_seal_writes_sidecar_mode_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();
        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        let mode = std::fs::metadata(tmp.path().join(SIDECAR_FILENAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_seal_atomic_no_partial_sidecar_on_disk() {
        // The sealing path writes <SIDECAR>.tmp then renames. After
        // a successful seal the .tmp file must not exist; only the
        // final sidecar.
        let tmp = tempfile::tempdir().unwrap();
        let files = make_snap(tmp.path());
        let key_path = tmp.path().join("snapshot.key");
        let key = load_or_init_key(&key_path).unwrap();
        seal(tmp.path(), &files, "1.2.3", &key).unwrap();
        assert!(!tmp.path().join(format!("{SIDECAR_FILENAME}.tmp")).exists());
        assert!(tmp.path().join(SIDECAR_FILENAME).exists());
    }

    #[test]
    fn test_compute_tag_length_prefixing_prevents_splice() {
        // Two snapshots with the same total bytes but different
        // boundary between vmstate and mem must produce different
        // tags. Without length-prefixing they'd be indistinguishable.
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp1.path().join("vmstate.bin"), b"AAA").unwrap();
        std::fs::write(tmp1.path().join("mem.bin"), b"BBBBB").unwrap();
        std::fs::write(tmp2.path().join("vmstate.bin"), b"AAAA").unwrap();
        std::fs::write(tmp2.path().join("mem.bin"), b"BBBB").unwrap();

        let files1 = SnapshotFiles {
            vmstate: tmp1.path().join("vmstate.bin"),
            mem: tmp1.path().join("mem.bin"),
        };
        let files2 = SnapshotFiles {
            vmstate: tmp2.path().join("vmstate.bin"),
            mem: tmp2.path().join("mem.bin"),
        };

        let key = [0u8; HMAC_KEY_BYTES];
        let (_, tag1) = compute_tag(&files1, "1", &key).unwrap();
        let (_, tag2) = compute_tag(&files2, "1", &key).unwrap();
        assert_ne!(tag1, tag2, "splice across vmstate/mem must change the tag");
    }

    #[test]
    fn test_constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
