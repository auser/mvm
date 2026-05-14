//! End-to-end coverage for the [`mvm_build::app_deps`] host
//! orchestrator (Plan 73 Followup B.1). Uses on-disk sealed-volume
//! fixtures hand-authored via
//! [`mvm_sdk::compile::deps_audit::seal_volume`] so the cache-hit
//! path exercises the same wire format the builder VM will emit in
//! slice B.2.
//!
//! The cache layout under test:
//!
//! ```text
//! <cache_root>/
//! ├── <volume_hash>/      # hand-authored sealed volume
//! │   ├── content/
//! │   ├── sbom.cdx.json
//! │   ├── fetch.log
//! │   ├── cve.json
//! │   └── meta.json
//! └── index/
//!     └── <lockfile_hash>  # plain text: <volume_hash>\n
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mvm_build::app_deps::{
    GateLevel, InstallError, InstallSpec, Language, derive_lockfile_hash, install_app_deps,
    resolve_cache_root,
};
use mvm_sdk::compile::deps_audit::{
    FILE_CONTENT_DIR, FILE_CVE, FILE_FETCH_LOG, FILE_MANIFEST, FILE_SBOM, VolumeSealResult,
    seal_volume,
};
use sha2::{Digest, Sha256};

/// All-in-one fixture: a tempdir with a `cache_root/`, a
/// `source_root/`, and a hand-authored lockfile + sealed volume.
struct Fixture {
    tmp: tempfile::TempDir,
    cache_root: PathBuf,
    source_root: PathBuf,
    lockfile: PathBuf,
}

impl Fixture {
    /// Build a fixture with a populated source root + lockfile.
    /// `populate_cache=true` also seals a volume into the cache and
    /// writes its index entry; `false` exercises the miss path.
    fn new(populate_cache: bool) -> (Self, Option<VolumeSealResult>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cache_root = root.join("cache");
        let source_root = root.join("project");
        fs::create_dir_all(&cache_root).unwrap();
        fs::create_dir_all(&source_root).unwrap();

        // Realistic uv.lock-shaped bytes. Hashed verbatim by the
        // orchestrator; the content doesn't have to parse.
        let lockfile = source_root.join("uv.lock");
        fs::write(
            &lockfile,
            b"version = 1\n[[package]]\nname = \"requests\"\nversion = \"2.31.0\"\n",
        )
        .unwrap();
        // A source file or two so the source_root is a real dir.
        fs::write(
            source_root.join("pyproject.toml"),
            b"[project]\nname=\"t\"\n",
        )
        .unwrap();

        let sealed = if populate_cache {
            Some(seal_into_cache(&cache_root, &lockfile))
        } else {
            None
        };

        (
            Self {
                tmp,
                cache_root,
                source_root,
                lockfile,
            },
            sealed,
        )
    }

    fn spec(&self) -> InstallSpec {
        InstallSpec {
            lockfile: self.lockfile.clone(),
            source_root: self.source_root.clone(),
            language: Language::Python,
            gate: GateLevel::Dev,
            cache_root_override: Some(self.cache_root.clone()),
        }
    }
}

