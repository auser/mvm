//! Host-side resolver for the mvm runtime overlay disk
//! (ADR-051).
//!
//! Every microVM mvm boots — Nix-built rootfs and OCI-pulled
//! rootfs alike — attaches a second virtio-blk device carrying
//! the guest agent + seccomp shim + runner + per-language SDK
//! runtime libraries. This module is the host-side half: given
//! a cache root + an mvmctl version + a host arch, it picks the
//! right ext4 + verity sidecar + roothash from the local cache
//! and returns paths the backend can attach.
//!
//! ## Cache layout
//!
//! ```text
//! <cache_root>/
//!   runtime-overlay/
//!     <semver>/                # e.g. "0.14.0"
//!       <arch>/                # "aarch64" or "x86_64"
//!         overlay.ext4
//!         overlay.verity
//!         overlay.roothash     # text file, 64 lowercase hex chars + newline
//!         VERSION              # text file, semver of the producing mvmctl
//! ```
//!
//! The VERSION file is what the resolver checks against the
//! caller's `expected_version`. Mismatched versions are an
//! admission-time error (the agent's vsock protocol is
//! versioned per ADR-002 §W4.1; a stale overlay paired with a
//! newer host would silently misbehave).
//!
//! ## Out of scope
//!
//! - **Building** the overlay (`nix build`). W1.4b.2 lands the
//!   Nix flake that produces these artifacts.
//! - **Downloading** the overlay from a release. W1.4b.4 wires
//!   the artifact-acquisition path (similar to how
//!   `download_dev_image` works for the dev VM image).
//! - **Attaching** the overlay to a microVM at boot. W1.4b.2 /
//!   .3 land the backend + `mvm-verity-init` extensions.
//!
//! This module is pure file I/O + string parsing. Cross-
//! platform; no `cfg(target_os)` gates.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Host architecture that an mvm runtime overlay is built for.
/// The overlay is arch-specific because it contains a Linux
/// binary (`mvm-guest-agent`) compiled for a specific arch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// The host arch the current binary was built for.
    /// Compile-time; mvmctl isn't a cross-arch process.
    pub const fn host() -> Self {
        #[cfg(target_arch = "aarch64")]
        {
            Arch::Aarch64
        }
        #[cfg(target_arch = "x86_64")]
        {
            Arch::X86_64
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            // The match is exhaustive on the two arches mvm
            // actually targets; an unsupported arch would fail
            // to build long before reaching this branch. Picking
            // Aarch64 as a stub keeps the function `const`-able
            // for tests that pin a specific arch explicitly.
            Arch::Aarch64
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
pub enum RuntimeOverlayError {
    /// One of the four files the cache layout requires
    /// (`overlay.ext4`, `overlay.verity`, `overlay.roothash`,
    /// `VERSION`) is missing under the expected path.
    #[error(
        "runtime overlay artifact incomplete at {artifact_dir:?}: \
         missing {missing:?} (mvmctl version {version}, arch {arch})"
    )]
    ArtifactIncomplete {
        artifact_dir: PathBuf,
        missing: PathBuf,
        version: String,
        arch: String,
    },

    /// The cache's `VERSION` file content doesn't match the
    /// version the caller expected. Always fail closed — a
    /// version mismatch means the overlay's agent protocol
    /// could disagree with the host's.
    #[error(
        "runtime overlay version mismatch: expected {expected:?}, \
         cache holds {found:?}"
    )]
    VersionMismatch { expected: String, found: String },

    /// The `overlay.roothash` text didn't parse as 64 lowercase
    /// hex chars (sha256). Always fail closed — a malformed
    /// roothash means the kernel cmdline can't be set
    /// correctly and the verity-init would panic at boot.
    #[error("runtime overlay roothash malformed: {reason}")]
    InvalidRoothash { reason: String },

    /// The `VERSION` file is empty, non-UTF-8, or otherwise
    /// unreadable.
    #[error("runtime overlay VERSION file invalid: {reason}")]
    InvalidVersionFile { reason: String },

    /// Underlying io failure during a file read.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// File-system layout for one resolved overlay artifact. The
