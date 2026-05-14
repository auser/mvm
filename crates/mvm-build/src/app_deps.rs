//! Host-side orchestrator for the application-dependency install
//! pipeline (ADR-047, Plan 73 Followup B).
//!
//! This module is the host-side seam between a user's `mvmctl build`
//! invocation and the builder-VM-side install pipeline that runs
//! `uv pip install --no-deps`, `pnpm install --frozen-lockfile`, etc.
//! It does **two** things:
//!
//! 1. **Cache resolution.** Given an [`InstallSpec`] pointing at a
//!    lockfile, derive a stable `lockfile_hash`, look it up in the
//!    deps-volume index, and — on a cache hit — re-verify the on-disk
//!    sealed volume via
//!    [`mvm_sdk::compile::deps_audit::verify_sealed_volume`]. A hit
//!    returns `InstallResult { cache_hit: true, .. }` carrying the
//!    canonical `volume_hash` + `manifest.sha256` the supervisor's
//!    admission gate (Followup A) pins.
//! 2. **Cache miss → builder-VM dispatch.** Slice B.1 reserves the
//!    seam but does not yet drive the builder VM; instead it returns
//!    [`InstallError::BuilderVmNotWired`]. Slice B.2 replaces this
//!    branch with a call into
//!    `crates/mvm-build/src/builder_vm.rs::LibkrunBuilderVm::run_build`
//!    (with mounted lockfile + source root + writable volume out).
//!
//! ### Why the cache key is a lockfile hash
//!
//! ADR-047 §"Lifecycle gates" pins the *volume* hash at admission
//! time, but the volume hash bakes in `cve.json` and `meta.json` —
//! values only the builder VM knows after the install runs. The
//! orchestrator needs a key it can derive *before* the VM runs so it
//! can answer "have I already installed this lockfile?" without a
//! VM spawn. The lockfile sha256 is that key: same lockfile bytes
//! ⇒ same key ⇒ same cached volume.
//!
//! The cache layout is:
//!
//! ```text
//! <deps_volumes_dir>/
//! ├── <volume_hash>/          # sealed volume (content + sidecars)
//! │   ├── content/
//! │   ├── sbom.cdx.json
//! │   ├── fetch.log
//! │   ├── cve.json
//! │   └── meta.json
//! └── index/
//!     └── <lockfile_hash>     # plain text: <volume_hash>\n
//! ```
//!
//! The supervisor only ever reads the `<volume_hash>/` directories;
//! the `index/` tree is host-orchestrator state. A corrupt index
//! entry can only ever cause a cache *miss* — the verifier still
//! re-derives the canonical volume_hash from disk, so a tampered
//! index can't pass a forged volume into admission.
//!
//! ### Cross-platform
//!
//! Per CLAUDE.md ("Host Nix is never used by mvmctl"): this module
//! does not invoke host Nix and does not call out to any host
//! installer. Every operation here is pure I/O — sha256 streaming,
//! filesystem reads, `verify_sealed_volume`. The actual install runs
//! inside the libkrun builder VM (slice B.2).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use mvm_sdk::compile::deps_audit::{VolumeError, verify_sealed_volume};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Subdirectory under the deps-volumes root that holds the
/// `lockfile_hash → volume_hash` index. Kept out of the supervisor's
/// admission path (it only walks `<root>/<volume_hash>/`).
pub const INDEX_SUBDIR: &str = "index";

/// Which language ecosystem the lockfile belongs to. Determines
/// which installer the builder VM dispatches (slice B.2) and
/// participates in the cache key so the same lockfile bytes can't
/// accidentally collide across ecosystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    Node,
}

impl Language {
    /// Short token mixed into the cache key. Stable wire-string;
    /// must not change without a cache-rebuild.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Node => "node",
        }
    }
}

/// ADR-047 §"Lifecycle gates" — the gate level controls strictness
/// of the builder-VM-side checks (attestations, CVE severity). The
/// orchestrator carries it through to slice B.2; in B.1 it
/// participates in the cache key so a `--dev` cache entry can't
/// satisfy a `--prod` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateLevel {
    /// Warnings on missing attestations / non-critical CVEs.
    Dev,
    /// Fails closed on missing attestations or high/critical CVEs.
    Prod,
}

impl GateLevel {
    pub fn token(&self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Prod => "prod",
        }
    }
}

