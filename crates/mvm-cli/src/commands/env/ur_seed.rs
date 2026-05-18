//! Ur-seed (Stage -1) cache management for `mvmctl dev` (Plan 86 / ADR-054).
//!
//! The ur-seed is a minimal aarch64-/x86_64-linux rootfs that exists
//! only to bootstrap the libkrun builder VM when no contract-compliant
//! dev image is available locally. See `nix/ur-seed/flake.nix` for the
//! producer side; this module handles the host-side cache.
//!
//! Layout: `~/.cache/mvm/ur-seed/<arch>/{rootfs.ext4, manifest.json, cmdline.txt}`.
//!
//! Acquisition is opt-in (`mvmctl dev fetch-ur-seed`) or air-gapped
//! (`mvmctl dev import-ur-seed`). `mvmctl dev up` never auto-fetches.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::ui;

/// File names the ur-seed cache directory carries after install.
const ROOTFS_NAME: &str = "rootfs.ext4";
const MANIFEST_NAME: &str = "manifest.json";
const CMDLINE_NAME: &str = "cmdline.txt";

/// Required entries the contributor or release CI ships in the tarball.
const REQUIRED_ENTRIES: &[&str] = &[ROOTFS_NAME, MANIFEST_NAME, CMDLINE_NAME];

/// Host arch string used in cache paths + tarball names.
pub(in crate::commands) fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

/// Resolved per-arch cache directory under `~/.cache/mvm/ur-seed/<arch>/`.
pub(in crate::commands) fn cache_dir(arch: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/ur-seed/{arch}",
        mvm_core::config::mvm_cache_dir()
    ))
}

/// A populated ur-seed cache. All required files present + readable.
///
/// `manifest` and `cmdline` paths are not consumed today — Stage 0 uses
/// its own pinned cmdline (`STAGE0_BOOTSTRAP_CMDLINE`) and doesn't
/// re-parse the manifest. They're kept on the struct for the seed
/// contract upgrade (W4 follow-up) which will read the manifest to
/// pick the right init path per seed.
#[derive(Debug, Clone)]
pub(in crate::commands) struct UrSeedCache {
    pub dir: PathBuf,
    pub rootfs: PathBuf,
    #[allow(dead_code)]
    pub manifest: PathBuf,
    #[allow(dead_code)]
    pub cmdline: PathBuf,
}

/// Probe the on-disk cache. Returns `Some` only if every required
/// entry exists and is non-empty. A partial cache (e.g. from a crashed
/// extraction) returns `None` so the caller can re-install.
pub(in crate::commands) fn probe(arch: &str) -> Option<UrSeedCache> {
    let dir = cache_dir(arch);
    let rootfs = dir.join(ROOTFS_NAME);
    let manifest = dir.join(MANIFEST_NAME);
    let cmdline = dir.join(CMDLINE_NAME);
    for p in [&rootfs, &manifest, &cmdline] {
        match std::fs::metadata(p) {
            Ok(m) if m.is_file() && m.len() > 0 => {}
            _ => return None,
        }
    }
    Some(UrSeedCache {
        dir,
        rootfs,
        manifest,
        cmdline,
    })
}

/// `mvmctl dev fetch-ur-seed` — download + install from the release mirror.
///
/// Default mirror is the GitHub release for the binary's `CARGO_PKG_VERSION`.
/// Override via `--mirror <URL>` for air-gapped relays. This is the only
/// network-touching ur-seed call site; Stage 0 never invokes it.
pub(in crate::commands) fn cmd_dev_fetch_ur_seed(
    arch_arg: Option<&str>,
    mirror_arg: Option<&str>,
) -> Result<()> {
    let arch = arch_arg.unwrap_or_else(|| host_arch()).to_string();
    validate_arch(&arch)?;

    let version = env!("CARGO_PKG_VERSION");
    let (tarball_url, sha256_url) = resolve_mirror_urls(mirror_arg, version, &arch);

    ui::progress(&format!(
        "Fetching ur-seed v{version} for {arch} from {tarball_url}"
    ));

    let tarball_bytes = http_get(&tarball_url)
        .with_context(|| format!("downloading ur-seed tarball from {tarball_url}"))?;
    let sha256_text = http_get_text(&sha256_url)
        .with_context(|| format!("downloading ur-seed sha256 sidecar from {sha256_url}"))?;

    install_bytes(&arch, &tarball_bytes, sha256_text.trim()).context("installing ur-seed")?;

    ui::success(&format!(
        "Ur-seed v{version} ({arch}) installed at {}",
        cache_dir(&arch).display()
    ));
    Ok(())
}

