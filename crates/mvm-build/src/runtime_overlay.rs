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
//! ## Two ways to land an artifact in the cache
//!
//! 1. **Build from the flake.** [`build_overlay_with_nix`]
//!    shells out to `nix build` against
//!    `<workspace>/nix/images/runtime-overlay/` (W1.4b.2's flake)
//!    and returns paths into the nix store. Linux-only — `nix`
//!    is unavailable on the macOS host per CLAUDE.md's "Host
//!    Nix is never used by mvmctl" rule, so the function gates
//!    on `target_os = "linux"`. The macOS path runs through the
//!    libkrun builder VM (W1.4b.3b's wiring).
//! 2. **Download from a release.** W1.4b.4 wires the
//!    artifact-acquisition path (similar to how
//!    `download_dev_image` works for the dev VM image).
//!
//! ## Out of scope (this PR)
//!
//! - **Attaching** the overlay to a microVM at boot. W1.4b.3b
//!   lands the backend + `mvm-verity-init` extensions.
//! - **Routing macOS calls** through the libkrun builder VM.
//!   W1.4b.3b also wires this (same VM the builder-vm flake
//!   runs in today).
//! - **`mkGuest` refactor** to drop the agent / shim / runner
//!   from per-image closures. W1.4b.3c.
//!
//! The resolver and the build-spec construction are pure file
//! I/O + string parsing; only [`build_overlay_with_nix`] gates
//! on Linux.

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

    /// `nix build` exited non-zero or couldn't be spawned. Plan
    /// 74 W1.4b.3a — the orchestrator that drives `nix build`
    /// against the runtime-overlay flake. Includes the upstream
    /// stderr so failures are debuggable without re-running with
    /// `--verbose`.
    #[error("nix build failed: {reason}")]
    NixBuildFailed { reason: String },

    /// The runtime-overlay operation is unsupported on this
    /// host. `nix build` runs Linux-only; macOS callers route
    /// through the libkrun builder VM (W1.4b.3b's wiring).
    #[error("host does not support {operation}: {reason}")]
    HostUnsupported {
        operation: &'static str,
        reason: &'static str,
    },

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

// =================================================================
// Build orchestrator (W1.4b.3a)
// =================================================================

/// Spec for `nix build` of the runtime-overlay flake at
/// `<workspace>/nix/images/runtime-overlay/`. Pure data; the
/// actual invocation lives in [`build_overlay_with_nix`].
///
/// The spec exposes its argv + env separately so callers that
/// drive `nix build` *inside* the libkrun builder VM (rather
/// than on the host) can plumb the same shape through without
/// reaching for an external command.
#[derive(Debug, Clone)]
pub struct OverlayBuildSpec {
    /// Workspace root — the dir containing `nix/`, `crates/`,
    /// `Cargo.toml`. The flake reads
    /// `<workspace_root>/nix/images/runtime-overlay/flake.nix`.
    pub workspace_root: PathBuf,
    /// Which target arch to build for. Maps to the Nix
    /// `system` attribute on the flake's `packages` output.
    pub arch: Arch,
    /// Where the resulting result-symlink should live. Typically
    /// a tempdir or a staging location under
    /// `~/.cache/mvm/runtime-overlay/<version>/<arch>/.work/` —
    /// the install-to-cache step is the caller's responsibility.
    pub out_link: PathBuf,
    /// Override the `nix` binary location. Default `None` ⇒
    /// resolved via `$PATH`. Tests use this to substitute a stub.
    pub nix_binary: Option<PathBuf>,
}

impl OverlayBuildSpec {
    /// Construct a spec for the given (workspace, arch, out_link).
    pub fn new(workspace_root: PathBuf, arch: Arch, out_link: PathBuf) -> Self {
        Self {
            workspace_root,
            arch,
            out_link,
            nix_binary: None,
        }
    }