/// What the caller wants installed. Pure data — the orchestrator
/// reads files at the given paths but does not retain handles.
#[derive(Debug, Clone)]
pub struct InstallSpec {
    /// Path to the lockfile (`uv.lock`, `package-lock.json`,
    /// `pnpm-lock.yaml`). Hashed verbatim — the orchestrator does
    /// not parse it.
    pub lockfile: PathBuf,
    /// Project root that holds the lockfile + any auxiliary inputs
    /// the installer needs (`pyproject.toml`, `package.json`). Slice
    /// B.2 bind-mounts this into the builder VM; B.1 records the
    /// path on `InstallResult` for diagnostics but does not read
    /// from it.
    pub source_root: PathBuf,
    pub language: Language,
    pub gate: GateLevel,
    /// Optional override for the deps-volumes cache root. When
    /// `None`, resolves to
    /// [`mvm_core::config::mvm_deps_volumes_dir`] (which itself
    /// honors `MVM_DEPS_VOLUMES_DIR` — same env knob Followup A's
    /// admission verifier uses).
    pub cache_root_override: Option<PathBuf>,
}

/// Result of a successful install resolution. The caller (slice
/// B.3's `mvmctl build` wiring + Followup A's `ExecutionPlan`
/// synthesis) pins both `volume_hash` and `manifest_sha256` into
/// the plan so the admission verifier can re-derive them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub volume_hash: String,
    pub manifest_sha256: String,
    pub cache_hit: bool,
    /// The path the supervisor will read from at admission time.
    pub volume_dir: PathBuf,
    /// The sha256 of the lockfile bytes, surfaced for diagnostics +
    /// `mvmctl deps inspect` (slice B.3).
    pub lockfile_sha256: String,
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("install spec lockfile `{}` is missing", .0.display())]
    LockfileMissing(PathBuf),

    #[error("install spec source root `{}` is missing", .0.display())]
    SourceRootMissing(PathBuf),

    #[error("failed to read lockfile `{}`: {source}", path.display())]
    LockfileIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to read cache index `{}`: {source}", path.display())]
    IndexIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Cache hit on the index, but the volume directory it pointed
    /// at failed [`verify_sealed_volume`]. Propagates the underlying
    /// audit error so tamper-detection messages flow through. The
    /// orchestrator does **not** silently demote this to a miss —
    /// a corrupt cache entry needs operator attention.
    #[error("cache verify failed for lockfile_hash {lockfile_hash}: {source}")]
    CacheVerifyFailed {
        lockfile_hash: String,
        #[source]
        source: VolumeError,
    },

    /// Cache hit on the index, the volume verified, but the
    /// volume_hash that lived inside the volume disagrees with the
    /// hash the index pointed at. Means someone overwrote a
    /// directory in-place. Fail closed — same posture as
    /// [`Self::CacheVerifyFailed`].
    #[error(
        "cache index hash mismatch for lockfile_hash {lockfile_hash}: index said {index_hash}, on-disk volume sealed at {volume_hash}"
    )]
    CacheHashMismatch {
        lockfile_hash: String,
        index_hash: String,
        volume_hash: String,
    },

    /// The cache miss path. Slice B.1's deliberate seam — slice B.2
    /// replaces this branch with a builder-VM dispatch. The error
    /// message points users at the manual cache-population path so
    /// they can hand-author a sealed volume for testing the
    /// downstream admission gates (Followup A) before B.2 lands.
    #[error(
        "no cached deps volume for lockfile_hash {lockfile_hash} ({language}/{gate}); \
         builder-VM install pipeline is not yet wired (Plan 73 Followup B.2). \
         To unblock local testing, hand-author a sealed volume under \
         `{cache_root}/<volume_hash>/` (use `mvm_sdk::compile::deps_audit::seal_volume`) \
         and place a pointer at `{cache_root}/index/{lockfile_hash}`."
    )]
    BuilderVmNotWired {
        lockfile_hash: String,
        language: &'static str,
        gate: &'static str,
        cache_root: PathBuf,
    },
}

