//! Tarball intake for `mvmctl up --flake <path>.tar.gz`.
//!
//! Implements the binding contract at
//! `specs/contracts/mvm-archive-input.md`. Dispatch on filesystem
//! type:
//!
//! - **Directory** → `FlakeInput::Directory` (existing behavior).
//! - **Regular file ending in `.tar.gz` or `.tgz`** (case-insensitive)
//!   → extract to a 0700 tempdir under the runtime working area,
//!   verify the layout (`flake.nix`, `launch.json`, `source/`), and
//!   return `FlakeInput::Archive`. The temp dir is cleaned up when
//!   the returned `TempDir` guard drops.
//! - **`<scheme>:` ref** (`github:org/repo`, `git+https://…`) →
//!   `FlakeInput::Remote` (passthrough).
//! - **Anything else** → `ArchiveError::InputKindUnsupported`.
//!
//! Error codes match the contract verbatim — `E_UP_INPUT_KIND_UNSUPPORTED`,
//! `E_ARCHIVE_PATH_TRAVERSAL`, `E_ARCHIVE_TOO_LARGE`,
//! `E_ARCHIVE_LAYOUT_INVALID` — and are surfaced as the first token of
//! the error's `Display` output.

use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

/// Default inflated-size cap (1 GiB). Configurable via
/// `MVMCTL_MAX_ARCHIVE_INFLATED_BYTES`.
pub const DEFAULT_INFLATED_CAP_BYTES: u64 = 1024 * 1024 * 1024;

/// Env var to override the inflated-size cap.
pub const INFLATED_CAP_ENV: &str = "MVMCTL_MAX_ARCHIVE_INFLATED_BYTES";

/// Default cumulative cap across all concurrent extractions (4 GiB).
/// Bounds disk-pressure when several `mvmctl up` calls run in parallel
/// — each individual extraction is capped by `INFLATED_CAP_ENV`, but
/// without a cumulative ceiling N concurrent extractions could still
/// occupy N × per-call. Configurable via
/// `MVMCTL_MAX_TOTAL_INFLATED_BYTES`.
pub const DEFAULT_TOTAL_INFLATED_CAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Env var for the cumulative cap.
pub const TOTAL_INFLATED_CAP_ENV: &str = "MVMCTL_MAX_TOTAL_INFLATED_BYTES";

/// Classified `--flake` input. Owns the tempdir guard for the archive
/// case so the caller can keep the extracted tree alive for the
/// duration of the boot pipeline by holding the value.
#[derive(Debug)]
pub enum FlakeInput {
    /// A directory path (existing behavior — handed off as-is).
    Directory(PathBuf),
    /// An extracted tarball. `path` is inside `guard`; dropping `guard`
    /// removes the directory.
    Archive {
        path: PathBuf,
        // Held only for its `Drop` (cleans up the extracted dir).
        // dead_code-allowed because no caller reads the guard directly.
        #[allow(dead_code)]
        guard: TempDir,
    },
    /// A remote ref (`github:…`, `git+https://…`, etc.) — passthrough.
    Remote(String),
}

impl FlakeInput {
    /// Resolved string the rest of `mvmctl` should feed into the build
    /// pipeline. For archive inputs this is the temp-dir path; for
    /// directory inputs it's the original path; for remote refs it's
    /// the original string.
    pub fn resolved(&self) -> String {
        match self {
            FlakeInput::Directory(p) | FlakeInput::Archive { path: p, .. } => {
                p.to_string_lossy().into_owned()
            }
            FlakeInput::Remote(s) => s.clone(),
        }
    }

    /// True if this input came from extracting a `.tar.gz` / `.tgz`.
    pub fn is_archive(&self) -> bool {
        matches!(self, FlakeInput::Archive { .. })
    }
}

