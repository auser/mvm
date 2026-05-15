//! `mvmctl overlay` — user-facing CLI for the runtime overlay disk
//! (ADR-051 / plan 74 W1.4b).
//!
//! Verbs:
//!
//! - **`fetch`** — downloads the per-arch overlay artifact
//!   published by the `runtime-overlay-image` job in
//!   `.github/workflows/release.yml`, hash-verifies against the
//!   checksums manifest, and atomically installs into
//!   `~/.cache/mvm/runtime-overlay/<version>/<arch>/`.
//! - **`status`** — read-only inspection of the cache: reports
//!   whether the running mvmctl's overlay is present, missing,
//!   version-mismatched, or invalid. Supports `--json` for
//!   scripting and `--cache-root`/`--version`/`--arch` for the
//!   same testability story as `fetch`.
//!
//! Defaults are aligned so flag-less invocations Do The Right
//! Thing — `Arch::host()`, the running mvmctl's
//! `CARGO_PKG_VERSION`, and the standard cache directory. The
//! flags exist for tests, private mirrors (via
//! `MVM_OVERLAY_BASE_URL`), and unusual setups (e.g. pre-fetching
//! the `x86_64` overlay on an aarch64 host for sneakernet to a
//! Linux box).
//!
//! Future verbs (separate slices, intentionally deferred):
//!
//! - `mvmctl overlay clean` — remove stale overlays from older
//!   mvmctl versions (`~/.cache/mvm/runtime-overlay/<old>/`).
//! - Auto-fetch on first `mvmctl up` cache miss — needs to integrate
//!   with the start path; a small but cross-cutting follow-up.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};