/// Resolve a deps install for `spec`. Pure host-side operation —
/// hashes the lockfile, probes the cache index, and either returns
/// a verified [`InstallResult`] or the [`InstallError::BuilderVmNotWired`]
/// placeholder. Does not invoke the builder VM and does not write to
/// the cache.
pub fn install_app_deps(spec: &InstallSpec) -> Result<InstallResult, InstallError> {
    if !spec.lockfile.is_file() {
        return Err(InstallError::LockfileMissing(spec.lockfile.clone()));
    }
    if !spec.source_root.is_dir() {
        return Err(InstallError::SourceRootMissing(spec.source_root.clone()));
    }

    let lockfile_sha256 =
        sha256_file(&spec.lockfile).map_err(|source| InstallError::LockfileIo {
            path: spec.lockfile.clone(),
            source,
        })?;
    let lockfile_hash = derive_lockfile_hash(&lockfile_sha256, spec.language, spec.gate);
    let cache_root = resolve_cache_root(spec.cache_root_override.as_deref());

    match lookup_cached_volume(&cache_root, &lockfile_hash)? {
        Some(volume_hash) => {
            let volume_dir = cache_root.join(&volume_hash);
            let computed = verify_sealed_volume(&volume_dir).map_err(|source| {
                InstallError::CacheVerifyFailed {
                    lockfile_hash: lockfile_hash.clone(),
                    source,
                }
            })?;
            if computed != volume_hash {
                return Err(InstallError::CacheHashMismatch {
                    lockfile_hash,
                    index_hash: volume_hash,
                    volume_hash: computed,
                });
            }
            let manifest_sha256 =
                sha256_file(&volume_dir.join(mvm_sdk::compile::deps_audit::FILE_MANIFEST))
                    .map_err(|source| InstallError::LockfileIo {
                        path: volume_dir.join(mvm_sdk::compile::deps_audit::FILE_MANIFEST),
                        source,
                    })?;
            Ok(InstallResult {
                volume_hash,
                manifest_sha256,
                cache_hit: true,
                volume_dir,
                lockfile_sha256,
            })
        }
        None => Err(InstallError::BuilderVmNotWired {
            lockfile_hash,
            language: spec.language.token(),
            gate: spec.gate.token(),
            cache_root,
        }),
    }
}

/// Derive the stable cache key from the lockfile sha256 + the
/// language + the gate. Pure function so callers (tests, future
/// `mvmctl deps inspect`) can reproduce it without re-hashing.
///
/// The mix-in tokens prevent two ecosystems from colliding on a
/// byte-identical lockfile (`requirements.txt` vs. a Node lockfile
/// of the same length), and prevent a `--dev` cache entry from
/// satisfying a `--prod` admission request.
pub fn derive_lockfile_hash(lockfile_sha256: &str, language: Language, gate: GateLevel) -> String {
    let mut h = Sha256::new();
    h.update(b"mvm-app-deps-v1\n");
    h.update(language.token().as_bytes());
    h.update(b"\n");
    h.update(gate.token().as_bytes());
    h.update(b"\n");
    h.update(lockfile_sha256.as_bytes());
    hex(&h.finalize())
}

/// Resolve the deps-volumes cache root, honoring the per-call
/// override and falling back to
/// [`mvm_core::config::mvm_deps_volumes_dir`] (which itself honors
/// `MVM_DEPS_VOLUMES_DIR`). Shared lookup with Followup A's
/// admission verifier — no duplication.
pub fn resolve_cache_root(override_path: Option<&Path>) -> PathBuf {
    override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(mvm_core::config::mvm_deps_volumes_dir()))
}

/// Read the cache index entry for a `lockfile_hash`. Returns the
/// recorded `volume_hash` on hit, `None` on miss (no index file).
/// I/O errors other than `NotFound` propagate.
fn lookup_cached_volume(
    cache_root: &Path,
    lockfile_hash: &str,
) -> Result<Option<String>, InstallError> {
    let path = cache_root.join(INDEX_SUBDIR).join(lockfile_hash);
    match fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim().to_owned();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(InstallError::IndexIo { path, source }),
    }
}

fn sha256_file(path: &Path) -> Result<String, io::Error> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    //! Module-level unit coverage for the pure helpers; the
    //! end-to-end orchestrator paths (cache hit, miss, tamper,
    //! determinism) live in
    //! `crates/mvm-build/tests/app_deps_orchestrator.rs` so they
    //! exercise the public API.
    use super::*;

    #[test]
    fn lockfile_hash_differs_across_languages() {
        let h_py = derive_lockfile_hash("aa", Language::Python, GateLevel::Dev);
        let h_node = derive_lockfile_hash("aa", Language::Node, GateLevel::Dev);
        assert_ne!(h_py, h_node);
    }

    #[test]
    fn lockfile_hash_differs_across_gates() {
        let h_dev = derive_lockfile_hash("aa", Language::Python, GateLevel::Dev);
        let h_prod = derive_lockfile_hash("aa", Language::Python, GateLevel::Prod);
        assert_ne!(h_dev, h_prod);
    }

    #[test]
    fn lockfile_hash_is_pure() {
        let h1 = derive_lockfile_hash("aa", Language::Python, GateLevel::Dev);
        let h2 = derive_lockfile_hash("aa", Language::Python, GateLevel::Dev);
        assert_eq!(h1, h2);
    }

    #[test]
    fn resolve_cache_root_prefers_override() {
        let p = PathBuf::from("/tmp/forced");
        assert_eq!(resolve_cache_root(Some(&p)), p);
    }
}
