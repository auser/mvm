//! User-facing manifest file (`mvm.toml` or `Mvmfile.toml`) — the
//! "what to build and how to size it" primitive that drives the
//! template flow per plan 38 (manifest-driven template DX).
//!
//! A manifest is identified by its canonical filesystem path. The
//! registry slot for its build artifacts will live at
//! `~/.mvm/templates/<sha256(canonical_manifest_path)>/`. The
//! optional `name` field is a display label / S3 channel hint —
//! NOT the registry key.
//!
//! Schema (v1):
//! ```toml
//! flake = "."
//! profile = "default"
//! vcpus = 2
//! mem = "1024M"
//! data_disk = "0"
//! name = "openclaw"   # optional, display only
//! ```
//!
//! Boundary: build inputs + dev sizing only. No `role` (the flake's
//! profile selects role variants), no `[network]` (runtime policy
//! lives in `mvmctl up` flags / `~/.mvm/config.toml` / mvmd tenant
//! config), no dependencies (Nix owns build deps; mvmd owns runtime
//! deps).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::naming::{validate_flake_ref, validate_template_name};
use crate::util::parse_human_size;

/// Filenames recognised as manifests, in discovery preference order.
/// `mvm.toml` is preferred; `Mvmfile.toml` is accepted so the legacy
/// `mvmctl build` flow folds into the same parser/schema. If both
/// exist in one directory the discovery layer errors.
pub const MANIFEST_FILENAMES: &[&str] = &["mvm.toml", "Mvmfile.toml"];

/// Highest manifest schema version this build understands. Future
/// fields are additive via `#[serde(default)]`; bumping this signals
/// breaking changes that older mvmctl versions must reject.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Floor for `mem`. Below this Firecracker guests don't reliably
/// boot; we'd rather fail loudly at parse time than have a confusing
/// runtime error.
const MIN_MEM_MIB: u32 = 64;

fn default_schema_version() -> u32 {
    MANIFEST_SCHEMA_VERSION
}

fn default_flake() -> String {
    ".".to_string()
}

fn default_profile() -> String {
    "default".to_string()
}

fn default_vcpus() -> u8 {
    2
}

fn default_mem() -> String {
    "1024M".to_string()
}

fn default_data_disk() -> String {
    "0".to_string()
}

/// User-facing manifest file. One per project directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// Nix flake reference. `"."` resolves to the directory the
    /// manifest lives in. Any flake ref form is accepted (path,
    /// `github:owner/repo`, `git+https://…`, etc.).
    #[serde(default = "default_flake")]
    pub flake: String,

    /// Flake package selector — picks `packages.<system>.<profile>`
    /// out of the flake's outputs.
    #[serde(default = "default_profile")]
    pub profile: String,

    /// Firecracker host-side vCPU count.
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,

    /// Human-readable memory size (`512M`, `1G`, `1024`, …).
    #[serde(default = "default_mem")]
    pub mem: String,

    /// Human-readable data disk size; `"0"` means no data disk.
    #[serde(default = "default_data_disk")]
    pub data_disk: String,

    /// Optional display name used in `template list` output and as
    /// the S3 channel key for `template push`/`pull`. NOT the
    /// registry key — the registry uses the manifest's canonical
    /// path hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Manifest {
    /// Parse a manifest from TOML text and validate semantically.
    /// Validation runs immediately so broken manifests fail before
    /// any I/O (e.g. before `nix build` is invoked).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text).context("Failed to parse manifest TOML")?;
        m.validate()?;
        Ok(m)
    }

    /// Read and parse a manifest at a file path.
    pub fn read_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read manifest at {}", path.display()))?;
        Self::from_toml_str(&text)
    }

    /// Validate the manifest's contents.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(anyhow!(
                "manifest declares schema_version={}; this mvmctl supports {}; upgrade mvmctl",
                self.schema_version,
                MANIFEST_SCHEMA_VERSION
            ));
        }
        if self.flake.trim().is_empty() {
            return Err(anyhow!("manifest field `flake` must not be empty"));
        }
        validate_flake_ref(&self.flake)
            .with_context(|| format!("invalid `flake` field: {:?}", self.flake))?;
        if self.vcpus == 0 {
            return Err(anyhow!("manifest field `vcpus` must be >= 1"));
        }
        let mem = parse_human_size(&self.mem)
            .with_context(|| format!("invalid `mem` field: {:?}", self.mem))?;
        if mem < MIN_MEM_MIB {
            return Err(anyhow!(
                "manifest field `mem` must be >= {MIN_MEM_MIB} MiB (got {mem} MiB)"
            ));
        }
        let _ = parse_human_size(&self.data_disk)
            .with_context(|| format!("invalid `data_disk` field: {:?}", self.data_disk))?;
        if let Some(name) = self.name.as_deref() {
            validate_template_name(name)
                .with_context(|| format!("invalid `name` field: {:?}", name))?;
        }
        Ok(())
    }

    /// Memory in MiB, parsed from the human-readable string.
    pub fn mem_mib(&self) -> Result<u32> {
        parse_human_size(&self.mem).with_context(|| format!("invalid `mem` field: {:?}", self.mem))
    }

    /// Data disk in MiB.
    pub fn data_disk_mib(&self) -> Result<u32> {
        parse_human_size(&self.data_disk)
            .with_context(|| format!("invalid `data_disk` field: {:?}", self.data_disk))
    }
}