/// Build a minimal sealed volume at `<cache_root>/<volume_hash>/`
/// using `seal_volume`, then write the matching index pointer at
/// `<cache_root>/index/<lockfile_hash>`. Returns the seal result
/// so callers can assert against `volume_hash`.
fn seal_into_cache(cache_root: &Path, lockfile_path: &Path) -> VolumeSealResult {
    // Sealed volume materialized inside a scratch dir first; the
    // orchestrator's cache-hit path reads from the resulting hashed
    // directory at `<cache_root>/<volume_hash>/`.
    let scratch = cache_root.join("scratch");
    let content_dir = scratch.join(FILE_CONTENT_DIR);
    fs::create_dir_all(&content_dir).unwrap();
    fs::write(content_dir.join("requests-2.31.0.dist-info"), b"meta\n").unwrap();
    fs::create_dir_all(content_dir.join("requests")).unwrap();
    fs::write(
        content_dir.join("requests").join("__init__.py"),
        b"# stub\n",
    )
    .unwrap();

    let sbom = scratch.join(FILE_SBOM);
    fs::write(&sbom, br#"{"bomFormat":"CycloneDX","specVersion":"1.5"}"#).unwrap();
    let fetch_log = scratch.join(FILE_FETCH_LOG);
    fs::write(
        &fetch_log,
        b"GET https://pypi.org/simple/requests/\nGET https://files.pythonhosted.org/...\n",
    )
    .unwrap();
    let cve = scratch.join(FILE_CVE);
    fs::write(&cve, br#"{"results":[]}"#).unwrap();

    let result = seal_volume(
        &content_dir,
        &sbom,
        &fetch_log,
        &cve,
        "2026-05-13T00:00:00Z",
        BTreeMap::new(),
    )
    .expect("seal_volume");

    // Rename the scratch dir to `<cache_root>/<volume_hash>/` and
    // drop `meta.json` inside.
    let final_dir = cache_root.join(&result.volume_hash);
    fs::rename(&scratch, &final_dir).unwrap();
    fs::write(final_dir.join(FILE_MANIFEST), &result.manifest_bytes).unwrap();

    // Index pointer.
    let lockfile_sha = sha256_file(lockfile_path);
    let lockfile_hash = derive_lockfile_hash(&lockfile_sha, Language::Python, GateLevel::Dev);
    let index_dir = cache_root.join("index");
    fs::create_dir_all(&index_dir).unwrap();
    fs::write(index_dir.join(&lockfile_hash), &result.volume_hash).unwrap();

    result
}

fn sha256_file(p: &Path) -> String {
    let bytes = fs::read(p).unwrap();
    let mut h = Sha256::new();
    h.update(&bytes);
    let out = h.finalize();
    let mut s = String::new();
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn cache_hit_returns_verified_install_result() {
    let (fx, sealed) = Fixture::new(true);
    let sealed = sealed.expect("populated");
    let res = install_app_deps(&fx.spec()).expect("install ok");
    assert!(res.cache_hit, "expected cache_hit=true");
    assert_eq!(res.volume_hash, sealed.volume_hash);
    assert_eq!(res.volume_dir, fx.cache_root.join(&sealed.volume_hash));
    // manifest_sha256 is the sha256 of the on-disk meta.json bytes —
    // the supervisor's admission gate pins this separately from the
    // volume_hash (Followup A §"manifest_sha256 cross-check").
    assert!(!res.manifest_sha256.is_empty());
    assert_eq!(res.manifest_sha256.len(), 64);
    // Lockfile sha is the un-mixed-in value; deterministic over the
    // lockfile bytes.
    assert_eq!(res.lockfile_sha256, sha256_file(&fx.lockfile));
}

#[test]
fn cache_miss_returns_builder_vm_not_wired() {
    let (fx, _) = Fixture::new(false);
    let err = install_app_deps(&fx.spec()).expect_err("must miss");
    match err {
        InstallError::BuilderVmNotWired {
            lockfile_hash,
            language,
            gate,
            cache_root,
        } => {
            assert_eq!(language, "python");
            assert_eq!(gate, "dev");
            assert_eq!(cache_root, fx.cache_root);
            // The lockfile_hash is the derived key; check it matches
            // the helper so users + slice B.2 can reproduce it.
            let expected =
                derive_lockfile_hash(&sha256_file(&fx.lockfile), Language::Python, GateLevel::Dev);
            assert_eq!(lockfile_hash, expected);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn cache_hit_on_tampered_cve_fails_verify() {
    let (fx, sealed) = Fixture::new(true);
    let sealed = sealed.unwrap();
    // Hand-corrupt cve.json in the cached volume — same tamper the
    // SDK's `verify_detects_tampered_cve` test exercises.
    let cve_path = fx.cache_root.join(&sealed.volume_hash).join(FILE_CVE);
    fs::write(&cve_path, br#"{"results":["FORGED"]}"#).unwrap();

    let err = install_app_deps(&fx.spec()).expect_err("must fail closed");
    match err {
        InstallError::CacheVerifyFailed { lockfile_hash, .. } => {
            let expected =
                derive_lockfile_hash(&sha256_file(&fx.lockfile), Language::Python, GateLevel::Dev);
            assert_eq!(lockfile_hash, expected);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn cache_index_hash_mismatch_fails_closed() {
    // Index pointer says one volume, the on-disk dir is sealed at a
    // different hash. Means someone overwrote a directory in place;
    // fail closed rather than serve a stale cached value.
    let (fx, sealed) = Fixture::new(true);
    let sealed = sealed.unwrap();
    let bogus_hash = "f".repeat(64);
    let lockfile_sha = sha256_file(&fx.lockfile);
    let lockfile_hash = derive_lockfile_hash(&lockfile_sha, Language::Python, GateLevel::Dev);
    fs::write(
        fx.cache_root.join("index").join(&lockfile_hash),
        &bogus_hash,
    )
    .unwrap();
    // The bogus_hash dir doesn't exist either, so we get a Missing
    // error wrapped in CacheVerifyFailed (not CacheHashMismatch —
    // that variant only triggers when the dir exists but seals to a
    // different hash, which is unreachable via legitimate
    // `seal_volume` use). We also want the volume_hash variant
    // covered for the case where two different sealed dirs collide;
    // build a second sealed dir to exercise it.
    let err = install_app_deps(&fx.spec()).expect_err("must fail closed");
    assert!(
        matches!(err, InstallError::CacheVerifyFailed { .. }),
        "expected CacheVerifyFailed for missing dir, got: {err:?}"
    );

    // Now: place the *real* sealed volume under the bogus hash —
    // verify_sealed_volume succeeds but the derived hash disagrees
    // with the index pointer. This is the CacheHashMismatch path.
    let real_dir = fx.cache_root.join(&sealed.volume_hash);
    let bogus_dir = fx.cache_root.join(&bogus_hash);
    fs::rename(&real_dir, &bogus_dir).unwrap();
    let err = install_app_deps(&fx.spec()).expect_err("must fail closed");
    match err {
        InstallError::CacheHashMismatch {
            lockfile_hash: lh,
            index_hash,
            volume_hash,
        } => {
            assert_eq!(lh, lockfile_hash);
            assert_eq!(index_hash, bogus_hash);
            assert_eq!(volume_hash, sealed.volume_hash);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn lockfile_hash_is_deterministic_across_invocations() {
    // Same lockfile bytes → same derived cache path → same miss
    // diagnostic. Asserts the cache key is a pure function of
    // (lockfile bytes, language, gate).
    let (fx, _) = Fixture::new(false);
    let first = install_app_deps(&fx.spec()).expect_err("miss");
    let second = install_app_deps(&fx.spec()).expect_err("miss");
    let (first_hash, second_hash) = match (&first, &second) {
        (
            InstallError::BuilderVmNotWired {
                lockfile_hash: a, ..
            },
            InstallError::BuilderVmNotWired {
                lockfile_hash: b, ..
            },
        ) => (a, b),
        _ => panic!("expected miss variant from both calls"),
    };
    assert_eq!(first_hash, second_hash);
}

#[test]
fn missing_lockfile_returns_typed_error() {
    let (mut fx, _) = Fixture::new(false);
    fx.lockfile = fx.source_root.join("does-not-exist.lock");
    let err = install_app_deps(&fx.spec()).expect_err("missing lockfile");
    assert!(
        matches!(err, InstallError::LockfileMissing(_)),
        "got: {err:?}"
    );
}

#[test]
fn missing_source_root_returns_typed_error() {
    let (mut fx, _) = Fixture::new(false);
    fx.source_root = fx.tmp.path().join("no-such-project");
    let err = install_app_deps(&fx.spec()).expect_err("missing source_root");
    assert!(
        matches!(err, InstallError::SourceRootMissing(_)),
        "got: {err:?}"
    );
}

#[test]
fn override_takes_precedence_over_env() {
    // `cache_root_override` is the canonical knob; assert it short-
    // circuits before `mvm_deps_volumes_dir()` is consulted.
    let p = PathBuf::from("/tmp/precedence-fixture");
    assert_eq!(resolve_cache_root(Some(&p)), p);
}
