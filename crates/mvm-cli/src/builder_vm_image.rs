//! Layer-1 builder VM image acquisition (plan 72 W5).
//!
//! Resolves the kernel + rootfs.ext4 + cmdline triple consumed by
//! [`mvm_build::libkrun_builder::LibkrunBuilderVm::with_image`]. Two
//! acquisition paths per ADR-046 §"Two artifact layers, two
//! acquisition paths":
//!
//!   1. **Source checkout** (`find_builder_vm_flake()` returns Ok):
//!      look in `${mvm_cache_dir}/builder-vm/<sha256-key>/` where
//!      the key hashes `nix/images/builder-vm/flake.nix` (+ its
//!      lock if present). On cache miss, the bootstrap-build path
//!      (Stage 0 — microsandbox + `nixos/nix:2.24.10` per plan 72
//!      W5) populates the cache. That bootstrap lives in a follow-on
//!      and gates on `--features contributor-bootstrap`; this module
//!      surfaces a clear error pointing there until it ships.
//!
//!   2. **Installed binary** (no source checkout): look in
//!      `${mvm_cache_dir}/builder-vm/v<mvmctl-version>/`. On miss,
//!      [`download_builder_vm_image`] fetches the four artifacts
//!      (vmlinux, rootfs.ext4, cmdline, checksums.txt) from the
//!      matching GitHub release and SHA-256 verifies per
//!      ADR-002 §W5.1. The `MVM_SKIP_HASH_VERIFY=1` env var is the
//!      documented emergency escape.
//!
//! ## Why this lives in mvm-cli, not mvm-build
//!
//! The trait-impl side (`LibkrunBuilderVm` in `mvm-build`) has to
//! stay narrow — it's the runtime VM driver. The image-acquisition
//! side has a much wider blast radius (filesystem layouts, version
//! strings, eventual HTTP, cache pruning) and shares that surface
//! with the existing dev-image / default-microvm acquisition logic
//! that already lives in `mvm-cli`. Keeping all three resolvers in
//! one crate means a future "unify the cache layout" refactor
//! touches one module.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[cfg(feature = "backends-builder-vm-libkrun")]
use mvm_build::libkrun_builder::BuilderVmImage;

#[cfg(feature = "backends-builder-vm-libkrun")]
use crate::http;

/// Locate the Layer-1 builder VM Nix flake in a source checkout.
///
/// Returns `Ok(path)` when `<workspace_root>/nix/images/builder-vm/flake.nix`
/// exists. The workspace root is derived from `CARGO_MANIFEST_DIR`,
/// which resolves to this crate's source dir at compile time:
///
///   - Source checkouts: `<repo>/crates/mvm-cli` → workspace = `<repo>`
///   - Installed binaries (cargo registry): no `nix/images/...` in
///     a registry tarball, so we return Err and the caller falls
///     through to the release-download path.
///
/// Mirrors `find_dev_image_flake` in `commands/env/apple_container.rs`
/// (the Layer-2 / dev-shell resolver) — same shape, different
/// directory. The two resolvers stay separate so plan 72 W5 can
/// rename `nix/images/builder/` → `nix/images/dev-shell/` without
/// touching this side.
pub fn find_builder_vm_flake() -> Result<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("cannot locate workspace root from {manifest_dir}"))?;
    let candidate = workspace_root
        .join("nix")
        .join("images")
        .join("builder-vm");
    if candidate.join("flake.nix").exists() {
        return Ok(candidate);
    }
    anyhow::bail!(
        "Builder VM flake not found. Expected at nix/images/builder-vm/flake.nix \
         (this is a source-checkout signal — installed mvmctl binaries hit the \
         release-download path instead)."
    )
}