#[derive(Debug)]
pub enum ArchiveError {
    /// Path is neither a directory nor a recognized archive file.
    InputKindUnsupported { path: PathBuf },
    /// A tar entry tried to escape the extraction root (`..` component,
    /// absolute path, or symlink/hardlink entry).
    PathTraversal { entry: String },
    /// Inflated size exceeded the configured cap.
    TooLarge { actual: u64, cap: u64 },
    /// Reserving this extraction would push the cumulative in-flight
    /// inflation past the host-wide ceiling
    /// (`MVMCTL_MAX_TOTAL_INFLATED_BYTES`). Refused before any bytes
    /// are decompressed. Bounds the disk-pressure window across many
    /// concurrent `mvmctl up` calls.
    QuotaExceeded {
        in_flight: u64,
        reservation: u64,
        cap: u64,
    },
    /// Extracted tree is missing a required top-level entry.
    LayoutInvalid { missing: &'static str },
    /// IO error during extraction.
    Io(std::io::Error),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::InputKindUnsupported { path } => write!(
                f,
                "E_UP_INPUT_KIND_UNSUPPORTED: --flake path {:?} is neither a directory \
                 nor a recognized archive (.tar.gz, .tgz)",
                path
            ),
            ArchiveError::PathTraversal { entry } => write!(
                f,
                "E_ARCHIVE_PATH_TRAVERSAL: tar entry {:?} attempted to escape extraction root",
                entry
            ),
            ArchiveError::TooLarge { actual, cap } => write!(
                f,
                "E_ARCHIVE_TOO_LARGE: inflated size {} bytes exceeds cap {} bytes \
                 (override with {})",
                actual, cap, INFLATED_CAP_ENV
            ),
            ArchiveError::QuotaExceeded {
                in_flight,
                reservation,
                cap,
            } => write!(
                f,
                "E_ARCHIVE_QUOTA_EXCEEDED: cumulative inflation cap of {} bytes \
                 would be exceeded by reserving {} more bytes \
                 ({} already in-flight; override with {})",
                cap, reservation, in_flight, TOTAL_INFLATED_CAP_ENV
            ),
            ArchiveError::LayoutInvalid { missing } => write!(
                f,
                "E_ARCHIVE_LAYOUT_INVALID: extracted archive is missing required \
                 top-level entry {:?}",
                missing
            ),
            ArchiveError::Io(e) => write!(f, "io error during archive extraction: {e}"),
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArchiveError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

/// Classify a `--flake` argument and (if it's a tarball) extract it.
///
/// On success returns a `FlakeInput`; for archive inputs the embedded
/// `TempDir` guard owns the extracted directory and must be kept alive
/// for the lifetime of the boot pipeline.
pub fn classify_flake_input(arg: &str) -> Result<FlakeInput, ArchiveError> {
    // Remote ref dispatch — a `:` is the existing-behavior marker
    // (matching `resolve_flake_ref`'s passthrough heuristic). Catches
    // `github:org/repo`, `git+https://…`, `path:./…`, etc.
    if arg.contains(':') {
        return Ok(FlakeInput::Remote(arg.to_string()));
    }
    let path = Path::new(arg);
    if path.is_dir() {
        return Ok(FlakeInput::Directory(path.to_path_buf()));
    }
    if path.is_file() && is_tarball_path(arg) {
        let guard = extract_archive(path)?;
        let extracted = guard.path().to_path_buf();
        return Ok(FlakeInput::Archive {
            path: extracted,
            guard,
        });
    }
    Err(ArchiveError::InputKindUnsupported {
        path: path.to_path_buf(),
    })
}

fn is_tarball_path(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

/// Extract a `.tar.gz` to a 0700 tempdir, refusing path-traversal /
/// over-cap / layout-invalid. Returns the `TempDir` guard.
pub fn extract_archive(path: &Path) -> Result<TempDir, ArchiveError> {
    let cap = inflated_cap();

    // Reserve up to `cap` bytes against the cumulative quota before
    // we touch the gzip stream. If the reservation can't fit, fail
    // fast with `E_ARCHIVE_QUOTA_EXCEEDED` — cheaper than starting
    // decompression and tearing down. The guard releases the
    // reservation on drop (success or failure) by subtracting the
    // amount we actually consumed from the in-flight counter.
    let _quota_guard = QuotaReservation::acquire(cap)?;

    let temp = tempfile::Builder::new().prefix("mvmctl-up-").tempdir()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
    }

    let file = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let mut total: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;

        // Refuse symlink / hardlink entries (defense in depth — the
        // mvmforge archiver doesn't emit them per ADR-0012).
        let etype = entry.header().entry_type();
        if etype.is_symlink() || etype.is_hard_link() {
            let entry_path = entry.path()?.to_string_lossy().into_owned();
            return Err(ArchiveError::PathTraversal { entry: entry_path });
        }

        let entry_path = entry.path()?.into_owned();
        validate_entry_path(&entry_path)?;

        // Track the *content* size against the cap. Directory entries
        // report 0; regular file entries report the file's bytes.
        total = total.saturating_add(entry.header().size().unwrap_or(0));
        if total > cap {
            return Err(ArchiveError::TooLarge { actual: total, cap });
        }

        entry.unpack_in(temp.path())?;
    }

    verify_layout(temp.path())?;

    Ok(temp)
}

fn inflated_cap() -> u64 {
    std::env::var(INFLATED_CAP_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INFLATED_CAP_BYTES)
}

fn total_inflated_cap() -> u64 {
    std::env::var(TOTAL_INFLATED_CAP_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TOTAL_INFLATED_CAP_BYTES)
}

/// Filesystem location of the cumulative-quota counter. Lives under
/// the runtime dir alongside the session table so we don't pollute
/// other state directories. The counter file holds a single ASCII
/// integer (in-flight bytes); concurrent updates are serialized by
/// `fs2`-flavored exclusive locking on the file itself.
fn quota_counter_path() -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_runtime_dir()).join("archive_quota.bytes")
}