/// `mvmctl dev import-ur-seed --from <path>` — install from a local
/// tarball file. The expected sidecar is `<from>.sha256` unless
/// `--sha256 <path>` overrides.
pub(in crate::commands) fn cmd_dev_import_ur_seed(
    from_path: &str,
    sha256_path_arg: Option<&str>,
) -> Result<()> {
    let from = Path::new(from_path);
    if !from.is_file() {
        anyhow::bail!("Ur-seed tarball not found at {from_path}");
    }

    let sha256_path: PathBuf = match sha256_path_arg {
        Some(p) => PathBuf::from(p),
        None => {
            let mut p = from.as_os_str().to_owned();
            p.push(".sha256");
            PathBuf::from(p)
        }
    };
    if !sha256_path.is_file() {
        anyhow::bail!(
            "Ur-seed sha256 sidecar not found at {}. Pass --sha256 <path> if it lives elsewhere.",
            sha256_path.display()
        );
    }

    let arch = arch_from_filename(from)
        .with_context(|| format!("could not infer arch from tarball name {from_path}"))?;

    let tarball_bytes = std::fs::read(from)
        .with_context(|| format!("reading tarball {}", from.display()))?;
    let sha256_text = std::fs::read_to_string(&sha256_path)
        .with_context(|| format!("reading sha256 sidecar {}", sha256_path.display()))?;

    ui::progress(&format!(
        "Importing ur-seed ({arch}) from {} ({} bytes)",
        from.display(),
        tarball_bytes.len()
    ));

    install_bytes(&arch, &tarball_bytes, sha256_text.trim()).context("installing ur-seed")?;

    ui::success(&format!(
        "Ur-seed ({arch}) installed at {}",
        cache_dir(&arch).display()
    ));
    Ok(())
}