/// Compute the source-checkout cache key for a builder VM flake.
///
/// Key = sha256(flake.nix bytes || `\0` || flake.lock bytes). The
/// NUL separator distinguishes (flake.nix = "AB", lock = "C") from
/// (flake.nix = "A", lock = "BC"). When `flake.lock` is absent
/// (first `nix build` hasn't happened yet) the key hashes only
/// flake.nix; on next run, the freshly-written lock file changes the
/// key, which forces a rebuild — that's the intended invariant per
/// plan 72 W5 ("any contributor edit to either invalidates").
pub fn cache_key_from_flake(flake_dir: &Path) -> Result<String> {
    let flake_nix = flake_dir.join("flake.nix");
    let flake_nix_bytes = std::fs::read(&flake_nix)
        .with_context(|| format!("reading {}", flake_nix.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&flake_nix_bytes);
    hasher.update([0u8]);
    let lock_path = flake_dir.join("flake.lock");
    if lock_path.exists() {
        let lock_bytes = std::fs::read(&lock_path)
            .with_context(|| format!("reading {}", lock_path.display()))?;
        hasher.update(&lock_bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Root directory for cached builder VM images. Mirrors
/// `mvm_build::libkrun_builder::cache_dir()` but exposes a `Result<PathBuf>`-free
/// helper for mvm-cli's `Result<...>` callers — both delegate to
/// `mvm_core::config::mvm_cache_dir()` for the precedence rule.
pub fn cache_root() -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_cache_dir()).join("builder-vm")
}

/// Cache subdirectory for a source-checkout build, keyed by the
/// flake's hash. Plan 72 W5 "if `~/.cache/mvm/builder-vm/<hash>/`
/// exists and is hash-valid → use it" — this is `<hash>`.
pub fn source_cache_dir(flake_dir: &Path) -> Result<PathBuf> {
    let key = cache_key_from_flake(flake_dir)?;
    Ok(cache_root().join(key))
}

/// Cache subdirectory for an installed-binary build, keyed by the
/// running mvmctl version. Plan 72 W5 "outside a source checkout:
/// download the mvm-published prebuilt for the running mvmctl
/// version".
pub fn installed_cache_dir() -> PathBuf {
    let version = env!("CARGO_PKG_VERSION");
    cache_root().join(format!("v{version}"))
}

/// Resolve the Layer-1 builder VM image, preferring (in order):
///   1. Source-checkout cache hit at `<cache>/builder-vm/<flake-hash>/`.
///   2. Installed-binary cache hit at `<cache>/builder-vm/v<version>/`.
///   3. Discoverable error pointing at the bootstrap/download
///      follow-on that fills those caches.
///
/// Plan 72 W5 turns step 3 into "kick off Stage 0 / download" — for
/// the in-flight scaffolding PR we surface the cache-miss path so
/// contributors can manually populate the cache for ad-hoc testing
/// (drop `vmlinux` + `rootfs.ext4` + `cmdline` from a fresh
/// `nix build` into the directory the error names).
#[cfg(feature = "backends-builder-vm-libkrun")]
pub fn ensure_builder_vm_image() -> Result<BuilderVmImage> {
    match find_builder_vm_flake() {
        Ok(flake_dir) => {
            let cache = source_cache_dir(&flake_dir)?;
            if cache.is_dir()
                && let Ok(img) = BuilderVmImage::load_from_dir(&cache)
            {
                return Ok(img);
            }

            // Stage 0 bootstrap — populate the cache by running
            // `nix build` inside a microsandbox VM. Only available when
            // built with `--features contributor-bootstrap` since that
            // pulls in the microsandbox dep closure (sqlx-sqlite,
            // microsandbox-runtime, etc.) that the default mvmctl
            // binary leaves out.
            #[cfg(feature = "contributor-bootstrap")]
            {
                build_builder_vm_image_via_microsandbox(&flake_dir, &cache).with_context(|| {
                    format!(
                        "Stage 0 microsandbox build of builder-vm flake at {} → {}",
                        flake_dir.display(),
                        cache.display(),
                    )
                })?;
                BuilderVmImage::load_from_dir(&cache).with_context(|| {
                    format!(
                        "loading freshly-built builder VM image from {}",
                        cache.display()
                    )
                })
            }

            #[cfg(not(feature = "contributor-bootstrap"))]
            anyhow::bail!(
                "Source-checkout cache miss for builder VM image at {cache_disp}\n\
                 Rebuild mvmctl with `--features contributor-bootstrap` to \
                 enable the Stage 0 microsandbox bootstrap that populates this \
                 cache. Or build the image manually:\n\
                 \n  \
                 nix build path:{flake_disp}#packages.{arch}-linux.default --out-link {cache_disp}/result\n\
                 \n\
                 then copy `result/vmlinux`, `result/rootfs.ext4`, and \
                 `result/cmdline` into {cache_disp}.",
                cache_disp = cache.display(),
                flake_disp = flake_dir.display(),
                arch = std::env::consts::ARCH,
            )
        }
        Err(_no_source) => {
            let cache = installed_cache_dir();
            if cache.is_dir()
                && let Ok(img) = BuilderVmImage::load_from_dir(&cache)
            {
                return Ok(img);
            }
            download_builder_vm_image(&cache).with_context(|| {
                format!(
                    "downloading builder VM image v{version} to {cache_disp}",
                    cache_disp = cache.display(),
                    version = env!("CARGO_PKG_VERSION"),
                )
            })?;
            BuilderVmImage::load_from_dir(&cache).with_context(|| {
                format!(
                    "loading downloaded builder VM image from {}",
                    cache.display()
                )
            })
        }
    }
}

// ──────────────────── Stage 0 microsandbox bootstrap ──────────────

/// Build the in-repo `nix/images/builder-vm/` flake inside a
/// microsandbox VM running `nixos/nix:2.24.10` (the same OCI image
/// the existing dev-image builder uses) and stash the artifacts under
/// `dest_dir`. The flake emits vmlinux + rootfs.ext4 + cmdline +
/// manifest.json; `MicrosandboxBuilderVm`'s copy_script extracts them
/// back to `dest_dir` over the bind-mounted `/out`.
///
/// This is plan 72 W5's Stage 0 — the contributor-bootstrap path that
/// lets a developer modify `nix/images/builder-vm/flake.nix` and see
/// their change in the next `mvmctl dev up` without a release-pipeline
/// round-trip (CLAUDE.md §"Source-checkout builds never depend on
/// mvm-published artifacts").
///
/// Why microsandbox + `nixos/nix:2.24.10` rather than host Nix:
/// CLAUDE.md §"Host Nix is never used by mvmctl" applies here too.
/// Every Nix evaluation goes through a VM we launched; this is the
/// only Stage 0 path. The 4 GiB microsandbox overlay limit (ADR-046
/// §"Open questions") applies but the builder-vm rootfs closure
/// fits — plan 72 W2 §"Image size budget" enforces ≤ 1.2 GiB
/// uncompressed at flake-build time.
#[cfg(feature = "contributor-bootstrap")]
pub fn build_builder_vm_image_via_microsandbox(
    flake_dir: &Path,
    dest_dir: &Path,
) -> Result<()> {
    use mvm_build::builder_vm::{
        BUILDER_GUEST_WORK_DIR, BuilderJob, BuilderMounts, BuilderVm, MicrosandboxBuilderVm,
        host_system_linux,
    };

    if !flake_dir.exists() {
        anyhow::bail!("builder-vm flake dir does not exist: {}", flake_dir.display());
    }
    std::fs::create_dir_all(dest_dir).with_context(|| {
        format!(
            "creating destination cache dir {} for builder VM image",
            dest_dir.display()
        )
    })?;

    // The builder-vm flake at `<workspace>/nix/images/builder-vm/`
    // uses `builtins.path { path = ../../..; }` to capture the
    // workspace root (for mkGuest's `mvmSrc` + `Cargo.lock`). Mount
    // the whole workspace at /work and point `flake_ref` at the
    // subdir — same pattern as `build_image_via_microsandbox` in
    // `commands/env/apple_container.rs`.
    let workspace_root = flake_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "flake dir is not three levels deep in workspace: {}",
                flake_dir.display()
            )
        })?
        .to_path_buf();
    let flake_rel = flake_dir.strip_prefix(&workspace_root).map_err(|_| {
        anyhow::anyhow!(
            "flake dir not under derived workspace root: {}",
            flake_dir.display()
        )
    })?;
    let flake_rel_str = flake_rel.to_str().ok_or_else(|| {
        anyhow::anyhow!("flake subpath has non-UTF-8 bytes: {flake_rel:?}")
    })?;
    let guest_flake_ref = format!("path:{BUILDER_GUEST_WORK_DIR}/{flake_rel_str}");

    // Bind /nix opportunistically on Linux hosts that already run
    // native Nix — same rationale as the dev-image path. Skipped on
    // macOS (Darwin-targeted closures + permission tangle).
    let host_nix_store = if cfg!(target_os = "macos") {
        None
    } else {
        let host_nix = PathBuf::from("/nix");
        if host_nix.join("store").is_dir() {
            Some(host_nix)
        } else {
            None
        }
    };

    let job = BuilderJob {
        flake_ref: guest_flake_ref,
        attr_path: format!("packages.{}.default", host_system_linux()),
    };
    let mounts = BuilderMounts {
        flake_src: workspace_root,
        host_nix_store,
        artifact_out: dest_dir.to_path_buf(),
    };

    MicrosandboxBuilderVm::default()
        .run_build(&job, &mounts)
        .map_err(|e| anyhow::anyhow!("Stage 0 microsandbox build failed: {e}"))?;

    // MicrosandboxBuilderVm's copy_script handles vmlinux, rootfs.ext4,
    // cmdline, and manifest.json (plan 72 W5 added cmdline + manifest
    // to the copy list). Sanity-check the three files BuilderVmImage
    // expects before returning.
    for required in [
        BuilderVmImage::KERNEL_FILENAME,
        BuilderVmImage::ROOTFS_FILENAME,
        BuilderVmImage::CMDLINE_FILENAME,
    ] {
        let p = dest_dir.join(required);
        if !p.exists() {
            anyhow::bail!(
                "Stage 0 build completed but {} is missing from {}. \
                 The builder-vm flake may have failed to emit it (check the \
                 microsandbox build logs above) or the copy_script regressed.",
                required,
                dest_dir.display()
            );
        }
    }
    Ok(())
}