use mvm_build::runtime_overlay::{
    Arch, RuntimeOverlayError, RuntimeOverlayLayout, RuntimeOverlayResolver,
    download_runtime_overlay,
};
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

    /// Show the cache state for the running mvmctl's runtime
    /// overlay. Reports one of: `present` (resolved successfully),
    /// `missing` (one or more files absent), `version-mismatch`
    /// (cache holds a different mvmctl version), or `invalid`
    /// (roothash or VERSION file malformed). Read-only — never
    /// mutates the cache.
    ///
    /// With `--json`, emits a stable machine-readable object so
    /// scripts (e.g. `mvmctl doctor`, CI gates) can branch on
    /// `.status` and surface the typed `paths` map.
    Status(StatusArgs),
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

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct StatusArgs {
    /// Architecture to query ("aarch64" or "x86_64"). Defaults to
    /// the host arch.
    #[arg(long)]
    pub arch: Option<String>,

    /// mvmctl version to query. Defaults to the running binary's
    /// compiled-in version.
    #[arg(long)]
    pub version: Option<String>,

    /// Override the cache root. Defaults to
    /// `mvm_core::config::mvm_cache_dir()`.
    #[arg(long)]
    pub cache_root: Option<PathBuf>,

    /// Emit a stable JSON object on stdout instead of the
    /// human-readable summary. Useful for `mvmctl doctor` and CI
    /// gates that want to branch on `.status`.
    #[arg(long)]
    pub json: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.action {
        OverlayAction::Fetch(f) => fetch(f),
        OverlayAction::Status(s) => status(s),
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

// ─── status ────────────────────────────────────────────────────────

/// Machine-readable status emitted on `--json` and used as the
/// internal model behind the human-readable printer. `Serialize`
/// is stable across mvmctl releases — downstream tooling can pin
/// on `status` + `paths.*`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StatusReport {
    /// One of `"present"`, `"missing"`, `"version-mismatch"`,
    /// `"invalid"`. Read this field to branch in a script.
    status: &'static str,
    /// The mvmctl version the resolver expected (i.e. what `fetch`
    /// would install for).
    expected_version: String,
    /// The arch the resolver queried.
    arch: String,
    /// The cache root the resolver walked.
    cache_root: PathBuf,
    /// The artifact directory the resolver expected (whether or
    /// not it exists).
    artifact_dir: PathBuf,
    /// Per-file presence + size in bytes (when present). Keys are
    /// the canonical names (`overlay.ext4`, `overlay.verity`,
    /// `overlay.roothash`, `VERSION`). `present == false`
    /// rows still appear so consumers see the full expected
    /// layout.
    paths: Vec<FileEntry>,
    /// Populated only when `status == "present"` — the 64-hex
    /// roothash that `mvm-verity-init` reads from the cmdline.
    #[serde(skip_serializing_if = "Option::is_none")]
    roothash: Option<String>,
    /// Populated only when `status == "version-mismatch"` —
    /// what the cache's `VERSION` file actually said.
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_version: Option<String>,
    /// One-line human-readable reason; populated for non-`present`
    /// statuses to surface what went wrong without re-running
    /// resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FileEntry {
    name: &'static str,
    path: PathBuf,
    present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

fn status(args: StatusArgs) -> Result<()> {
    let arch = resolve_arch(args.arch.as_deref())?;
    let version = args
        .version
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let cache_root = args
        .cache_root
        .unwrap_or_else(|| PathBuf::from(mvm_core::config::mvm_cache_dir()));

    let report = build_status_report(&cache_root, &version, arch);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status_report(&report);
    }
    Ok(())
}

/// Pure builder over the resolver's `Result`. Splitting it out
/// keeps `status` thin and makes the test harness an easy
/// `assert_eq!` against a constructed `StatusReport`.
fn build_status_report(cache_root: &Path, version: &str, arch: Arch) -> StatusReport {
    let resolver = RuntimeOverlayResolver::new(cache_root.to_path_buf(), version.to_string());
    let layout = resolver.layout(arch);
    let paths = describe_paths(&layout);

    match resolver.resolve(arch) {
        Ok(artifact) => StatusReport {
            status: "present",
            expected_version: version.to_string(),
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: Some(artifact.roothash),
            cached_version: Some(artifact.version),
            detail: None,
        },
        Err(RuntimeOverlayError::ArtifactIncomplete { missing, .. }) => StatusReport {
            status: "missing",
            expected_version: version.to_string(),
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: None,
            cached_version: None,
            detail: Some(format!(
                "missing file: {} — run `mvmctl overlay fetch` to populate",
                missing.display()
            )),
        },
        Err(RuntimeOverlayError::VersionMismatch { expected, found }) => StatusReport {
            status: "version-mismatch",
            expected_version: expected,
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: None,
            cached_version: Some(found),
            detail: Some(
                "cache holds a different mvmctl version — \
                 `mvmctl overlay fetch` will install the correct one"
                    .to_string(),
            ),
        },
        Err(RuntimeOverlayError::InvalidRoothash { reason }) => StatusReport {
            status: "invalid",
            expected_version: version.to_string(),
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: None,
            cached_version: None,
            detail: Some(format!("malformed roothash: {reason}")),
        },
        Err(RuntimeOverlayError::InvalidVersionFile { reason }) => StatusReport {
            status: "invalid",
            expected_version: version.to_string(),
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: None,
            cached_version: None,
            detail: Some(format!("malformed VERSION file: {reason}")),
        },
        // The other RuntimeOverlayError variants
        // (NixBuildFailed, HostUnsupported, Io, DownloadFailed,
        // ChecksumMismatch, ChecksumMissing) don't surface from
        // `resolve` today, but a future refactor could fold one
        // in. Map them to `"invalid"` with the verbatim error
        // text rather than panicking — operator clarity beats
        // exhaustiveness on a read-only command.
        Err(other) => StatusReport {
            status: "invalid",
            expected_version: version.to_string(),
            arch: arch.to_string(),
            cache_root: cache_root.to_path_buf(),
            artifact_dir: layout.artifact_dir,
            paths,
            roothash: None,
            cached_version: None,
            detail: Some(format!("resolve failed: {other}")),
        },
    }
}

fn describe_paths(layout: &RuntimeOverlayLayout) -> Vec<FileEntry> {
    [
        ("overlay.ext4", &layout.overlay_ext4),
        ("overlay.verity", &layout.sidecar),
        ("overlay.roothash", &layout.roothash_file),
        ("VERSION", &layout.version_file),
    ]
    .into_iter()
    .map(|(name, p)| {
        let meta = std::fs::metadata(p).ok();
        FileEntry {
            name,
            path: p.clone(),
            present: meta.as_ref().map(|m| m.is_file()).unwrap_or(false),
            size_bytes: meta.as_ref().and_then(|m| m.is_file().then_some(m.len())),
        }
    })
    .collect()
}

fn print_status_report(r: &StatusReport) {
    println!("mvm runtime overlay (ADR-051)");
    println!("  status:           {}", r.status);
    println!("  expected version: {}", r.expected_version);
    println!("  arch:             {}", r.arch);
    println!("  cache root:       {}", r.cache_root.display());
    println!("  artifact dir:     {}", r.artifact_dir.display());
    println!("  paths:");
    for entry in &r.paths {
        let marker = if entry.present { " " } else { "!" };
        let size = entry
            .size_bytes
            .map(|n| format!(" ({})", human_bytes(n)))
            .unwrap_or_default();
        println!(
            "    [{marker}] {:<16} {}{}",
            entry.name,
            entry.path.display(),
            size
        );
    }
    if let Some(h) = &r.roothash {
        println!("  roothash:         {}", h);
    }
    if let Some(v) = &r.cached_version
        && r.status == "version-mismatch"
    {
        println!("  cached version:   {}", v);
    }
    if let Some(d) = &r.detail {
        println!("  detail:           {}", d);
    }
}

fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

// ─── arch helper (shared between fetch + status) ──────────────────

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

    // ─── status tests ──────────────────────────────────────────────

    const VALID_ROOTHASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Stage a valid four-file overlay under
    /// `<cache>/runtime-overlay/<version>/<arch>/`. Returns the
    /// cache root TempDir so the caller can use its path.
    fn stage_valid_cache(version: &str, arch: Arch) -> tempfile::TempDir {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache
            .path()
            .join("runtime-overlay")
            .join(version)
            .join(arch.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("overlay.ext4"), b"fake-ext4-bytes").unwrap();
        std::fs::write(dir.join("overlay.verity"), b"fake-verity").unwrap();
        std::fs::write(dir.join("overlay.roothash"), format!("{VALID_ROOTHASH}\n")).unwrap();
        std::fs::write(dir.join("VERSION"), format!("{version}\n")).unwrap();
        cache
    }

    #[test]
    fn status_report_present_when_cache_is_complete_and_version_matches() {
        let cache = stage_valid_cache("9.9.9", Arch::Aarch64);
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        assert_eq!(r.status, "present");
        assert_eq!(r.expected_version, "9.9.9");
        assert_eq!(r.arch, "aarch64");
        assert_eq!(r.roothash.as_deref(), Some(VALID_ROOTHASH));
        assert_eq!(r.cached_version.as_deref(), Some("9.9.9"));
        assert!(r.detail.is_none());
        // Every path row marked present + has a size.
        for entry in &r.paths {
            assert!(entry.present, "{entry:?}");
            assert!(entry.size_bytes.is_some(), "{entry:?}");
        }
    }

    #[test]
    fn status_report_missing_when_cache_is_empty() {
        let cache = tempfile::tempdir().unwrap();
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        assert_eq!(r.status, "missing");
        assert!(r.detail.as_deref().unwrap().contains("missing file"));
        assert!(
            r.detail
                .as_deref()
                .unwrap()
                .contains("mvmctl overlay fetch")
        );
        assert!(r.roothash.is_none());
        // No file is present in the per-path map.
        for entry in &r.paths {
            assert!(!entry.present, "{entry:?}");
            assert!(entry.size_bytes.is_none(), "{entry:?}");
        }
    }

    #[test]
    fn status_report_missing_when_one_file_absent() {
        let cache = stage_valid_cache("9.9.9", Arch::Aarch64);
        // Remove `overlay.verity` to fail completeness.
        let dir = cache.path().join("runtime-overlay/9.9.9/aarch64");
        std::fs::remove_file(dir.join("overlay.verity")).unwrap();
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        assert_eq!(r.status, "missing");
        // The detail line should name the missing file by path.
        let detail = r.detail.unwrap();
        assert!(detail.contains("overlay.verity"), "detail was: {detail}");
        // Surviving siblings still report present.
        let ext4_entry = r.paths.iter().find(|p| p.name == "overlay.ext4").unwrap();
        assert!(ext4_entry.present);
        let verity_entry = r.paths.iter().find(|p| p.name == "overlay.verity").unwrap();
        assert!(!verity_entry.present);
    }

    #[test]
    fn status_report_version_mismatch_when_cache_has_different_version() {
        // Stage 0.13.0; query for 9.9.9.
        let cache = stage_valid_cache("0.13.0", Arch::Aarch64);
        // The resolver looks under the *expected* version path,
        // not whatever the cache has, so we need to re-stage at
        // the 9.9.9 path with a mismatching VERSION file.
        let dir = cache.path().join("runtime-overlay/9.9.9/aarch64");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("overlay.ext4"), b"x").unwrap();
        std::fs::write(dir.join("overlay.verity"), b"x").unwrap();
        std::fs::write(dir.join("overlay.roothash"), format!("{VALID_ROOTHASH}\n")).unwrap();
        std::fs::write(dir.join("VERSION"), b"0.13.0\n").unwrap();

        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        assert_eq!(r.status, "version-mismatch");
        assert_eq!(r.expected_version, "9.9.9");
        assert_eq!(r.cached_version.as_deref(), Some("0.13.0"));
        assert!(
            r.detail
                .as_deref()
                .unwrap()
                .contains("mvmctl overlay fetch")
        );
    }

    #[test]
    fn status_report_invalid_when_roothash_malformed() {
        let cache = stage_valid_cache("9.9.9", Arch::Aarch64);
        // Corrupt the roothash with non-hex content.
        let dir = cache.path().join("runtime-overlay/9.9.9/aarch64");
        std::fs::write(dir.join("overlay.roothash"), b"not-a-hash\n").unwrap();
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        assert_eq!(r.status, "invalid");
        assert!(
            r.detail.as_deref().unwrap().contains("roothash"),
            "detail: {:?}",
            r.detail
        );
    }

    #[test]
    fn status_report_serializes_to_stable_json_keys() {
        let cache = stage_valid_cache("9.9.9", Arch::Aarch64);
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        let json = serde_json::to_value(&r).unwrap();
        // Pin every load-bearing key. A downstream `mvmctl doctor`
        // PR can rename UI strings, but these keys are the
        // scripting contract.
        let obj = json.as_object().unwrap();
        for key in [
            "status",
            "expected_version",
            "arch",
            "cache_root",
            "artifact_dir",
            "paths",
            "roothash",
        ] {
            assert!(obj.contains_key(key), "missing key {key}");
        }
        assert_eq!(obj["status"], "present");
        let paths = obj["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 4);
        for p in paths {
            assert!(p["name"].is_string());
            assert!(p["path"].is_string());
            assert!(p["present"].is_boolean());
        }
    }

    #[test]
    fn status_report_omits_optional_fields_when_absent() {
        let cache = tempfile::tempdir().unwrap();
        let r = build_status_report(cache.path(), "9.9.9", Arch::Aarch64);
        let json = serde_json::to_value(&r).unwrap();
        let obj = json.as_object().unwrap();
        // `skip_serializing_if = "Option::is_none"` means the JSON
        // should not carry these keys for the missing case.
        assert!(!obj.contains_key("roothash"));
        assert!(!obj.contains_key("cached_version"));
        // `detail` is populated for non-present statuses.
        assert!(obj.contains_key("detail"));
    }

    #[test]
    fn human_bytes_formats_each_unit_band() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2 * 1024), "2.00 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.00 MiB");
        assert_eq!(human_bytes(4 * 1024 * 1024 * 1024), "4.00 GiB");
    }

    /// Status handler end-to-end against a valid cache (no
    /// network). Exercises `status(StatusArgs { json: true })`
    /// down through the JSON printer.
    #[test]
    fn status_handler_against_valid_cache_succeeds() {
        let cache = stage_valid_cache("9.9.9", Arch::Aarch64);
        status(StatusArgs {
            arch: Some("aarch64".to_string()),
            version: Some("9.9.9".to_string()),
            cache_root: Some(cache.path().to_path_buf()),
            json: true,
        })
        .expect("status handler must succeed against a valid cache");
    }

    #[test]
    fn status_handler_against_empty_cache_still_succeeds() {
        // `status` always returns Ok — the cache state is in the
        // report's `status` field, not in the function's
        // Result. This keeps `mvmctl overlay status` script-safe
        // (always exit 0; `jq '.status'` does the branching).
        let cache = tempfile::tempdir().unwrap();
        status(StatusArgs {
            arch: Some("x86_64".to_string()),
            version: Some("9.9.9".to_string()),
            cache_root: Some(cache.path().to_path_buf()),
            json: true,
        })
        .expect("empty cache reports missing, doesn't error");
    }
}
