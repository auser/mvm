//! `mvmctl overlay` — user-facing CLI for the runtime overlay disk
//! (ADR-051 / plan 74 W1.4b).
//!
//! v1 ships one verb: `fetch`. It downloads the per-arch overlay
//! artifact published by the `runtime-overlay-image` job in
//! `.github/workflows/release.yml`, hash-verifies against the
//! checksums manifest, and atomically installs into
//! `~/.cache/mvm/runtime-overlay/<version>/<arch>/`.
//!
//! Defaults are aligned so a user typing `mvmctl overlay fetch` with
//! no flags does the right thing — picks `Arch::host()`, picks the
//! running mvmctl's semver via `CARGO_PKG_VERSION`, and writes to
//! the standard cache directory. The flags exist for tests, private
//! mirrors (via `MVM_OVERLAY_BASE_URL`), and unusual setups (e.g.
//! pre-fetching the `x86_64` overlay on an aarch64 host for
//! sneakernet to a Linux box).
//!
//! Future verbs (separate slices, intentionally deferred):
//!
//! - `mvmctl overlay status` — show what's cached for the current
//!   version + arch.
//! - `mvmctl overlay clean` — remove stale overlays from older
//!   mvmctl versions (`~/.cache/mvm/runtime-overlay/<old>/`).
//! - Auto-fetch on first `mvmctl up` cache miss — needs to integrate
//!   with the start path; a small but cross-cutting follow-up.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use std::path::PathBuf;

use mvm_build::runtime_overlay::{Arch, download_runtime_overlay};
use mvm_core::user_config::MvmConfig;

use super::Cli;
use crate::ui;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: OverlayAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum OverlayAction {
    /// Download + hash-verify the runtime overlay artifact and
    /// install it into the local cache.
    ///
    /// With no flags, fetches the artifact for the running mvmctl's
    /// version + host arch into `~/.cache/mvm/runtime-overlay/`.
    /// The release-side artifact names carry an arch suffix
    /// (`runtime-overlay-<arch>.{ext4,verity,roothash,VERSION}`);
    /// the install renames them to the canonical
    /// `overlay.{ext4,verity,roothash}` + `VERSION` layout the
    /// `RuntimeOverlayResolver` reads at boot.
    ///
    /// Honors `MVM_OVERLAY_BASE_URL` for private mirrors and
    /// `MVM_SKIP_HASH_VERIFY=1` for emergency rotation (the latter
    /// is documented as the ADR-002 §W5.1 escape — never set in CI).
    Fetch(FetchArgs),
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct FetchArgs {
    /// Architecture to fetch ("aarch64" or "x86_64"). Defaults to
    /// the host arch. Override only when pre-fetching for a
    /// different machine (e.g. sneakernet to a Linux box).
    #[arg(long)]
    pub arch: Option<String>,

    /// mvmctl version to fetch the overlay for. Defaults to the
    /// running binary's compiled-in version
    /// (`env!("CARGO_PKG_VERSION")`). Override when populating
    /// a cache for a *different* mvmctl version — only useful
    /// during version rollover.
    #[arg(long)]
    pub version: Option<String>,

    /// Override the cache root. Defaults to
    /// `mvm_core::config::mvm_cache_dir()`
    /// (`~/.cache/mvm/` honoring `XDG_CACHE_HOME` and
    /// `MVM_CACHE_DIR`). Useful for tests and for operators who
    /// want overlays staged outside the home cache.
    #[arg(long)]
    pub cache_root: Option<PathBuf>,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.action {
        OverlayAction::Fetch(f) => fetch(f),
    }
}

fn fetch(args: FetchArgs) -> Result<()> {
    let arch = resolve_arch(args.arch.as_deref())?;
    let version = args
        .version
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let cache_root = args
        .cache_root
        .unwrap_or_else(|| PathBuf::from(mvm_core::config::mvm_cache_dir()));

    ui::info(&format!(
        "Fetching runtime overlay for mvmctl v{version} ({arch}) into {}",
        cache_root.display()
    ));

    let artifact = download_runtime_overlay(&version, arch, &cache_root).with_context(|| {
        format!(
            "Failed to fetch runtime overlay for v{version} ({arch}). \
             Confirm a release exists at the expected URL — see \
             `MVM_OVERLAY_BASE_URL` for private mirrors."
        )
    })?;

    ui::success(&format!(
        "Runtime overlay installed at {}",
        artifact
            .overlay_ext4
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown)".to_string()),
    ));
    ui::info(&format!("  arch:     {}", artifact.arch));
    ui::info(&format!("  version:  {}", artifact.version));
    ui::info(&format!("  roothash: {}", artifact.roothash));
    Ok(())
}