/// Common install path used by both fetch and import: verify sha256,
/// extract under a staging dir, atomic rename into the live cache.
fn install_bytes(arch: &str, tarball: &[u8], expected_sha256_hex: &str) -> Result<()> {
    let expected = parse_sha256_sidecar(expected_sha256_hex)
        .context("parsing sha256 sidecar contents")?;
    let actual = Sha256::digest(tarball);
    if actual.as_slice() != expected.as_slice() {
        anyhow::bail!(
            "Ur-seed tarball sha256 mismatch.\n  expected: {}\n  got:      {}",
            hex::encode(expected),
            hex::encode(actual)
        );
    }

    let live_dir = cache_dir(arch);
    if let Some(parent) = live_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Staging dir alongside the live dir; renamed in atomically once
    // the extract validates. Matches Plan 77 W2 / W5 pattern.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = live_dir.with_file_name(format!(
        ".{}.ur-seed-{}-{nonce}",
        live_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| arch.to_string()),
        std::process::id()
    ));
    let _cleanup = StagingCleanup::new(&staging_dir);

    std::fs::create_dir_all(&staging_dir)
        .with_context(|| format!("creating staging dir {}", staging_dir.display()))?;

    extract_tarball_strict(tarball, &staging_dir).context("extracting tarball")?;

    // Atomic-replace the live dir with the staged dir. If a previous
    // install left a live dir, move it aside first so the rename is a
    // simple directory rename without overlay semantics.
    if live_dir.exists() {
        let evicted = live_dir.with_file_name(format!(
            ".{}.ur-seed-evicted-{nonce}",
            live_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        std::fs::rename(&live_dir, &evicted)
            .with_context(|| format!("evicting prior cache at {}", live_dir.display()))?;
        // Best-effort drop the evicted dir; a failure here doesn't
        // affect the live cache.
        let _ = std::fs::remove_dir_all(&evicted);
    }
    std::fs::rename(&staging_dir, &live_dir)
        .with_context(|| format!("promoting staging to {}", live_dir.display()))?;

    Ok(())
}

/// Extract a gz-compressed tar stream into `target`, accepting only the
/// REQUIRED_ENTRIES. Rejects unknown entries (defense in depth — the
/// tarball is signed-by-checksum but the contents are not), absolute
/// paths, parent-traversals, symlinks, and hardlinks.
fn extract_tarball_strict(tarball_bytes: &[u8], target: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(tarball_bytes));
    let mut archive = tar::Archive::new(gz);
    let mut found: std::collections::HashSet<&'static str> = std::collections::HashSet::new();

    for entry_res in archive.entries().context("opening tar entries")? {
        let mut entry = entry_res.context("reading tar entry header")?;
        let entry_type = entry.header().entry_type();
        let path = entry
            .path()
            .context("reading tar entry path")?
            .into_owned();
        if !entry_type.is_file() {
            // Skip directory entries (we create the target dir
            // ourselves); reject everything else.
            if entry_type.is_dir() {
                continue;
            }
            anyhow::bail!(
                "ur-seed tarball entry {} has disallowed type {:?}",
                path.display(),
                entry_type
            );
        }

        // Normalize: strip leading "./", reject absolute + parent-traversal.
        let rel_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("ur-seed tar entry has non-UTF-8 path"))?;
        let rel_clean = rel_str.trim_start_matches("./");
        if rel_clean.is_empty()
            || rel_clean.starts_with('/')
            || rel_clean.split('/').any(|c| c == ".." || c.is_empty())
        {
            anyhow::bail!("ur-seed tarball entry has unsafe path {rel_str:?}");
        }

        let matched = REQUIRED_ENTRIES
            .iter()
            .copied()
            .find(|expected| *expected == rel_clean);
        let expected_name = match matched {
            Some(n) => n,
            None => anyhow::bail!(
                "ur-seed tarball contains unexpected entry {rel_clean:?}; \
                 only {REQUIRED_ENTRIES:?} are allowed"
            ),
        };

        let out_path = target.join(expected_name);
        let mut out_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out_file)
            .with_context(|| format!("writing {}", out_path.display()))?;
        found.insert(expected_name);
    }

    for expected in REQUIRED_ENTRIES {
        if !found.contains(*expected) {
            anyhow::bail!("ur-seed tarball missing required entry {expected:?}");
        }
    }
    Ok(())
}

/// Best-effort cleanup of a staging dir when install bails midway.
struct StagingCleanup<'a> {
    dir: &'a Path,
    armed: bool,
}

impl<'a> StagingCleanup<'a> {
    fn new(dir: &'a Path) -> Self {
        Self { dir, armed: true }
    }
}

impl Drop for StagingCleanup<'_> {
    fn drop(&mut self) {
        if self.armed && self.dir.exists() {
            let _ = std::fs::remove_dir_all(self.dir);
        }
    }
}

fn validate_arch(arch: &str) -> Result<()> {
    match arch {
        "aarch64" | "x86_64" => Ok(()),
        other => anyhow::bail!(
            "unsupported ur-seed arch {other:?}; allowed values are aarch64 and x86_64"
        ),
    }
}

fn arch_from_filename(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("tarball path has no UTF-8 file name"))?;
    // Canonical name is `ur-seed-<arch>-linux.tar.gz`; tolerate the
    // bare `ur-seed-<arch>.tar.gz` form too.
    for arch in ["aarch64", "x86_64"] {
        if name.contains(arch) {
            return Ok(arch.to_string());
        }
    }
    anyhow::bail!(
        "cannot infer arch from filename {name:?}; expected substring 'aarch64' or 'x86_64'"
    )
}