/// resolver builds this up before checking existence so callers
/// can compute paths without actually invoking the resolver
/// (e.g. for testing or for telling a download orchestrator
/// where to land a fetched artifact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOverlayLayout {
    pub artifact_dir: PathBuf,
    pub overlay_ext4: PathBuf,
    pub sidecar: PathBuf,
    pub roothash_file: PathBuf,
    pub version_file: PathBuf,
    pub arch: Arch,
    pub version: String,
}

impl RuntimeOverlayLayout {
    /// Compute the canonical layout for `(version, arch)` under
    /// `cache_root`. Performs no I/O — pure path construction.
    pub fn under(cache_root: &Path, version: &str, arch: Arch) -> Self {
        let artifact_dir = cache_root
            .join("runtime-overlay")
            .join(version)
            .join(arch.as_str());
        Self {
            overlay_ext4: artifact_dir.join("overlay.ext4"),
            sidecar: artifact_dir.join("overlay.verity"),
            roothash_file: artifact_dir.join("overlay.roothash"),
            version_file: artifact_dir.join("VERSION"),
            artifact_dir,
            arch,
            version: version.to_string(),
        }
    }
}

/// Resolved overlay artifact, ready to hand to a microVM
/// backend as the second virtio-blk drive + sidecar +
/// cmdline arg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOverlayArtifact {
    pub overlay_ext4: PathBuf,
    pub sidecar: PathBuf,
    pub roothash_file: PathBuf,
    /// Root hash as 64 lowercase hex chars (sha256). What
    /// `mvm-verity-init` reads from the kernel cmdline as
    /// `mvm.runtime_roothash=<hex>`.
    pub roothash: String,
    pub arch: Arch,
    pub version: String,
}

/// Host-side resolver for the runtime overlay cache. Stateless
/// configuration: cache root + expected mvmctl version. The
/// `resolve` method does the actual cache probe.
pub struct RuntimeOverlayResolver {
    cache_root: PathBuf,
    expected_version: String,
}

impl RuntimeOverlayResolver {
    /// Create a resolver against `cache_root` (typically
    /// `~/.cache/mvm/`) that expects overlays tagged with
    /// `expected_version` (typically `env!("CARGO_PKG_VERSION")`).
    pub fn new(cache_root: PathBuf, expected_version: String) -> Self {
        Self {
            cache_root,
            expected_version,
        }
    }

    /// Compute the cache layout for `arch` without doing any
    /// I/O. Useful for callers that need to know where an
    /// artifact *would* live (e.g. download orchestrators).
    pub fn layout(&self, arch: Arch) -> RuntimeOverlayLayout {
        RuntimeOverlayLayout::under(&self.cache_root, &self.expected_version, arch)
    }

    /// Find the overlay artifact in cache. Validates:
    ///
    /// 1. All four files exist.
    /// 2. `VERSION` file matches the resolver's expected version.
    /// 3. `overlay.roothash` parses as 64 lowercase hex chars.
    ///
    /// Returns an [`RuntimeOverlayArtifact`] on success. Fails
    /// closed on every other path — no partial / degraded
    /// fallback. Plan 74 §Risks R13: the agent must come from a
    /// trusted overlay or the W3 verity claim is silently
    /// weakened.
    pub fn resolve(&self, arch: Arch) -> Result<RuntimeOverlayArtifact, RuntimeOverlayError> {
        let layout = self.layout(arch);
        check_exists(
            &layout.overlay_ext4,
            &layout.artifact_dir,
            &self.expected_version,
            arch,
        )?;
        check_exists(
            &layout.sidecar,
            &layout.artifact_dir,
            &self.expected_version,
            arch,
        )?;
        check_exists(
            &layout.roothash_file,
            &layout.artifact_dir,
            &self.expected_version,
            arch,
        )?;
        check_exists(
            &layout.version_file,
            &layout.artifact_dir,
            &self.expected_version,
            arch,
        )?;

        let version = read_version_file(&layout.version_file)?;
        if version != self.expected_version {
            return Err(RuntimeOverlayError::VersionMismatch {
                expected: self.expected_version.clone(),
                found: version,
            });
        }

        let roothash = read_and_validate_roothash(&layout.roothash_file)?;

        Ok(RuntimeOverlayArtifact {
            overlay_ext4: layout.overlay_ext4,
            sidecar: layout.sidecar,
            roothash_file: layout.roothash_file,
            roothash,
            arch,
            version,
        })
    }
}