/// RAII guard for a cumulative-quota reservation. Created via
/// `QuotaReservation::acquire(cap)`; bumps the in-flight counter by
/// `cap` and releases it on drop.
///
/// **Limitations of the reserve-and-release model**: we reserve the
/// per-call cap up front rather than the actual extracted size,
/// because the extraction's true bytes-on-disk aren't knowable until
/// the gzip stream is parsed. This is conservative: a small archive
/// holds a 1 GiB-worth slot in the quota for the duration of its
/// extraction, even though only a few KiB land on disk. In exchange,
/// the quota check is cheap (one read-modify-write per call) and
/// the worst-case disk-pressure is bounded predictably by
/// `total_inflated_cap()`.
///
/// The guard is **best-effort across processes**: a `kill -9` of an
/// in-flight `mvmctl up` leaves the counter elevated. Operators
/// recover by running `mvmctl session reap` (which sweeps the same
/// runtime dir; we co-opt that as the natural reset point) or by
/// `rm $XDG_RUNTIME_DIR/mvm/archive_quota.bytes` manually. Cheap to
/// recover; not a security boundary.
struct QuotaReservation {
    reservation: u64,
}

impl QuotaReservation {
    fn acquire(reservation: u64) -> Result<Self, ArchiveError> {
        let cap = total_inflated_cap();
        Self::with_counter(|in_flight| {
            // `saturating_add` keeps a malformed counter from
            // panicking; the next branch refuses oversized reservations.
            let after = in_flight.saturating_add(reservation);
            if after > cap {
                return Err(ArchiveError::QuotaExceeded {
                    in_flight,
                    reservation,
                    cap,
                });
            }
            Ok(after)
        })?;
        Ok(Self { reservation })
    }

    /// Read-modify-write the counter under an exclusive flock. The
    /// closure returns the *new* value to write; if it returns Err,
    /// the counter is left unchanged and the error is propagated.
    fn with_counter<F>(update: F) -> Result<(), ArchiveError>
    where
        F: FnOnce(u64) -> Result<u64, ArchiveError>,
    {
        use fs2::FileExt;
        use std::io::{Read, Seek, SeekFrom, Write};

        let path = quota_counter_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.lock_exclusive()?;

        // Read current value (empty file → 0; malformed → 0).
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        let current: u64 = buf.trim().parse().unwrap_or(0);

        let new_value = update(current)?;
        let serialized = new_value.to_string();
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(serialized.as_bytes())?;
        file.flush()?;
        // Lock releases on drop.
        Ok(())
    }
}

impl Drop for QuotaReservation {
    fn drop(&mut self) {
        let r = self.reservation;
        // Best-effort: if the counter file is gone or we can't
        // re-lock, log and move on. The next session-reap or manual
        // cleanup recovers.
        if let Err(e) = Self::with_counter(|in_flight| Ok(in_flight.saturating_sub(r))) {
            tracing::warn!(err = %e, "archive quota: release failed");
        }
    }
}