fn resolve_mirror_urls(mirror_arg: Option<&str>, version: &str, arch: &str) -> (String, String) {
    let base = mirror_arg
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| {
            format!("https://github.com/tinylabscom/mvm/releases/download/v{version}")
        });
    let tarball = format!("{base}/ur-seed-{arch}-linux.tar.gz");
    let sha256 = format!("{tarball}.sha256");
    (tarball, sha256)
}

fn parse_sha256_sidecar(text: &str) -> Result<[u8; 32]> {
    // Accept either the bare hex (from `sha256sum | awk '{print $1}'`)
    // or the `<hex>  <filename>` form (from raw `sha256sum`).
    let hex = text.split_whitespace().next().unwrap_or("");
    if hex.len() != 64 {
        anyhow::bail!("sha256 sidecar must be a 64-character hex string; got {hex:?}");
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex, &mut out)
        .with_context(|| format!("decoding sha256 hex {hex:?}"))?;
    Ok(out)
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::blocking::get(url)
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned non-success status"))?;
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading response body from {url}"))?;
    Ok(bytes.to_vec())
}

fn http_get_text(url: &str) -> Result<String> {
    let bytes = http_get(url)?;
    String::from_utf8(bytes).with_context(|| format!("decoding response from {url} as UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let buf = Vec::new();
        let gz = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn parse_sha256_sidecar_accepts_bare_hex() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let bytes = parse_sha256_sidecar(hex).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn parse_sha256_sidecar_accepts_sha256sum_format() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let line = format!("{hex}  ur-seed-aarch64-linux.tar.gz");
        let bytes = parse_sha256_sidecar(&line).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn parse_sha256_sidecar_rejects_wrong_length() {
        assert!(parse_sha256_sidecar("deadbeef").is_err());
    }

    #[test]
    fn arch_from_filename_recognizes_both_arches() {
        assert_eq!(
            arch_from_filename(Path::new("ur-seed-aarch64-linux.tar.gz")).unwrap(),
            "aarch64"
        );
        assert_eq!(
            arch_from_filename(Path::new("ur-seed-x86_64-linux.tar.gz")).unwrap(),
            "x86_64"
        );
    }

    #[test]
    fn extract_strict_accepts_required_entries() {
        let tarball = make_tarball(&[
            ("rootfs.ext4", b"rootfs-bytes"),
            ("manifest.json", b"{}"),
            ("cmdline.txt", b"console=hvc0"),
        ]);
        let target = tempfile::tempdir().unwrap();
        extract_tarball_strict(&tarball, target.path()).unwrap();
        for entry in REQUIRED_ENTRIES {
            assert!(target.path().join(entry).is_file(), "missing {entry}");
        }
    }

    #[test]
    fn extract_strict_rejects_unknown_entries() {
        let tarball = make_tarball(&[
            ("rootfs.ext4", b"x"),
            ("manifest.json", b"x"),
            ("cmdline.txt", b"x"),
            ("evil.sh", b"#!/bin/sh\n"),
        ]);
        let target = tempfile::tempdir().unwrap();
        let err = extract_tarball_strict(&tarball, target.path()).unwrap_err();
        assert!(err.to_string().contains("unexpected entry"), "got: {err}");
    }

    #[test]
    fn extract_strict_rejects_missing_required() {
        let tarball = make_tarball(&[
            ("rootfs.ext4", b"x"),
            ("manifest.json", b"x"),
            // cmdline.txt missing
        ]);
        let target = tempfile::tempdir().unwrap();
        let err = extract_tarball_strict(&tarball, target.path()).unwrap_err();
        assert!(err.to_string().contains("missing required entry"), "got: {err}");
    }

    #[test]
    fn extract_strict_rejects_path_traversal() {
        let tarball = make_tarball(&[
            ("rootfs.ext4", b"x"),
            ("manifest.json", b"x"),
            ("cmdline.txt", b"x"),
            ("../escape.sh", b"x"),
        ]);
        let target = tempfile::tempdir().unwrap();
        let err = extract_tarball_strict(&tarball, target.path()).unwrap_err();
        assert!(err.to_string().contains("unsafe path") || err.to_string().contains("unexpected entry"), "got: {err}");
    }
}