/// If exactly one of `mvm.toml` / `Mvmfile.toml` exists in `dir`,
/// return its path. If both exist, error (ambiguous). If neither,
/// return `None`.
pub fn manifest_in_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let candidates: Vec<PathBuf> = MANIFEST_FILENAMES
        .iter()
        .filter_map(|name| {
            let p = dir.join(name);
            if p.is_file() { Some(p) } else { None }
        })
        .collect();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(
            candidates.into_iter().next().expect("len checked above"),
        )),
        _ => Err(anyhow!(
            "found both mvm.toml and Mvmfile.toml in {}; pick one",
            dir.display()
        )),
    }
}

/// Walk upward from `start` looking for a manifest. Stops at the
/// first directory containing one, at a `.git` boundary, or at the
/// filesystem root. Returns the (canonicalised) manifest path or
/// `None` if none found before a stop condition.
pub fn discover_manifest_from_dir(start: &Path) -> Result<Option<PathBuf>> {
    let mut cur: PathBuf = std::fs::canonicalize(start)
        .with_context(|| format!("Failed to canonicalize {}", start.display()))?;
    loop {
        if let Some(p) = manifest_in_dir(&cur)? {
            return Ok(Some(p));
        }
        // .git marks a project boundary — don't escape upward.
        if cur.join(".git").exists() {
            return Ok(None);
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => return Ok(None),
        }
    }
}

/// Resolve a `--mvm-config <path>` argument: file paths are used
/// directly; directories are resolved via `manifest_in_dir`.
pub fn resolve_manifest_config_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        return manifest_in_dir(path)?
            .ok_or_else(|| anyhow!("no mvm.toml or Mvmfile.toml found in {}", path.display()));
    }
    Err(anyhow!("manifest path does not exist: {}", path.display()))
}