// ─────────────────── release download (ADR-002 §W5.1) ────────────

/// Architecture suffix used in release artifact filenames. Mirrors
/// the matrix used by `download_dev_image` so installed binaries on
/// Apple Silicon and Linux aarch64 share the artifact set.
pub fn release_arch_tag() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

/// Base URL for the matching mvmctl release. Each binary downloads
/// the artifacts tagged with its own `CARGO_PKG_VERSION`; bumping
/// the binary automatically invalidates the cache and pulls fresh
/// artifacts on the next miss.
#[cfg(feature = "backends-builder-vm-libkrun")]
fn release_base_url() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("https://github.com/tinylabscom/mvm/releases/download/v{version}")
}

/// Download the four builder VM image artifacts to `dest_dir` (the
/// installed-binary cache subdir), SHA-256 verifying each against
/// the release's `builder-vm-<arch>-checksums-sha256.txt` file.
///
/// On success, the dest dir contains `vmlinux`, `rootfs.ext4`, and
/// `cmdline` — the three filenames [`BuilderVmImage::load_from_dir`]
/// expects.
///
/// `MVM_SKIP_HASH_VERIFY=1` downgrades hash mismatches to warnings
/// (ADR-002 §W5.1 emergency-rotation escape). Never set in CI.
///
/// Feature-gated because the only consumer
/// ([`BuilderVmImage::load_from_dir`]) lives behind the same flag —
/// downloading the image without the ability to load it would be
/// dead weight in the default binary.
#[cfg(feature = "backends-builder-vm-libkrun")]
pub fn download_builder_vm_image(dest_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating builder VM cache dir {}", dest_dir.display()))?;

    let arch = release_arch_tag();
    let base = release_base_url();

    let vmlinux_name = format!("builder-vm-vmlinux-{arch}");
    let rootfs_name = format!("builder-vm-rootfs-{arch}.ext4");
    let cmdline_name = format!("builder-vm-cmdline-{arch}");
    let checksums_name = format!("builder-vm-{arch}-checksums-sha256.txt");

    let checksums_url = format!("{base}/{checksums_name}");
    let checksums_body = http::fetch_text(&checksums_url).with_context(|| {
        format!(
            "fetching {checksums_name} from {checksums_url} — the release tag may \
             predate plan 72 W2 or be missing the builder-vm artifacts entirely"
        )
    })?;
    let expected = parse_checksums(&checksums_body);

    for (src_name, dest_filename) in [
        (vmlinux_name.as_str(), BuilderVmImage::KERNEL_FILENAME),
        (rootfs_name.as_str(), BuilderVmImage::ROOTFS_FILENAME),
        (cmdline_name.as_str(), BuilderVmImage::CMDLINE_FILENAME),
    ] {
        let url = format!("{base}/{src_name}");
        let dest = dest_dir.join(dest_filename);
        http::download_file(&url, &dest)
            .with_context(|| format!("downloading {src_name} from {url}"))?;
        verify_artifact_hash(&dest, src_name, expected.get(src_name))?;
    }

    Ok(())
}

