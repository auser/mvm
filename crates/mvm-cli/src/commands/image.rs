//! `mvmctl image` - inspect and prune the local OCI image cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ui;
use mvm_core::user_config::MvmConfig;

use super::Cli;
use super::shared::human_bytes;

const INDEX_FILE: &str = "index.json";

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: ImageAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum ImageAction {
    /// List cached OCI images
    Ls {
        /// Filter cached images by registry host
        #[arg(long, value_name = "HOST")]
        registry: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Inspect a cached OCI image by reference or resolved digest
    Inspect {
        /// Image reference or resolved digest
        reference: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove a cached OCI image and garbage-collect unreferenced layers
    Rm {
        /// Image reference or resolved digest
        reference: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OciCacheIndex {
    #[serde(default = "schema_version")]
    schema_version: u32,
    #[serde(default)]
    images: Vec<CachedOciImage>,
}

fn schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedOciImage {
    reference: String,
    registry: String,
    repository: String,
    tag: Option<String>,
    resolved_digest: String,
    fetched_at: String,
    manifest_path: String,
    #[serde(default)]
    config_path: Option<String>,
    #[serde(default)]
    rootfs_path: Option<String>,
    #[serde(default)]
    claims_path: Option<String>,
    #[serde(default)]
    layers: Vec<CachedOciLayer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedOciLayer {
    digest: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ImageListRow {
    reference: String,
    registry: String,
    repository: String,
    tag: Option<String>,
    resolved_digest: String,
    fetched_at: String,
    size_bytes: u64,
    layers: usize,
}

#[derive(Debug, Serialize)]
struct InspectOutput {
    image: CachedOciImage,
    size_bytes: u64,
    manifest: Option<Value>,
    config: Option<Value>,
    claims: Option<Value>,
}

#[derive(Debug, PartialEq, Eq)]
struct RemoveOutcome {
    reference: String,
    removed_files: usize,
    freed_bytes: u64,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let cache_root = oci_cache_root();
    match args.action {
        ImageAction::Ls { registry, json } => {
            let rows = list_rows(&cache_root, registry.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                render_list(&rows);
            }
            Ok(())
        }
        ImageAction::Inspect { reference, json } => {
            let output = inspect_image(&cache_root, &reference)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                render_inspect(&output);
            }
            Ok(())
        }
        ImageAction::Rm { reference } => {
            let outcome = remove_image(&cache_root, &reference)?;
            ui::success(&format!(
                "Removed cached image {} ({} file(s), freed {}).",
                outcome.reference,
                outcome.removed_files,
                human_bytes(outcome.freed_bytes)
            ));
            mvm_core::audit_emit!(
                CachePrune,
                "source=image_rm reference={} removed={} freed_bytes={}",
                outcome.reference,
                outcome.removed_files,
                outcome.freed_bytes
            );
            Ok(())
        }
    }
}

fn oci_cache_root() -> PathBuf {
    PathBuf::from(mvm_core::config::mvm_cache_dir()).join("oci")
}

fn list_rows(cache_root: &Path, registry: Option<&str>) -> Result<Vec<ImageListRow>> {
    let index = load_index(cache_root)?;
    let rows = index
        .images
        .iter()
        .filter(|image| registry.is_none_or(|want| image.registry == want))
        .map(|image| ImageListRow {
            reference: image.reference.clone(),
            registry: image.registry.clone(),
            repository: image.repository.clone(),
            tag: image.tag.clone(),
            resolved_digest: image.resolved_digest.clone(),
            fetched_at: image.fetched_at.clone(),
            size_bytes: image_size_bytes(cache_root, image),
            layers: image.layers.len(),
        })
        .collect();
    Ok(rows)
}

fn inspect_image(cache_root: &Path, reference: &str) -> Result<InspectOutput> {
    let index = load_index(cache_root)?;
    let image = find_image(&index, reference)
        .with_context(|| format!("cached OCI image not found for '{reference}'"))?
        .clone();
    Ok(InspectOutput {
        size_bytes: image_size_bytes(cache_root, &image),
        manifest: read_json_optional(cache_root, &image.manifest_path)?,
        config: image
            .config_path
            .as_deref()
            .map(|p| read_json_optional(cache_root, p))
            .transpose()?
            .flatten(),
        claims: image
            .claims_path
            .as_deref()
            .map(|p| read_json_optional(cache_root, p))
            .transpose()?
            .flatten(),
        image,
    })
}

fn remove_image(cache_root: &Path, reference: &str) -> Result<RemoveOutcome> {
    let mut index = load_index(cache_root)?;
    let Some(position) = index
        .images
        .iter()
        .position(|image| image_matches(image, reference))
    else {
        bail!("cached OCI image not found for '{reference}'");
    };

    let image = index.images.remove(position);
    let mut removed_files = 0usize;
    let mut freed_bytes = 0u64;
    let shared_layer_paths = remaining_layer_paths(&index);
    validate_image_paths(cache_root, &image)?;

    for path in metadata_paths(&image) {
        remove_cache_file(cache_root, &path, &mut removed_files, &mut freed_bytes)?;
    }

    for layer in &image.layers {
        let Some(path) = layer.path.as_deref() else {
            continue;
        };
        if shared_layer_paths.contains(path) {
            continue;
        }
        remove_cache_file(cache_root, path, &mut removed_files, &mut freed_bytes)?;
    }

    save_index(cache_root, &index)?;
    Ok(RemoveOutcome {
        reference: image.reference,
        removed_files,
        freed_bytes,
    })
}

fn load_index(cache_root: &Path) -> Result<OciCacheIndex> {
    let path = cache_root.join(INDEX_FILE);
    if !path.exists() {
        return Ok(OciCacheIndex::default());
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let index: OciCacheIndex =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if index.schema_version != 1 {
        bail!(
            "unsupported OCI cache index schema_version {} at {}",
            index.schema_version,
            path.display()
        );
    }
    Ok(index)
}

fn save_index(cache_root: &Path, index: &OciCacheIndex) -> Result<()> {
    fs::create_dir_all(cache_root).with_context(|| format!("create {}", cache_root.display()))?;
    let path = cache_root.join(INDEX_FILE);
    let bytes = serde_json::to_vec_pretty(index).context("serialize OCI cache index")?;
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
}

fn find_image<'a>(index: &'a OciCacheIndex, reference: &str) -> Option<&'a CachedOciImage> {
    index
        .images
        .iter()
        .find(|image| image_matches(image, reference))
}

fn image_matches(image: &CachedOciImage, reference: &str) -> bool {
    image.reference == reference
        || image.resolved_digest == reference
        || image
            .resolved_digest
            .strip_prefix("sha256:")
            .is_some_and(|digest| digest == reference)
}

fn image_size_bytes(cache_root: &Path, image: &CachedOciImage) -> u64 {
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    for path in all_image_paths(image) {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Ok(path) = safe_cache_path(cache_root, &path)
            && let Ok(meta) = path.metadata()
            && meta.is_file()
        {
            total = total.saturating_add(meta.len());
        }
    }
    if total == 0 {
        image
            .layers
            .iter()
            .map(|layer| layer.size_bytes)
            .fold(0u64, u64::saturating_add)
    } else {
        total
    }
}

fn all_image_paths(image: &CachedOciImage) -> Vec<String> {
    let mut paths = metadata_paths(image);
    paths.extend(image.layers.iter().filter_map(|layer| layer.path.clone()));
    paths
}

fn metadata_paths(image: &CachedOciImage) -> Vec<String> {
    let mut paths = vec![image.manifest_path.clone()];
    paths.extend(image.config_path.clone());
    paths.extend(image.rootfs_path.clone());
    paths.extend(image.claims_path.clone());
    paths
}

fn remaining_layer_paths(index: &OciCacheIndex) -> BTreeSet<String> {
    index
        .images
        .iter()
        .flat_map(|image| image.layers.iter())
        .filter_map(|layer| layer.path.clone())
        .collect()
}

fn validate_image_paths(cache_root: &Path, image: &CachedOciImage) -> Result<()> {
    for path in all_image_paths(image) {
        let _ = safe_cache_path(cache_root, &path)?;
    }
    Ok(())
}

fn read_json_optional(cache_root: &Path, relative: &str) -> Result<Option<Value>> {
    let path = safe_cache_path(cache_root, relative)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

fn remove_cache_file(
    cache_root: &Path,
    relative: &str,
    removed_files: &mut usize,
    freed_bytes: &mut u64,
) -> Result<()> {
    let path = safe_cache_path(cache_root, relative)?;
    if !path.exists() {
        return Ok(());
    }
    let meta = path
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        bail!("refusing to remove non-file cache path {}", path.display());
    }
    let len = meta.len();
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    *removed_files += 1;
    *freed_bytes = freed_bytes.saturating_add(len);
    prune_empty_parents(cache_root, path.parent())?;
    Ok(())
}

fn prune_empty_parents(cache_root: &Path, mut current: Option<&Path>) -> Result<()> {
    while let Some(dir) = current {
        if dir == cache_root {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => current = dir.parent(),
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(e) => return Err(e).with_context(|| format!("remove {}", dir.display())),
        }
    }
    Ok(())
}

fn safe_cache_path(cache_root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        bail!("OCI cache path must be relative: {relative}");
    }
    if rel.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("OCI cache path escapes cache root: {relative}");
    }
    Ok(cache_root.join(rel))
}

fn render_list(rows: &[ImageListRow]) {
    if rows.is_empty() {
        println!("No cached OCI images.");
        return;
    }
    println!(
        "{:<38} {:<20} {:<18} {:>10}",
        "REFERENCE", "DIGEST", "FETCHED", "SIZE"
    );
    for row in rows {
        println!(
            "{:<38} {:<20} {:<18} {:>10}",
            truncate(&row.reference, 38),
            truncate(&row.resolved_digest, 20),
            truncate(&row.fetched_at, 18),
            human_bytes(row.size_bytes)
        );
    }
}

fn render_inspect(output: &InspectOutput) {
    let image = &output.image;
    println!("Reference: {}", image.reference);
    println!("Registry: {}", image.registry);
    println!("Repository: {}", image.repository);
    if let Some(tag) = &image.tag {
        println!("Tag: {tag}");
    }
    println!("Resolved digest: {}", image.resolved_digest);
    println!("Fetched at: {}", image.fetched_at);
    println!("Size: {}", human_bytes(output.size_bytes));
    println!("Manifest: {}", image.manifest_path);
    if let Some(path) = &image.config_path {
        println!("Config: {path}");
    }
    if let Some(path) = &image.rootfs_path {
        println!("Rootfs: {path}");
    }
    println!(
        "mvm-claims.json: {}",
        if output.claims.is_some() {
            "present"
        } else {
            "absent"
        }
    );
    println!("Layers:");
    for layer in &image.layers {
        let path = layer.path.as_deref().unwrap_or("-");
        println!(
            "  {}  {}  {}",
            layer.digest,
            human_bytes(layer.size_bytes),
            path
        );
    }
    if let Some(claims) = &output.claims
        && let Some(labels) = claims.as_object().and_then(|obj| obj.get("labels"))
    {
        let labels: BTreeMap<_, _> = labels
            .as_object()
            .into_iter()
            .flat_map(|obj| obj.iter())
            .collect();
        if !labels.is_empty() {
            println!("Claim labels:");
            for (key, value) in labels {
                println!("  {key}: {value}");
            }
        }
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut s: String = value.chars().take(max.saturating_sub(1)).collect();
    s.push('~');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image(reference: &str, digest: &str, layer_path: &str) -> CachedOciImage {
        CachedOciImage {
            reference: reference.to_string(),
            registry: "docker.io".to_string(),
            repository: "library/alpine".to_string(),
            tag: Some("3.20".to_string()),
            resolved_digest: digest.to_string(),
            fetched_at: "2026-05-18T00:00:00Z".to_string(),
            manifest_path: "manifests/alpine.json".to_string(),
            config_path: Some("configs/alpine.json".to_string()),
            rootfs_path: None,
            claims_path: Some("claims/alpine.json".to_string()),
            layers: vec![CachedOciLayer {
                digest: "sha256:layer".to_string(),
                size_bytes: 4,
                path: Some(layer_path.to_string()),
            }],
        }
    }

    fn write_index(cache_root: &Path, index: &OciCacheIndex) {
        fs::create_dir_all(cache_root).expect("create cache root");
        fs::write(
            cache_root.join(INDEX_FILE),
            serde_json::to_vec_pretty(index).expect("serialize index"),
        )
        .expect("write index");
    }

    fn write_file(cache_root: &Path, relative: &str, body: &[u8]) {
        let path = cache_root.join(relative);
        fs::create_dir_all(path.parent().expect("relative has parent")).expect("create parent");
        fs::write(path, body).expect("write cache file");
    }

    #[test]
    fn missing_index_lists_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rows = list_rows(tmp.path(), None).expect("list");
        assert!(rows.is_empty());
    }

    #[test]
    fn registry_filter_limits_list_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut other = sample_image("ghcr.io/acme/app:1", "sha256:app", "blobs/app");
        other.registry = "ghcr.io".to_string();
        other.repository = "acme/app".to_string();
        write_index(
            tmp.path(),
            &OciCacheIndex {
                schema_version: 1,
                images: vec![
                    sample_image("docker.io/library/alpine:3.20", "sha256:alpine", "blobs/a"),
                    other,
                ],
            },
        );

        let rows = list_rows(tmp.path(), Some("ghcr.io")).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "ghcr.io/acme/app:1");
    }

    #[test]
    fn inspect_resolves_by_reference_and_digest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let image = sample_image("docker.io/library/alpine:3.20", "sha256:alpine", "blobs/a");
        write_index(
            tmp.path(),
            &OciCacheIndex {
                schema_version: 1,
                images: vec![image],
            },
        );
        write_file(
            tmp.path(),
            "manifests/alpine.json",
            br#"{"schemaVersion":2}"#,
        );
        write_file(
            tmp.path(),
            "configs/alpine.json",
            br#"{"architecture":"arm64"}"#,
        );
        write_file(
            tmp.path(),
            "claims/alpine.json",
            br#"{"labels":{"mvm":"yes"}}"#,
        );

        let by_ref =
            inspect_image(tmp.path(), "docker.io/library/alpine:3.20").expect("inspect by ref");
        let by_digest = inspect_image(tmp.path(), "alpine").expect("inspect by short digest");
        assert_eq!(by_ref.image.reference, by_digest.image.reference);
        assert!(by_ref.manifest.is_some());
        assert!(by_ref.config.is_some());
        assert!(by_ref.claims.is_some());
    }

    #[test]
    fn remove_refuses_paths_that_escape_cache_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut image = sample_image("docker.io/library/alpine:3.20", "sha256:alpine", "../bad");
        image.manifest_path = "../manifest.json".to_string();
        write_index(
            tmp.path(),
            &OciCacheIndex {
                schema_version: 1,
                images: vec![image],
            },
        );

        let err = remove_image(tmp.path(), "sha256:alpine").expect_err("unsafe path rejected");
        assert!(err.to_string().contains("escapes cache root"));
        let index = load_index(tmp.path()).expect("index still readable");
        assert_eq!(index.images.len(), 1);
    }

    #[test]
    fn remove_preserves_shared_layers_and_updates_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = sample_image(
            "docker.io/library/alpine:3.20",
            "sha256:first",
            "blobs/shared",
        );
        let mut second = sample_image(
            "docker.io/library/busybox:1",
            "sha256:second",
            "blobs/shared",
        );
        second.manifest_path = "manifests/busybox.json".to_string();
        second.config_path = None;
        second.claims_path = None;
        write_index(
            tmp.path(),
            &OciCacheIndex {
                schema_version: 1,
                images: vec![first, second],
            },
        );
        write_file(tmp.path(), "manifests/alpine.json", b"{}");
        write_file(tmp.path(), "configs/alpine.json", b"{}");
        write_file(tmp.path(), "claims/alpine.json", b"{}");
        write_file(tmp.path(), "blobs/shared", b"layer");

        let outcome = remove_image(tmp.path(), "sha256:first").expect("remove");
        assert_eq!(outcome.removed_files, 3);
        assert!(tmp.path().join("blobs/shared").exists());
        let index = load_index(tmp.path()).expect("load index");
        assert_eq!(index.images.len(), 1);
        assert_eq!(index.images[0].reference, "docker.io/library/busybox:1");
    }
}