fn check_exists(
    path: &Path,
    artifact_dir: &Path,
    version: &str,
    arch: Arch,
) -> Result<(), RuntimeOverlayError> {
    if !path.is_file() {
        return Err(RuntimeOverlayError::ArtifactIncomplete {
            artifact_dir: artifact_dir.to_path_buf(),
            missing: path.to_path_buf(),
            version: version.to_string(),
            arch: arch.to_string(),
        });
    }
    Ok(())
}

fn read_version_file(path: &Path) -> Result<String, RuntimeOverlayError> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(RuntimeOverlayError::InvalidVersionFile {
            reason: "VERSION file is empty".to_string(),
        });
    }
    if trimmed.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(RuntimeOverlayError::InvalidVersionFile {
            reason: format!("VERSION file contains internal whitespace: {trimmed:?}"),
        });
    }
    Ok(trimmed)
}

fn read_and_validate_roothash(path: &Path) -> Result<String, RuntimeOverlayError> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.trim();
    if trimmed.len() != 64 {
        return Err(RuntimeOverlayError::InvalidRoothash {
            reason: format!(
                "expected 64 hex chars (sha256), got {} chars",
                trimmed.len()
            ),
        });
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(RuntimeOverlayError::InvalidRoothash {
            reason: format!("expected lowercase hex; got {trimmed:?}"),
        });
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FAKE_ROOTHASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn make_cache(version: &str, arch: Arch, with_files: &[(&str, &[u8])]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp
            .path()
            .join("runtime-overlay")
            .join(version)
            .join(arch.as_str());
        std::fs::create_dir_all(&artifact_dir).unwrap();
        for (name, contents) in with_files {
            std::fs::write(artifact_dir.join(name), contents).unwrap();
        }
        tmp
    }

    fn complete_cache(version: &str, arch: Arch) -> TempDir {
        make_cache(
            version,
            arch,
            &[
                ("overlay.ext4", b"ext4-bytes"),
                ("overlay.verity", b"verity-sidecar"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", format!("{version}\n").as_bytes()),
            ],
        )
    }

    #[test]
    fn arch_as_str_matches_kernel_naming() {
        assert_eq!(Arch::Aarch64.as_str(), "aarch64");
        assert_eq!(Arch::X86_64.as_str(), "x86_64");
    }

    #[test]
    fn arch_host_returns_one_of_the_supported_arches() {
        // The const fn must compile and produce a value; the
        // exact value depends on the test binary's target arch.
        let host = Arch::host();
        assert!(matches!(host, Arch::Aarch64 | Arch::X86_64));
    }

    #[test]
    fn layout_under_uses_canonical_directory_layout() {
        let root = Path::new("/cache");
        let layout = RuntimeOverlayLayout::under(root, "0.14.0", Arch::Aarch64);
        assert_eq!(
            layout.artifact_dir,
            PathBuf::from("/cache/runtime-overlay/0.14.0/aarch64")
        );
        assert_eq!(
            layout.overlay_ext4,
            PathBuf::from("/cache/runtime-overlay/0.14.0/aarch64/overlay.ext4")
        );
        assert_eq!(
            layout.sidecar,
            PathBuf::from("/cache/runtime-overlay/0.14.0/aarch64/overlay.verity")
        );
        assert_eq!(
            layout.roothash_file,
            PathBuf::from("/cache/runtime-overlay/0.14.0/aarch64/overlay.roothash")
        );
        assert_eq!(
            layout.version_file,
            PathBuf::from("/cache/runtime-overlay/0.14.0/aarch64/VERSION")
        );
        assert_eq!(layout.arch, Arch::Aarch64);
        assert_eq!(layout.version, "0.14.0");
    }

    #[test]
    fn resolve_returns_artifact_when_all_files_present_and_version_matches() {
        let cache = complete_cache("0.14.0", Arch::Aarch64);
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let artifact = resolver.resolve(Arch::Aarch64).expect("resolve");

        assert_eq!(artifact.version, "0.14.0");
        assert_eq!(artifact.arch, Arch::Aarch64);
        assert_eq!(artifact.roothash, FAKE_ROOTHASH);
        assert!(artifact.overlay_ext4.ends_with("overlay.ext4"));
        assert!(artifact.sidecar.ends_with("overlay.verity"));
        assert!(artifact.roothash_file.ends_with("overlay.roothash"));
    }

    #[test]
    fn resolve_fails_when_overlay_ext4_missing() {
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", b"0.14.0\n"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        match err {
            RuntimeOverlayError::ArtifactIncomplete { missing, .. } => {
                assert!(missing.ends_with("overlay.ext4"), "missing={missing:?}");
            }
            other => panic!("expected ArtifactIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn resolve_fails_when_sidecar_missing() {
        let cache = make_cache(
            "0.14.0",
            Arch::X86_64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", b"0.14.0\n"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::X86_64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::ArtifactIncomplete { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_fails_when_version_file_disagrees() {
        // The artifact lives at the EXPECTED version's path
        // (`0.14.0/<arch>/`), but the VERSION file *inside* it
        // says `0.13.99` — the case where someone manually
        // hand-edited the file or a release-pipeline bug shipped
        // the wrong VERSION content alongside otherwise-valid
        // bytes. Fail closed.
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", b"0.13.99\n"),
            ],
        );
        let resolver =
            RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".to_string());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        match err {
            RuntimeOverlayError::VersionMismatch { expected, found } => {
                assert_eq!(expected, "0.14.0");
                assert_eq!(found, "0.13.99");
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn resolve_fails_when_directory_path_for_expected_version_missing() {
        // The other half of the version-mismatch story: the
        // cache only contains a `0.13.99/` directory, the
        // resolver expects `0.14.0/`. Surfaces as
        // `ArtifactIncomplete` (the *expected* artifact_dir
        // doesn't exist), not `VersionMismatch`. Distinct from
        // the VERSION-file disagreement above.
        let cache = complete_cache("0.13.99", Arch::Aarch64);
        let resolver =
            RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".to_string());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::ArtifactIncomplete { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_fails_on_malformed_roothash_wrong_length() {
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", b"abc\n"),
                ("VERSION", b"0.14.0\n"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::InvalidRoothash { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_fails_on_malformed_roothash_uppercase() {
        let upper = "0123456789ABCDEF".repeat(4);
        assert_eq!(upper.len(), 64);
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", format!("{upper}\n").as_bytes()),
                ("VERSION", b"0.14.0\n"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::InvalidRoothash { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_fails_on_empty_version_file() {
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", b""),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::InvalidVersionFile { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_fails_on_whitespace_inside_version_file() {
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", format!("{FAKE_ROOTHASH}\n").as_bytes()),
                ("VERSION", b"0.14 0\n"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let err = resolver.resolve(Arch::Aarch64).unwrap_err();
        assert!(
            matches!(err, RuntimeOverlayError::InvalidVersionFile { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_tolerates_roothash_without_trailing_newline() {
        let cache = make_cache(
            "0.14.0",
            Arch::Aarch64,
            &[
                ("overlay.ext4", b"ext4"),
                ("overlay.verity", b"sidecar"),
                ("overlay.roothash", FAKE_ROOTHASH.as_bytes()),
                ("VERSION", b"0.14.0"),
            ],
        );
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let artifact = resolver.resolve(Arch::Aarch64).expect("resolve");
        assert_eq!(artifact.roothash, FAKE_ROOTHASH);
    }

    #[test]
    fn layout_via_resolver_matches_under_helper() {
        let resolver = RuntimeOverlayResolver::new(PathBuf::from("/cache"), "0.14.0".to_string());
        let direct = RuntimeOverlayLayout::under(Path::new("/cache"), "0.14.0", Arch::Aarch64);
        let via_resolver = resolver.layout(Arch::Aarch64);
        assert_eq!(direct, via_resolver);
    }

    #[test]
    fn resolve_works_for_x86_64_arch_too() {
        let cache = complete_cache("0.14.0", Arch::X86_64);
        let resolver = RuntimeOverlayResolver::new(cache.path().to_path_buf(), "0.14.0".into());
        let artifact = resolver.resolve(Arch::X86_64).expect("resolve");
        assert_eq!(artifact.arch, Arch::X86_64);
        assert!(artifact.overlay_ext4.to_string_lossy().contains("x86_64"));
    }
}