/// Parse a `sha256sum`-format file into `{filename → hex hash}`.
/// Tolerant of leading whitespace and the `*` binary-mode prefix
/// some tools emit. Lines that don't fit `<hash>  <name>` are skipped
/// silently — release files always conform to the canonical format,
/// and a parse error here would mask the more useful "expected hash
/// missing for X" downstream error from [`verify_artifact_hash`].
pub fn parse_checksums(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `<sha256-hex>  <filename>` (two spaces is canonical; tolerate one)
        // or `<sha256-hex> *<filename>` for binary-mode entries.
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let name = rest.trim_start().trim_start_matches('*');
        if name.is_empty() {
            continue;
        }
        out.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    out
}

/// Compute the lowercase-hex SHA-256 of a file's contents.
///
/// Streamed so the hash works against multi-GiB rootfs files
/// without loading them entirely into memory.
pub fn sha256_of_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {} for hashing", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify a downloaded artifact's SHA-256 against the expected hash.
///
/// Errors when:
/// - `expected` is `None` — the checksums file didn't list this artifact
/// - the computed hash doesn't match — likely tampering or a partial download
///
/// `MVM_SKIP_HASH_VERIFY=1` downgrades mismatches to a tracing warning
/// without aborting; documented in ADR-002 §W5.1 as an emergency escape.
pub fn verify_artifact_hash(path: &Path, name: &str, expected: Option<&String>) -> Result<()> {
    let expected = expected.ok_or_else(|| {
        anyhow::anyhow!(
            "checksums file did not list {name} — refusing to trust an unverified \
             artifact (the release publisher must include every per-arch entry)"
        )
    })?;
    let actual = sha256_of_file(path)?;
    if actual == *expected {
        return Ok(());
    }
    if std::env::var_os("MVM_SKIP_HASH_VERIFY").is_some() {
        tracing::warn!(
            artifact = name,
            expected = %expected,
            actual = %actual,
            "MVM_SKIP_HASH_VERIFY set — accepting hash mismatch. ADR-002 §W5.1 \
             documents this as an emergency-rotation escape only; never set in CI."
        );
        return Ok(());
    }
    // Best-effort cleanup so a subsequent run doesn't reuse the
    // poisoned file. Cache layer is responsible for re-creating dir
    // on next miss.
    let _ = std::fs::remove_file(path);
    anyhow::bail!(
        "{name} hash mismatch: expected {expected}, got {actual}. \
         File deleted to force re-download on next attempt."
    )
}