/// Parse the optional `--arch` flag. `None` → host arch.
/// Accepts the same canonical strings `Arch::as_str` emits
/// (`"aarch64"`, `"x86_64"`).
fn resolve_arch(raw: Option<&str>) -> Result<Arch> {
    match raw {
        None => Ok(Arch::host()),
        Some("aarch64") => Ok(Arch::Aarch64),
        Some("x86_64") => Ok(Arch::X86_64),
        Some(other) => {
            anyhow::bail!("unsupported --arch '{other}': expected 'aarch64' or 'x86_64'")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_arch_defaults_to_host() {
        // The host arch is one of the two supported variants —
        // assert it's one of those, not a specific value (the
        // test runs on whichever runner built mvmctl).
        let a = resolve_arch(None).unwrap();
        assert!(matches!(a, Arch::Aarch64 | Arch::X86_64));
    }

    #[test]
    fn resolve_arch_accepts_aarch64() {
        assert_eq!(resolve_arch(Some("aarch64")).unwrap(), Arch::Aarch64);
    }

    #[test]
    fn resolve_arch_accepts_x86_64() {
        assert_eq!(resolve_arch(Some("x86_64")).unwrap(), Arch::X86_64);
    }

    #[test]
    fn resolve_arch_rejects_unknown_value() {
        let err = resolve_arch(Some("riscv64")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported --arch"), "msg was: {msg}");
        assert!(msg.contains("riscv64"), "msg was: {msg}");
    }

    #[test]
    fn resolve_arch_rejects_empty_string() {
        let err = resolve_arch(Some("")).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported"));
    }

    /// End-to-end smoke test of the `fetch` handler against the
    /// same `file://` fixture pattern the `mvm-build` integration
    /// test uses. Stages the artifacts on disk, points
    /// `MVM_OVERLAY_BASE_URL` at the fixture dir, and asserts the
    /// command succeeds + the cache is populated. Exercises the
    /// full glue layer from `FetchArgs` through
    /// `download_runtime_overlay`.
    #[test]
    fn fetch_handler_against_file_url_fixture_populates_cache() {
        // Serialize against any other env-touching tests in the
        // same process to avoid races on `MVM_OVERLAY_BASE_URL`.
        let _g = env_test_mutex().lock().unwrap();

        let upstream = tempfile::tempdir().unwrap();
        let release_dir = upstream.path().join("v9.9.9");
        std::fs::create_dir_all(&release_dir).unwrap();

        let arch = Arch::host();
        let ext4 = b"fake-ext4";
        let verity = b"fake-verity";
        let roothash_text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        let version_text = "9.9.9\n";
        let names = mvm_build::runtime_overlay::RuntimeOverlayArtifactNames::for_arch(arch);

        std::fs::write(release_dir.join(&names.ext4), ext4).unwrap();
        std::fs::write(release_dir.join(&names.verity), verity).unwrap();
        std::fs::write(release_dir.join(&names.roothash), roothash_text).unwrap();
        std::fs::write(release_dir.join(&names.version), version_text).unwrap();

        let checksums = format!(
            "{}  {}\n{}  {}\n{}  {}\n{}  {}\n",
            sha256_hex(ext4),
            names.ext4,
            sha256_hex(verity),
            names.verity,
            sha256_hex(roothash_text.as_bytes()),
            names.roothash,
            sha256_hex(version_text.as_bytes()),
            names.version,
        );
        std::fs::write(release_dir.join(&names.checksums), checksums.as_bytes()).unwrap();

        let cache = tempfile::tempdir().unwrap();
        let base_url = format!("file://{}", upstream.path().display());
        // SAFETY: protected by env_test_mutex.
        unsafe {
            std::env::set_var("MVM_OVERLAY_BASE_URL", &base_url);
        }

        let result = fetch(FetchArgs {
            arch: Some(arch.as_str().to_string()),
            version: Some("9.9.9".to_string()),
            cache_root: Some(cache.path().to_path_buf()),
        });

        unsafe {
            std::env::remove_var("MVM_OVERLAY_BASE_URL");
        }

        result.expect("fetch handler must succeed against fixture");
        let installed_dir = cache
            .path()
            .join("runtime-overlay")
            .join("9.9.9")
            .join(arch.as_str());
        assert!(installed_dir.join("overlay.ext4").is_file());
        assert!(installed_dir.join("overlay.verity").is_file());
        assert!(installed_dir.join("overlay.roothash").is_file());
        assert!(installed_dir.join("VERSION").is_file());
    }

    fn env_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }
}
