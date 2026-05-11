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

    #[test]
    fn over_cap_is_refused() {
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