fn validate_entry_path(p: &Path) -> Result<(), ArchiveError> {
    if p.is_absolute() {
        return Err(ArchiveError::PathTraversal {
            entry: p.to_string_lossy().into_owned(),
        });
    }
    for c in p.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArchiveError::PathTraversal {
                    entry: p.to_string_lossy().into_owned(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn verify_layout(root: &Path) -> Result<(), ArchiveError> {
    if !root.join("flake.nix").is_file() {
        return Err(ArchiveError::LayoutInvalid {
            missing: "flake.nix",
        });
    }
    if !root.join("launch.json").is_file() {
        return Err(ArchiveError::LayoutInvalid {
            missing: "launch.json",
        });
    }
    if !root.join("source").is_dir() {
        return Err(ArchiveError::LayoutInvalid { missing: "source/" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    /// Build a minimal valid tarball: `flake.nix`, `launch.json`,
    /// `source/main.py`. Returned bytes are gzipped tar.
    fn build_valid_tarball() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);

            append_file(&mut builder, "flake.nix", b"# minimal flake\n");
            append_file(&mut builder, "launch.json", b"{}\n");
            // `source/` directory implicitly created by `source/main.py`
            // entry; tar crate creates parents.
            append_file(&mut builder, "source/main.py", b"print('ok')\n");

            builder.finish().unwrap();
        }
        tar_bytes
    }

    fn append_file<W: Write>(builder: &mut tar::Builder<W>, name: &str, contents: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
    }

    fn write_tarball(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .prefix("mvmctl-test-")
            .suffix(".tar.gz")
            .tempfile()
            .unwrap();
        f.write_all(bytes).unwrap();
        f
    }

    #[test]
    fn tarball_extension_recognized_case_insensitive() {
        assert!(is_tarball_path("foo.tar.gz"));
        assert!(is_tarball_path("foo.tgz"));
        assert!(is_tarball_path("foo.TAR.GZ"));
        assert!(is_tarball_path("foo.TGZ"));
        assert!(!is_tarball_path("foo.tar"));
        assert!(!is_tarball_path("foo.zip"));
        assert!(!is_tarball_path("foo"));
    }

    #[test]
    fn classify_directory_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let arg = dir.path().to_str().unwrap();
        match classify_flake_input(arg).unwrap() {
            FlakeInput::Directory(p) => assert_eq!(p, dir.path()),
            other => panic!(
                "expected Directory, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn classify_remote_ref_passes_through() {
        match classify_flake_input("github:org/repo").unwrap() {
            FlakeInput::Remote(s) => assert_eq!(s, "github:org/repo"),
            _ => panic!("expected Remote"),
        }
    }

    #[test]
    fn classify_unsupported_path_errors_with_stable_code() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let err = classify_flake_input(nonexistent.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_UP_INPUT_KIND_UNSUPPORTED"),
            "expected E_UP_INPUT_KIND_UNSUPPORTED, got {err}"
        );
    }

    #[test]
    fn classify_non_archive_file_errors_with_stable_code() {
        let f = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
        let err = classify_flake_input(f.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_UP_INPUT_KIND_UNSUPPORTED"),
            "expected E_UP_INPUT_KIND_UNSUPPORTED, got {err}"
        );
    }

    #[test]
    fn happy_path_extracts_to_0700_tempdir_with_required_layout() {
        let _lock = lock_archive_env();
        let bytes = build_valid_tarball();
        let archive = write_tarball(&bytes);
        let arg = archive.path().to_str().unwrap();

        let input = classify_flake_input(arg).unwrap();
        match input {
            FlakeInput::Archive { path, guard } => {
                assert!(path.join("flake.nix").is_file());
                assert!(path.join("launch.json").is_file());
                assert!(path.join("source").is_dir());
                assert!(path.join("source").join("main.py").is_file());

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o700, "tempdir should be mode 0700");
                }

                // Drop the guard explicitly and verify cleanup.
                let to_check = path.clone();
                drop(guard);
                assert!(
                    !to_check.exists(),
                    "TempDir should clean up the extraction directory on drop"
                );
            }
            _ => panic!("expected Archive variant"),
        }
    }

    #[test]
    fn path_traversal_dotdot_is_refused() {
        let _lock = lock_archive_env();
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);
            append_file(&mut builder, "flake.nix", b"# flake\n");
            append_file(&mut builder, "launch.json", b"{}\n");
            // Sneaky entry escaping the root. Note: tar::Header::set_path
            // refuses absolute paths and `..`, so we craft via raw bytes.
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            // Set the path directly, bypassing set_path validation.
            let path_bytes = b"../escaped.txt";
            let name_field = &mut header.as_old_mut().name;
            name_field[..path_bytes.len()].copy_from_slice(path_bytes);
            header.set_cksum();
            builder.append(&header, &b"x"[..]).unwrap();
            builder.finish().unwrap();
        }
        let archive = write_tarball(&tar_bytes);

        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_ARCHIVE_PATH_TRAVERSAL"),
            "expected E_ARCHIVE_PATH_TRAVERSAL, got {err}"
        );
    }

    #[test]
    fn absolute_path_entry_is_refused() {
        let _lock = lock_archive_env();
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);
            append_file(&mut builder, "flake.nix", b"# flake\n");
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            let path_bytes = b"/etc/escaped.txt";
            let name_field = &mut header.as_old_mut().name;
            name_field[..path_bytes.len()].copy_from_slice(path_bytes);
            header.set_cksum();
            builder.append(&header, &b"x"[..]).unwrap();
            builder.finish().unwrap();
        }
        let archive = write_tarball(&tar_bytes);

        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_ARCHIVE_PATH_TRAVERSAL"),
            "expected E_ARCHIVE_PATH_TRAVERSAL, got {err}"
        );
    }

    /// Serializes archive-extraction tests within this module so
    /// `INFLATED_CAP_ENV` / `TOTAL_INFLATED_CAP_ENV` / `MVM_RUNTIME_DIR`
    /// don't race across cargo's parallel test threads. Tests that
    /// don't trigger extraction (directory passthrough, remote ref,
    /// extension detection, etc.) don't need the lock; the helper
    /// `lock_archive_env()` returns a guard the extracting tests
    /// hold for their duration.
    static QUOTA_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_archive_env() -> std::sync::MutexGuard<'static, ()> {
        QUOTA_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// RAII helper: holds the ENV_LOCK and pins the runtime + total
    /// cap env vars; restores prior values on drop.
    struct QuotaEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _runtime: tempfile::TempDir,
        prev_runtime: Option<String>,
        prev_total: Option<String>,
    }

    impl Drop for QuotaEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_runtime.take() {
                    Some(v) => std::env::set_var("MVM_RUNTIME_DIR", v),
                    None => std::env::remove_var("MVM_RUNTIME_DIR"),
                }
                match self.prev_total.take() {
                    Some(v) => std::env::set_var(TOTAL_INFLATED_CAP_ENV, v),
                    None => std::env::remove_var(TOTAL_INFLATED_CAP_ENV),
                }
            }
        }
    }

    fn pin_quota_env(total_cap_bytes: u64) -> QuotaEnvGuard {
        let lock = QUOTA_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let runtime = tempfile::tempdir().expect("tempdir");
        let prev_runtime = std::env::var("MVM_RUNTIME_DIR").ok();
        let prev_total = std::env::var(TOTAL_INFLATED_CAP_ENV).ok();
        unsafe {
            std::env::set_var("MVM_RUNTIME_DIR", runtime.path());
            std::env::set_var(TOTAL_INFLATED_CAP_ENV, total_cap_bytes.to_string());
        }
        QuotaEnvGuard {
            _lock: lock,
            _runtime: runtime,
            prev_runtime,
            prev_total,
        }
    }

    #[test]
    fn over_total_quota_is_refused() {
        // Pre-populate the cumulative counter with 1.5 GiB, set the
        // total cap at 2 GiB, attempt a 1 GiB-reservation extraction.
        // 1.5 + 1 = 2.5 > 2 → E_ARCHIVE_QUOTA_EXCEEDED.
        let guard = pin_quota_env(2 * 1024 * 1024 * 1024);
        let runtime_path = std::path::PathBuf::from(std::env::var("MVM_RUNTIME_DIR").unwrap());
        let counter_path = runtime_path.join("archive_quota.bytes");
        std::fs::create_dir_all(counter_path.parent().unwrap()).unwrap();
        std::fs::write(&counter_path, "1610612736").unwrap(); // 1.5 GiB

        let bytes = build_valid_tarball();
        let archive = write_tarball(&bytes);
        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        drop(guard);

        assert!(
            err.to_string().starts_with("E_ARCHIVE_QUOTA_EXCEEDED"),
            "expected E_ARCHIVE_QUOTA_EXCEEDED, got {err}"
        );
    }

    #[test]
    fn quota_reservation_releases_on_drop() {
        // 1 GiB cap holds one in-flight extraction at the default
        // 1 GiB per-call reservation. Two simultaneous would
        // exceed; two sequential must each succeed because the
        // first releases on drop.
        let guard = pin_quota_env(1024 * 1024 * 1024);
        let runtime_path = std::path::PathBuf::from(std::env::var("MVM_RUNTIME_DIR").unwrap());
        let counter_path = runtime_path.join("archive_quota.bytes");

        let bytes = build_valid_tarball();
        let archive = write_tarball(&bytes);
        let arg = archive.path().to_str().unwrap();

        // First extraction: acquires + releases on drop.
        {
            let _input = classify_flake_input(arg).expect("first extraction");
        }
        let after_first = std::fs::read_to_string(&counter_path)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();
        assert_eq!(after_first, 0, "reservation must release on drop");

        // Second extraction succeeds because the first released.
        {
            let _input = classify_flake_input(arg).expect("second extraction");
        }
        drop(guard);
    }

    #[test]
    fn over_cap_is_refused() {
        let _lock = lock_archive_env();
        // Build a tarball whose declared content bytes exceed a small
        // cap. We don't need the file to actually be on disk —
        // extraction tracks the header-declared size before reading.
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);
            append_file(&mut builder, "flake.nix", b"# flake\n");
            append_file(&mut builder, "launch.json", b"{}\n");
            // A 2 KiB file; we'll set the cap below 2 KiB.
            let big = vec![b'a'; 2048];
            append_file(&mut builder, "source/big.bin", &big);
            builder.finish().unwrap();
        }
        let archive = write_tarball(&tar_bytes);

        // Set cap to 1 KiB; extraction must refuse on the big file.
        // Safety: tests in this module run serially within the same
        // process, but `cargo test` runs tests in parallel by default.
        // Use a temp env override pattern that doesn't race with
        // other tests reading the same var: the test sets it then
        // immediately calls extraction, and no other test in this
        // file reads INFLATED_CAP_ENV.
        // SAFETY: setting an env var in tests is racy with other
        // threads reading env. This module's tests don't read this
        // var elsewhere, so the only risk is interleaving with this
        // single test's own classify_flake_input call — which reads
        // the var on the same thread, after the set, before the
        // unset. That's fine.
        unsafe {
            std::env::set_var(INFLATED_CAP_ENV, "1024");
        }
        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        unsafe {
            std::env::remove_var(INFLATED_CAP_ENV);
        }

        assert!(
            err.to_string().starts_with("E_ARCHIVE_TOO_LARGE"),
            "expected E_ARCHIVE_TOO_LARGE, got {err}"
        );
    }

    #[test]
    fn missing_layout_is_refused() {
        let _lock = lock_archive_env();
        // Tarball with flake.nix but no launch.json or source/.
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);
            append_file(&mut builder, "flake.nix", b"# flake\n");
            builder.finish().unwrap();
        }
        let archive = write_tarball(&tar_bytes);

        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_ARCHIVE_LAYOUT_INVALID"),
            "expected E_ARCHIVE_LAYOUT_INVALID, got {err}"
        );
        assert!(
            err.to_string().contains("launch.json"),
            "error should name the missing entry: {err}"
        );
    }

    #[test]
    fn symlink_entry_is_refused() {
        let _lock = lock_archive_env();
        let mut tar_bytes = Vec::new();
        {
            let gz = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(gz);
            append_file(&mut builder, "flake.nix", b"# flake\n");
            append_file(&mut builder, "launch.json", b"{}\n");
            // Symlink entry pointing at /etc/passwd.
            let mut header = tar::Header::new_gnu();
            header.set_path("source/evil-link").unwrap();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("/etc/passwd").unwrap();
            header.set_cksum();
            builder.append(&header, std::io::empty()).unwrap();
            builder.finish().unwrap();
        }
        let archive = write_tarball(&tar_bytes);

        let err = classify_flake_input(archive.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().starts_with("E_ARCHIVE_PATH_TRAVERSAL"),
            "expected E_ARCHIVE_PATH_TRAVERSAL, got {err}"
        );
    }
}
