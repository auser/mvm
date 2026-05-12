//! `mvmctl bundle fetch <path>` — verify a `.mvmpkg` archive
//! against the local trust store.
//!
//! Scope for the substrate close-out is verification only: reads
//! the archive, looks the publisher pubkey up via
//! [`FsTrustStore`], re-verifies the signature, and re-hashes
//! every artifact against the signed manifest. Reports the parsed
//! manifest on success.
//!
//! Replacing the local template registry's manifest-path-hash
//! keying with bundle-sha256 directories is a follow-up — it
//! interacts with the existing `~/.mvm/templates/<hash>/...` flow
//! and deserves its own commit.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use mvm_core::user_config::MvmConfig;
use mvm_plan::bundle::{FsTrustStore, bundle_sha256, read_and_verify_bundle};

use super::super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Path to a `.mvmpkg` archive on disk. (HTTP URLs are a
    /// follow-up — see module docs.)
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Override the trust store directory. Defaults to
    /// `~/.mvm/trusted-publishers/`.
    #[arg(long, value_name = "DIR")]
    pub trust_store: Option<PathBuf>,
    /// Output the verified manifest as JSON instead of a
    /// human-readable summary.
    #[arg(long)]
    pub json: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let bytes = std::fs::read(&args.path)
        .with_context(|| format!("reading bundle archive at {}", args.path.display()))?;

    let trust = match args.trust_store {
        Some(p) => FsTrustStore::new(p),
        None => FsTrustStore::default_path()
            .context("resolving default trust-store path (~/.mvm/trusted-publishers/)")?,
    };

    let verified = read_and_verify_bundle(&bytes, &trust)
        .with_context(|| format!("verifying bundle at {}", args.path.display()))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&verified.manifest)?);
    } else {
        let summary = BundleSummary {
            bundle_sha256: bundle_sha256(&bytes),
            key_id: verified.key_id.0.clone(),
            publisher: verified.manifest.publisher.clone(),
            arch: verified.manifest.arch.clone(),
            profile: verified.manifest.profile.clone(),
            workload_label: verified.manifest.workload_label.clone(),
            artifact_count: verified.manifest.artifacts.len(),
            has_verity: verified.manifest.verity.is_some(),
        };
        summary.render();
    }
    Ok(())
}

struct BundleSummary {
    bundle_sha256: String,
    key_id: String,
    publisher: String,
    arch: String,
    profile: Option<String>,
    workload_label: Option<String>,
    artifact_count: usize,
    has_verity: bool,
}

impl BundleSummary {
    fn render(&self) {
        println!("Bundle verified");
        println!("  sha256:    {}", self.bundle_sha256);
        println!("  key_id:    {}", self.key_id);
        println!("  publisher: {}", self.publisher);
        println!("  arch:      {}", self.arch);
        if let Some(p) = &self.profile {
            println!("  profile:   {p}");
        }
        if let Some(l) = &self.workload_label {
            println!("  label:     {l}");
        }
        println!("  artifacts: {}", self.artifact_count);
        println!(
            "  verity:    {}",
            if self.has_verity { "yes" } else { "no" }
        );
    }
}