    /// Nix `system` attribute string corresponding to `self.arch`.
    /// The runtime-overlay flake exposes outputs at
    /// `packages.<system>.default` for these two systems.
    pub fn system(&self) -> &'static str {
        match self.arch {
            Arch::Aarch64 => "aarch64-linux",
            Arch::X86_64 => "x86_64-linux",
        }
    }

    /// Absolute path to the flake directory (the dir containing
    /// `flake.nix`). Nix's `path:` URI scheme consumes the dir,
    /// not the `flake.nix` file.
    pub fn flake_path(&self) -> PathBuf {
        self.workspace_root
            .join("nix")
            .join("images")
            .join("runtime-overlay")
    }

    /// The Nix flake reference used by `nix build`. Pinned to
    /// the workspace-local path so we don't accidentally fetch
    /// a published flake when building from source.
    pub fn flake_reference(&self) -> String {
        format!(
            "path:{}#packages.{}.default",
            self.flake_path().display(),
            self.system()
        )
    }

    /// `nix build` argv, ready to hand to `Command::new` plus
    /// `.args(argv[1..])`. The orchestrator manages its own
    /// symlink position via `--out-link <path>`.
    pub fn argv(&self) -> Vec<String> {
        let nix = self
            .nix_binary
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "nix".to_string());
        vec![
            nix,
            "build".to_string(),
            "--extra-experimental-features".to_string(),
            "nix-command flakes".to_string(),
            "--out-link".to_string(),
            self.out_link.display().to_string(),
            self.flake_reference(),
        ]
    }

    /// Environment variables to thread through to `nix build`.
    /// `MVM_WORKSPACE_PATH` is the override the flake reads when
    /// running inside the libkrun-builder VM's sandbox where
    /// `..` resolution against the store copy doesn't reach the
    /// workspace — same mechanism the builder-vm flake uses.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![(
            "MVM_WORKSPACE_PATH".to_string(),
            self.workspace_root.display().to_string(),
        )]
    }
}

/// Drive `nix build` from a spec. Linux-only at runtime —
/// CLAUDE.md forbids host nix on macOS, and even if the binary
/// is installed it can't cross-compile to `aarch64-linux` /
/// `x86_64-linux` without a remote builder. Non-Linux callers
/// get `HostUnsupported`; W1.4b.3b routes those calls through
/// the libkrun builder VM.
///
/// On success the function:
///
/// 1. Verifies the four required files exist at
///    `<out_link>/{overlay.ext4, overlay.verity, overlay.roothash,
///    VERSION}`. The runtime-overlay flake's `runCommand`
///    produces exactly these names.
/// 2. Reads `VERSION` + `overlay.roothash` and validates them.
/// 3. Returns a [`RuntimeOverlayArtifact`] pointing at the
///    nix-store paths the result-symlink resolves to.
pub fn build_overlay_with_nix(
    spec: &OverlayBuildSpec,
) -> Result<RuntimeOverlayArtifact, RuntimeOverlayError> {
    #[cfg(target_os = "linux")]
    {
        run_nix_build(spec)?;
        validate_built_artifact(spec)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Suppress "unused" on non-Linux while keeping a single
        // public signature across hosts.
        let _ = spec;
        Err(RuntimeOverlayError::HostUnsupported {
            operation: "runtime-overlay nix build",
            reason: "nix build runs Linux-only; non-Linux callers route through the libkrun builder VM (W1.4b.3b)",
        })
    }
}

