//! `mvmctl image` - pull, inspect, and prune the local OCI image cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use flate2::read::GzDecoder;
use mvm_build::rootfs::{MaterializeExt4Input, MaterializeExt4Options, materialize_ext4};
use mvm_oci::{
    ImageReference, LayerDescriptor, LayerFetchOptions, LinuxPlatform, OciLayerFetcher,
    OciManifestFetcher, UnpackOptions, unpack_layer, verify_sha256_digest,
};
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
    /// Pull, unpack, and materialize an OCI image into the local cache
    Pull {
        /// OCI image reference
        reference: String,
        /// Production policy: require an immutable digest-pinned reference
        #[arg(long)]
        prod: bool,
    },
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
        ImageAction::Pull { reference, prod } => {
            let image = pull_image(&cache_root, &reference, prod)?;
            mvm_core::audit_emit!(
                ImageFetch,
                "source=image_pull reference={} digest={} prod={}",
                image.reference,
                image.resolved_digest,
                prod
            );
            ui::success(&format!(
                "Pulled {} -> {}",
                image.reference, image.resolved_digest
            ));
            if let Some(rootfs_path) = image.rootfs_path {
                ui::info(&format!(
                    "Rootfs: {}",
                    cache_root.join(rootfs_path).display()
                ));
            }
            Ok(())
        }
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