// ──────────────────────────── tests ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cache_root_lives_under_mvm_cache() {
        let root = cache_root();
        assert!(root.ends_with("builder-vm"));
        assert!(root.parent().unwrap().ends_with("mvm"));
    }

    #[test]
    fn installed_cache_dir_uses_version() {
        let dir = installed_cache_dir();
        let last = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(last, format!("v{}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn cache_key_changes_when_flake_changes() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("flake.nix"), b"version 1").unwrap();
        let key_a = cache_key_from_flake(dir.path()).unwrap();
        std::fs::write(dir.path().join("flake.nix"), b"version 2").unwrap();
        let key_b = cache_key_from_flake(dir.path()).unwrap();
        assert_ne!(
            key_a, key_b,
            "key must change when flake.nix bytes change"
        );
        assert_eq!(key_a.len(), 64, "sha256 hex digest should be 64 chars");
    }

    #[test]
    fn cache_key_changes_when_lock_appears() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("flake.nix"), b"flake body").unwrap();
        let key_without_lock = cache_key_from_flake(dir.path()).unwrap();
        std::fs::write(dir.path().join("flake.lock"), b"{}").unwrap();
        let key_with_lock = cache_key_from_flake(dir.path()).unwrap();
        assert_ne!(
            key_without_lock, key_with_lock,
            "appearing flake.lock must invalidate the cache key (forces rebuild)"
        );
    }

    #[test]
    fn cache_key_missing_flake_nix_errors() {
        let dir = tempdir().unwrap();
        let err = cache_key_from_flake(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("flake.nix"),
            "error should mention flake.nix: {msg}"
        );
    }

    #[test]
    fn source_cache_dir_uses_hash_subdir() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("flake.nix"), b"abc").unwrap();
        let key = cache_key_from_flake(dir.path()).unwrap();
        let cache = source_cache_dir(dir.path()).unwrap();
        let last = cache.file_name().unwrap().to_str().unwrap();
        assert_eq!(last, key);
    }

    #[test]
    fn find_builder_vm_flake_finds_the_in_repo_flake() {
        // This test runs from the workspace checkout — CARGO_MANIFEST_DIR
        // resolves into <repo>/crates/mvm-cli, so the workspace root is
        // <repo>, and <repo>/nix/images/builder-vm/flake.nix is what
        // plan 72 W2 just landed.
        let dir = find_builder_vm_flake().unwrap();
        assert!(dir.ends_with("nix/images/builder-vm"));
        assert!(dir.join("flake.nix").exists());
    }

    // 64-char hex strings used as fake sha256 digests in the tests
    // below. sha256("") is the canonical empty-input vector; any other
    // 64-char hex is just an opaque distinct value.
    const SHA_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const SHA_OTHER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_checksums_handles_canonical_sha256sum_format() {
        let body = format!("{SHA_EMPTY}  vmlinux\n{SHA_OTHER}  rootfs.ext4\n");
        let map = parse_checksums(&body);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("vmlinux").unwrap(), SHA_EMPTY);
        assert_eq!(map.get("rootfs.ext4").unwrap(), SHA_OTHER);
    }

    #[test]
    fn parse_checksums_tolerates_binary_prefix_and_blank_lines() {
        let body = format!("\n# this is a comment\n{SHA_EMPTY} *vmlinux\n\n");
        let map = parse_checksums(&body);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("vmlinux"));
    }

    #[test]
    fn parse_checksums_skips_malformed_lines() {
        let body = format!("not-a-hash-at-all  vmlinux\n{SHA_EMPTY}  good\n");
        let map = parse_checksums(&body);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("good"));
        assert!(!map.contains_key("vmlinux"));
    }

    #[test]
    fn sha256_of_file_matches_known_vector() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"abc").unwrap();
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sha256_of_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_artifact_hash_accepts_matching_hash() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string();
        verify_artifact_hash(&path, "data", Some(&expected)).unwrap();
        assert!(path.exists(), "matching hash must not delete the file");
    }

    #[test]
    fn verify_artifact_hash_rejects_mismatch_and_deletes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"abc").unwrap();
        let wrong = "0".repeat(64);
        let err = verify_artifact_hash(&path, "data", Some(&wrong)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hash mismatch"), "want mismatch error: {msg}");
        assert!(
            !path.exists(),
            "verify_artifact_hash must delete the poisoned file"
        );
    }

    #[test]
    fn verify_artifact_hash_errors_when_expected_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"abc").unwrap();
        let err = verify_artifact_hash(&path, "data", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("did not list"),
            "want 'did not list' message: {msg}"
        );
    }

    #[cfg(all(
        feature = "backends-builder-vm-libkrun",
        not(feature = "contributor-bootstrap")
    ))]
    #[test]
    fn ensure_builder_vm_image_signals_cache_miss_for_source_checkout() {
        // Without `contributor-bootstrap`, the source-checkout cache
        // miss path errors with a hint telling the user to rebuild
        // mvmctl with the feature on. The `contributor-bootstrap`
        // variant of this test path is harder to unit-test (it
        // genuinely attempts a microsandbox build) — coverage there
        // lives in the planned live test `dev_up_contributor_bootstrap.rs`.
        //
        // We rely on `MVM_CACHE_DIR` env-var precedence in
        // `mvm_core::config::mvm_cache_dir()`. `std::env::set_var` is
        // process-global; since we're inside `cargo test` with the
        // default thread-per-test runner, this can race with other
        // tests that also touch MVM_CACHE_DIR. Lock the env-var
        // section if you add another such test.
        let scratch = tempdir().unwrap();
        // Safety: this test is single-threaded with respect to its
        // own MVM_CACHE_DIR usage; cargo test runs tests in parallel
        // but no other test in this module touches the var.
        unsafe {
            std::env::set_var("MVM_CACHE_DIR", scratch.path());
        }
        let err = ensure_builder_vm_image().unwrap_err();
        unsafe {
            std::env::remove_var("MVM_CACHE_DIR");
        }
        let msg = format!("{err}");
        assert!(
            msg.contains("cache miss") || msg.contains("Cache miss"),
            "error should mention the cache miss: {msg}"
        );
        assert!(
            msg.contains("contributor-bootstrap"),
            "without the feature, error should point at it: {msg}"
        );
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn build_builder_vm_image_via_microsandbox_validates_flake_dir() {
        // Negative path: the function should reject a non-existent
        // flake dir before reaching the (expensive) microsandbox
        // spawn step. The positive path requires a working sandbox
        // and is exercised by the planned live test
        // `dev_up_contributor_bootstrap.rs`.
        let scratch = tempdir().unwrap();
        let missing = scratch.path().join("does-not-exist");
        let err = build_builder_vm_image_via_microsandbox(&missing, scratch.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("does not exist"),
            "should reject missing flake dir: {msg}"
        );
    }
}