#[cfg(target_os = "linux")]
fn run_nix_build(spec: &OverlayBuildSpec) -> Result<(), RuntimeOverlayError> {
    let argv = spec.argv();
    let binary = argv.first().cloned().unwrap_or_else(|| "nix".to_string());
    let mut cmd = std::process::Command::new(&binary);
    cmd.args(&argv[1..]);
    for (k, v) in spec.env() {
        cmd.env(k, v);
    }
    let exec = cmd
        .output()
        .map_err(|e| RuntimeOverlayError::NixBuildFailed {
            reason: format!("spawn `{binary}`: {e}"),
        })?;
    if !exec.status.success() {
        let stderr = String::from_utf8_lossy(&exec.stderr).into_owned();
        return Err(RuntimeOverlayError::NixBuildFailed {
            reason: format!("exit {:?}; stderr={stderr}", exec.status.code()),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_built_artifact(
    spec: &OverlayBuildSpec,
) -> Result<RuntimeOverlayArtifact, RuntimeOverlayError> {
    let dir = &spec.out_link;
    let overlay_ext4 = dir.join("overlay.ext4");
    let sidecar = dir.join("overlay.verity");
    let roothash_file = dir.join("overlay.roothash");
    let version_file = dir.join("VERSION");
    for required in [&overlay_ext4, &sidecar, &roothash_file, &version_file] {
        if !required.is_file() {
            return Err(RuntimeOverlayError::ArtifactIncomplete {
                artifact_dir: dir.to_path_buf(),
                missing: required.clone(),
                version: read_version_file(&version_file)
                    .unwrap_or_else(|_| "<unreadable>".to_string()),
                arch: spec.arch.to_string(),
            });
        }
    }
    let version = read_version_file(&version_file)?;
    let roothash = read_and_validate_roothash(&roothash_file)?;
    Ok(RuntimeOverlayArtifact {
        overlay_ext4,
        sidecar,
        roothash_file,
        roothash,
        arch: spec.arch,
        version,
    })
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

    // =================================================================
    // Build-spec tests (W1.4b.3a)
    // =================================================================

    #[test]
    fn build_spec_system_maps_arch_to_nix_system_string() {
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::Aarch64,
            PathBuf::from("/tmp/result"),
        );
        assert_eq!(spec.system(), "aarch64-linux");

        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::X86_64,
            PathBuf::from("/tmp/result"),
        );
        assert_eq!(spec.system(), "x86_64-linux");
    }

    #[test]
    fn build_spec_flake_path_points_at_runtime_overlay_dir() {
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::Aarch64,
            PathBuf::from("/tmp/result"),
        );
        assert_eq!(
            spec.flake_path(),
            PathBuf::from("/workspace/nix/images/runtime-overlay")
        );
    }

    #[test]
    fn build_spec_flake_reference_pins_path_uri_and_system_default() {
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::X86_64,
            PathBuf::from("/tmp/result"),
        );
        // Pinned to the workspace-local path so we don't fetch
        // a published flake on the rare host where nix is happy
        // to resolve a bare attribute against `nixpkgs`.
        assert_eq!(
            spec.flake_reference(),
            "path:/workspace/nix/images/runtime-overlay#packages.x86_64-linux.default"
        );
    }

    #[test]
    fn build_spec_argv_defaults_to_path_lookup_nix_and_includes_required_flags() {
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::Aarch64,
            PathBuf::from("/tmp/result"),
        );
        let argv = spec.argv();
        // Default nix binary is `nix` (resolved via $PATH).
        assert_eq!(argv[0], "nix");
        assert_eq!(argv[1], "build");
        // Experimental features enable nix-command + flakes
        // without requiring a contributor's `~/.config/nix/nix.conf`.
        let pair = argv
            .windows(2)
            .find(|w| w[0] == "--extra-experimental-features");
        assert!(
            pair.is_some(),
            "argv must enable nix-command + flakes: {argv:?}"
        );
        assert_eq!(pair.unwrap()[1], "nix-command flakes");
        // --out-link <path>
        let out = argv.windows(2).find(|w| w[0] == "--out-link");
        assert!(out.is_some(), "argv must specify --out-link: {argv:?}");
        assert_eq!(out.unwrap()[1], "/tmp/result");
        // Final positional is the flake reference.
        assert_eq!(
            argv.last().unwrap(),
            "path:/workspace/nix/images/runtime-overlay#packages.aarch64-linux.default"
        );
    }

    #[test]
    fn build_spec_argv_respects_nix_binary_override() {
        let spec = OverlayBuildSpec {
            workspace_root: PathBuf::from("/workspace"),
            arch: Arch::Aarch64,
            out_link: PathBuf::from("/tmp/result"),
            nix_binary: Some(PathBuf::from("/custom/nix-stub")),
        };
        let argv = spec.argv();
        assert_eq!(argv[0], "/custom/nix-stub");
    }

    #[test]
    fn build_spec_env_sets_workspace_path_for_sandbox_resolution() {
        // `MVM_WORKSPACE_PATH` is the env override the
        // runtime-overlay flake reads so the `..` resolution
        // against a store copy lands at the right tree when nix
        // runs inside the libkrun-builder VM sandbox.
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::Aarch64,
            PathBuf::from("/tmp/result"),
        );
        let env = spec.env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "MVM_WORKSPACE_PATH");
        assert_eq!(env[0].1, "/workspace");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn build_overlay_with_nix_returns_host_unsupported_on_non_linux() {
        let spec = OverlayBuildSpec::new(
            PathBuf::from("/workspace"),
            Arch::Aarch64,
            PathBuf::from("/tmp/result"),
        );
        let err = build_overlay_with_nix(&spec).unwrap_err();
        match err {
            RuntimeOverlayError::HostUnsupported { operation, .. } => {
                assert!(
                    operation.contains("runtime-overlay") || operation.contains("nix"),
                    "expected runtime-overlay or nix in operation; got {operation:?}"
                );
            }
            other => panic!("expected HostUnsupported, got {other:?}"),
        }
    }
}