fn pull_image(cache_root: &Path, reference: &str, prod: bool) -> Result<CachedOciImage> {
    let image_ref: ImageReference = reference.parse()?;
    if prod && !image_ref.is_digest_pinned() {
        bail!("mvmctl image pull --prod requires a digest-pinned reference");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for OCI pull")?;

    let manifest_fetcher = OciManifestFetcher::new();
    let manifest = runtime
        .block_on(
            manifest_fetcher
                .fetch_linux_platform_manifest(&image_ref, &LinuxPlatform::for_current_arch()),
        )
        .context("fetch OCI image manifest")?;
    let layers = manifest.layers().context("parse OCI image layers")?;
    if layers.is_empty() {
        bail!(
            "OCI image manifest has no layers: {}",
            image_ref.canonical()
        );
    }

    let manifest_hex = sha256_hex(&manifest.digest)?;
    let manifest_path = format!("manifests/{manifest_hex}.json");
    write_cache_file(cache_root, &manifest_path, &manifest.bytes)?;

    let config_path = write_config_blob(
        cache_root,
        &runtime,
        &manifest_fetcher,
        &image_ref,
        &manifest.bytes,
    )?;
    let layer_fetcher =
        OciLayerFetcher::from_manifest_fetcher(&manifest_fetcher, LayerFetchOptions::default());
    let unpacked_root = cache_root.join("unpacked").join(&manifest_hex);
    if unpacked_root.exists() {
        fs::remove_dir_all(&unpacked_root)
            .with_context(|| format!("remove stale unpacked root {}", unpacked_root.display()))?;
    }
    fs::create_dir_all(&unpacked_root)
        .with_context(|| format!("create {}", unpacked_root.display()))?;

    let mut cached_layers = Vec::with_capacity(layers.len());
    for layer in &layers {
        let compressed =
            fetch_or_read_layer(cache_root, &runtime, &layer_fetcher, &image_ref, layer)
                .with_context(|| format!("fetch layer {}", layer.digest))?;
        unpack_layer_bytes(layer, &compressed, &unpacked_root)
            .with_context(|| format!("unpack layer {}", layer.digest))?;
        cached_layers.push(CachedOciLayer {
            digest: layer.digest.clone(),
            size_bytes: layer.size,
            path: Some(layer_blob_path(&layer.digest)?),
        });
    }

    let unpacked_size = unpacked_tree_size(&unpacked_root)
        .with_context(|| format!("measure unpacked root {}", unpacked_root.display()))?;
    let rootfs_path = format!("rootfs/{manifest_hex}/rootfs.ext4");
    let rootfs_abs = cache_root.join(&rootfs_path);
    materialize_ext4(
        &MaterializeExt4Input::new(unpacked_root, rootfs_abs, unpacked_size),
        &MaterializeExt4Options::default(),
    )
    .context("materialize OCI rootfs.ext4")?;

    let mut index = load_index(cache_root)?;
    let cached = CachedOciImage {
        reference: image_ref.canonical(),
        registry: image_ref.registry.clone(),
        repository: image_ref.repository.clone(),
        tag: image_ref.tag.clone(),
        resolved_digest: manifest.digest.clone(),
        fetched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        manifest_path,
        config_path,
        rootfs_path: Some(rootfs_path),
        claims_path: None,
        layers: cached_layers,
    };
    upsert_image(&mut index, cached.clone());
    save_index(cache_root, &index)?;
    Ok(cached)
}

fn write_config_blob(
    cache_root: &Path,
    runtime: &tokio::runtime::Runtime,
    manifest_fetcher: &OciManifestFetcher,
    image_ref: &ImageReference,
    manifest_bytes: &[u8],
) -> Result<Option<String>> {
    let Some(config) = manifest_config_descriptor(manifest_bytes)? else {
        return Ok(None);
    };
    let config_path = format!("configs/{}.json", sha256_hex(&config.digest)?);
    if let Some(bytes) = read_verified_cache_file(cache_root, &config_path, &config.digest)?
        && serde_json::from_slice::<Value>(&bytes).is_ok()
    {
        return Ok(Some(config_path));
    }

    let fetcher =
        OciLayerFetcher::from_manifest_fetcher(manifest_fetcher, LayerFetchOptions::default());
    let mut bytes = Vec::new();
    runtime
        .block_on(fetcher.fetch_layer(image_ref, &config, &mut bytes))
        .context("fetch OCI image config blob")?;
    write_cache_file(cache_root, &config_path, &bytes)?;
    Ok(Some(config_path))
}

fn manifest_config_descriptor(manifest_bytes: &[u8]) -> Result<Option<LayerDescriptor>> {
    let value: Value = serde_json::from_slice(manifest_bytes).context("parse manifest JSON")?;
    let Some(config) = value.get("config").and_then(Value::as_object) else {
        return Ok(None);
    };
    let digest = config
        .get("digest")
        .and_then(Value::as_str)
        .context("manifest config missing digest")?
        .to_string();
    let media_type = config
        .get("mediaType")
        .and_then(Value::as_str)
        .unwrap_or("application/vnd.oci.image.config.v1+json")
        .to_string();
    let size = config.get("size").and_then(Value::as_u64).unwrap_or(0);
    Ok(Some(LayerDescriptor {
        digest,
        size,
        media_type,
    }))
}

fn fetch_or_read_layer(
    cache_root: &Path,
    runtime: &tokio::runtime::Runtime,
    fetcher: &OciLayerFetcher,
    image_ref: &ImageReference,
    layer: &LayerDescriptor,
) -> Result<Vec<u8>> {
    let path = layer_blob_path(&layer.digest)?;
    if let Some(bytes) = read_verified_cache_file(cache_root, &path, &layer.digest)? {
        return Ok(bytes);
    }
    let mut bytes = Vec::new();
    runtime.block_on(fetcher.fetch_layer(image_ref, layer, &mut bytes))?;
    write_cache_file(cache_root, &path, &bytes)?;
    Ok(bytes)
}

fn unpack_layer_bytes(layer: &LayerDescriptor, bytes: &[u8], unpacked_root: &Path) -> Result<()> {
    let report = if is_gzip_layer(&layer.media_type) {
        unpack_layer(
            GzDecoder::new(Cursor::new(bytes)),
            unpacked_root,
            &UnpackOptions::default(),
        )
    } else {
        unpack_layer(Cursor::new(bytes), unpacked_root, &UnpackOptions::default())
    }?;
    if !report.refused.is_empty() {
        bail!("layer unpack refused entries: {:?}", report.refused);
    }
    Ok(())
}

fn is_gzip_layer(media_type: &str) -> bool {
    media_type.ends_with("+gzip")
        || media_type.ends_with(".gzip")
        || media_type.contains("tar.gzip")
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

fn read_verified_cache_file(
    cache_root: &Path,
    relative: &str,
    digest: &str,
) -> Result<Option<Vec<u8>>> {
    let path = safe_cache_path(cache_root, relative)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    verify_sha256_digest(&bytes, digest)
        .with_context(|| format!("verify cached blob {}", path.display()))?;
    Ok(Some(bytes))
}

fn write_cache_file(cache_root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let path = safe_cache_path(cache_root, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
}

fn layer_blob_path(digest: &str) -> Result<String> {
    Ok(format!("blobs/sha256/{}", sha256_hex(digest)?))
}

fn sha256_hex(digest: &str) -> Result<String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("unsupported digest algorithm in {digest:?}");
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("malformed sha256 digest: {digest:?}");
    }
    Ok(hex.to_string())
}

fn unpacked_tree_size(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("stat unpacked path {}", path.display()))?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
                stack.push(entry?.path());
            }
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn upsert_image(index: &mut OciCacheIndex, image: CachedOciImage) {
    if let Some(existing) = index
        .images
        .iter_mut()
        .find(|cached| cached.reference == image.reference)
    {
        *existing = image;
    } else {
        index.images.push(image);
    }
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
    fn prod_pull_requires_digest_pin_before_network() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = pull_image(tmp.path(), "docker.io/library/alpine:3.20", true)
            .expect_err("mutable prod pull must fail before registry access");
        assert!(
            err.to_string()
                .contains("requires a digest-pinned reference"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn manifest_config_descriptor_extracts_config_blob() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": digest,
                "size": 17,
            },
            "layers": [],
        });
        let descriptor =
            manifest_config_descriptor(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        let descriptor = descriptor.expect("config descriptor");
        assert_eq!(descriptor.digest, digest);
        assert_eq!(descriptor.size, 17);
    }

    #[test]
    fn upsert_replaces_existing_reference_entry() {
        let mut index = OciCacheIndex {
            schema_version: 1,
            images: vec![sample_image(
                "docker.io/library/alpine:3.20",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "blobs/old",
            )],
        };
        let replacement = sample_image(
            "docker.io/library/alpine:3.20",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blobs/new",
        );

        upsert_image(&mut index, replacement);

        assert_eq!(index.images.len(), 1);
        assert_eq!(index.images[0].layers[0].path.as_deref(), Some("blobs/new"));
    }

    #[test]
    fn upsert_keeps_distinct_references_to_same_digest() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut index = OciCacheIndex {
            schema_version: 1,
            images: vec![sample_image(
                "docker.io/library/alpine:3.20",
                digest,
                "blobs/shared",
            )],
        };
        let second = sample_image(
            &format!("docker.io/library/alpine@{digest}"),
            digest,
            "blobs/shared",
        );

        upsert_image(&mut index, second);

        assert_eq!(index.images.len(), 2);
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