/// Canonical registry key for a manifest at `path`:
/// `sha256(canonical_absolute_path)` as 64-char hex. Resolves
/// symlinks so two access paths to the same file hash to the same
/// key.
pub fn canonical_key_for_path(path: &Path) -> Result<String> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Failed to canonicalize {}", path.display()))?;
    let bytes = canonical
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow!("canonical path contains non-UTF-8: {:?}", canonical))?
        .as_bytes();
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write fixture");
    }

    fn minimal_manifest_toml() -> &'static str {
        r#"
            flake = "."
            profile = "default"
            vcpus = 2
            mem = "1024M"
            data_disk = "0"
        "#
    }

    #[test]
    fn parse_minimal_manifest_succeeds() {
        let m = Manifest::from_toml_str(minimal_manifest_toml()).expect("parses");
        assert_eq!(m.flake, ".");
        assert_eq!(m.profile, "default");
        assert_eq!(m.vcpus, 2);
        assert_eq!(m.mem, "1024M");
        assert_eq!(m.data_disk, "0");
        assert!(m.name.is_none());
        assert_eq!(m.schema_version, MANIFEST_SCHEMA_VERSION);
    }

    #[test]
    fn parse_with_name_succeeds() {
        let toml = r#"
            flake = "."
            profile = "default"
            vcpus = 2
            mem = "1G"
            data_disk = "0"
            name = "openclaw"
        "#;
        let m = Manifest::from_toml_str(toml).expect("parses");
        assert_eq!(m.name.as_deref(), Some("openclaw"));
    }

    #[test]
    fn parse_uses_defaults_for_omitted_fields() {
        let m = Manifest::from_toml_str("").expect("parses");
        assert_eq!(m.flake, ".");
        assert_eq!(m.profile, "default");
        assert_eq!(m.vcpus, 2);
        assert_eq!(m.mem, "1024M");
        assert_eq!(m.data_disk, "0");
    }

    #[test]
    fn schema_version_too_new_rejected() {
        let toml = format!(
            r#"
                schema_version = {}
                flake = "."
                profile = "default"
                vcpus = 2
                mem = "1024M"
            "#,
            MANIFEST_SCHEMA_VERSION + 1
        );
        let err = Manifest::from_toml_str(&toml).expect_err("rejects too-new schema");
        let msg = format!("{err:#}");
        assert!(msg.contains("schema_version"));
        assert!(msg.contains("upgrade mvmctl"));
    }

    #[test]
    fn empty_flake_rejected() {
        let toml = r#"
            flake = ""
            profile = "default"
            vcpus = 2
            mem = "1024M"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects empty flake");
        assert!(format!("{err:#}").contains("flake"));
    }

    #[test]
    fn shell_meta_in_flake_rejected() {
        let toml = r#"
            flake = ". ; rm -rf /"
            profile = "default"
            vcpus = 2
            mem = "1024M"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects shell meta");
        assert!(format!("{err:#}").contains("flake"));
    }

    #[test]
    fn zero_vcpus_rejected() {
        let toml = r#"
            flake = "."
            profile = "default"
            vcpus = 0
            mem = "1024M"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects 0 vcpus");
        assert!(format!("{err:#}").contains("vcpus"));
    }

    #[test]
    fn too_small_mem_rejected() {
        let toml = r#"
            flake = "."
            profile = "default"
            vcpus = 2
            mem = "32M"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects <64M mem");
        assert!(format!("{err:#}").contains("mem"));
    }

    #[test]
    fn unparseable_mem_rejected() {
        let toml = r#"
            flake = "."
            profile = "default"
            vcpus = 2
            mem = "potato"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects junk mem");
        assert!(format!("{err:#}").contains("mem"));
    }

    #[test]
    fn invalid_name_rejected() {
        let toml = r#"
            flake = "."
            profile = "default"
            vcpus = 2
            mem = "1024M"
            name = "has/slash"
        "#;
        let err = Manifest::from_toml_str(toml).expect_err("rejects bad name");
        assert!(format!("{err:#}").contains("name"));
    }

    #[test]
    fn mem_mib_and_data_disk_mib_convert() {
        let m = Manifest::from_toml_str(minimal_manifest_toml()).unwrap();
        assert_eq!(m.mem_mib().unwrap(), 1024);
        assert_eq!(m.data_disk_mib().unwrap(), 0);
    }

    #[test]
    fn serde_skips_omitted_name() {
        let m = Manifest::from_toml_str(minimal_manifest_toml()).unwrap();
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("\"name\""));
    }

    #[test]
    fn manifest_in_dir_finds_mvm_toml() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        let p = manifest_in_dir(tmp.path()).unwrap().expect("found");
        assert_eq!(p.file_name().unwrap(), "mvm.toml");
    }

    #[test]
    fn manifest_in_dir_finds_mvmfile_toml() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Mvmfile.toml", minimal_manifest_toml());
        let p = manifest_in_dir(tmp.path()).unwrap().expect("found");
        assert_eq!(p.file_name().unwrap(), "Mvmfile.toml");
    }

    #[test]
    fn manifest_in_dir_returns_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(manifest_in_dir(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn manifest_in_dir_errors_when_both_present() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        write(tmp.path(), "Mvmfile.toml", minimal_manifest_toml());
        let err = manifest_in_dir(tmp.path()).expect_err("ambiguous");
        let msg = format!("{err:#}");
        assert!(msg.contains("both") || msg.contains("pick one"));
    }

    #[test]
    fn discover_walks_up_to_manifest() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        // Mark tmp as a project boundary so the walk stops here on
        // hosts whose tmpdir parent has its own manifest.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let p = discover_manifest_from_dir(&nested).unwrap().expect("found");
        assert_eq!(p.file_name().unwrap(), "mvm.toml");
    }

    #[test]
    fn discover_stops_at_git_boundary() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        // No manifest anywhere in tmp; .git stops the walk at tmp.
        let result = discover_manifest_from_dir(&nested).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolve_config_path_accepts_file() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        let path = tmp.path().join("mvm.toml");
        let resolved = resolve_manifest_config_path(&path).unwrap();
        assert_eq!(resolved, path);
    }

    #[test]
    fn resolve_config_path_accepts_directory() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        let resolved = resolve_manifest_config_path(tmp.path()).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "mvm.toml");
    }

    #[test]
    fn resolve_config_path_errors_on_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.toml");
        assert!(resolve_manifest_config_path(&missing).is_err());
    }

    #[test]
    fn resolve_config_path_errors_on_empty_directory() {
        let tmp = TempDir::new().unwrap();
        // Directory exists but contains no manifest.
        assert!(resolve_manifest_config_path(tmp.path()).is_err());
    }

    #[test]
    fn canonical_key_stable_across_relative_paths() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        let direct = canonical_key_for_path(&tmp.path().join("mvm.toml")).unwrap();
        let via_dot = canonical_key_for_path(&tmp.path().join("./mvm.toml")).unwrap();
        assert_eq!(direct, via_dot);
        assert_eq!(direct.len(), 64);
    }

    #[test]
    fn canonical_key_differs_between_files() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        write(tmp1.path(), "mvm.toml", minimal_manifest_toml());
        write(tmp2.path(), "mvm.toml", minimal_manifest_toml());
        let k1 = canonical_key_for_path(&tmp1.path().join("mvm.toml")).unwrap();
        let k2 = canonical_key_for_path(&tmp2.path().join("mvm.toml")).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn read_file_parses_and_validates() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "mvm.toml", minimal_manifest_toml());
        let m = Manifest::read_file(&tmp.path().join("mvm.toml")).unwrap();
        assert_eq!(m.flake, ".");
    }

    #[test]
    fn read_file_propagates_validation_error() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "mvm.toml",
            r#"
                flake = ""
                vcpus = 2
                mem = "1024M"
            "#,
        );
        assert!(Manifest::read_file(&tmp.path().join("mvm.toml")).is_err());
    }
}
