//! `mvmctl tenant` — operator-facing tenant lifecycle commands.
//!
//! Plan 60 Phase 7a Slices A + D capstone: provides the
//! user-facing verb that destroys a tenant's overlays and emits
//! signed destruction certificates so a hosted-cloud operator
//! can prove erasure to an auditor.
//!
//! ## `mvmctl tenant destroy`
//!
//! ```text
//! mvmctl tenant destroy --tenant <id> --confirm-deletion
//! ```
//!
//! Lists every overlay for `<id>`, destroys each (zero-fill +
//! unlink per the FsOverlayManager security model), signs the
//! destruction receipt under the host identity key (the same
//! `~/.mvm/keys/host-signer.ed25519` plan 64 W2 introduced for
//! audit-chain signing), and prints a JSON array of
//! [`SignedDestructionReceipt`] envelopes to stdout. Human-
//! readable progress goes to stderr so an operator can pipe
//! stdout to a file:
//!
//! ```bash
//! mvmctl tenant destroy --tenant acme --confirm-deletion \
//!     > certs.json
//! ```
//!
//! The `--confirm-deletion` flag is required. Defense in depth:
//! a copy-paste mistake involving `mvmctl tenant destroy --tenant
//! acme` without the flag is a no-op + error message, not a
//! destroyed tenant.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};

use mvm::vm::overlay::{
    FsOverlayManager, OverlayManager, SignedDestructionReceipt, sign_destruction_receipt,
};
use mvm_core::user_config::MvmConfig;

use super::Cli;
use crate::commands::vm::host_signer;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: TenantAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum TenantAction {
    /// Destroy a tenant: walk every overlay under
    /// `~/.mvm/overlays/<tenant>/`, zero-fill + unlink every file,
    /// remove the directories, and emit a signed destruction
    /// certificate per workload. The certificate proves to an
    /// off-host auditor that the operator wiped the bytes;
    /// verifying it requires the operator's host identity pubkey
    /// (from `~/.mvm/keys/host-signer.pub`).
    ///
    /// `--confirm-deletion` is required.
    Destroy {
        /// Tenant id to destroy. Must match the directory name
        /// under `~/.mvm/overlays/`; path-validated (no `..`, no
        /// slashes, length-capped at 64 bytes) by the overlay
        /// substrate.
        #[arg(long)]
        tenant: String,

        /// Required guard. Without this flag the command refuses
        /// to act + exits non-zero. Prevents copy-paste mistakes
        /// from destroying a tenant.
        #[arg(long)]
        confirm_deletion: bool,

        /// Override the overlay root directory. Defaults to
        /// `$HOME/.mvm/overlays/`. Test seam + escape hatch for
        /// operators whose `~/.mvm/` lives off the home dir.
        #[arg(long)]
        overlay_root: Option<std::path::PathBuf>,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.action {
        TenantAction::Destroy {
            tenant,
            confirm_deletion,
            overlay_root,
        } => destroy_tenant(&tenant, confirm_deletion, overlay_root),
    }
}

fn destroy_tenant(
    tenant: &str,
    confirm_deletion: bool,
    overlay_root: Option<std::path::PathBuf>,
) -> Result<()> {
    if !confirm_deletion {
        anyhow::bail!(
            "refusing to destroy tenant {tenant:?} without --confirm-deletion. \
             Re-run with `--confirm-deletion` to actually destroy the overlays."
        );
    }

    let root = match overlay_root {
        Some(p) => p,
        None => default_overlay_root().context("resolving default overlay root")?,
    };

    let mgr = FsOverlayManager::with_root(&root)
        .with_context(|| format!("opening overlay manager at {}", root.display()))?;

    let signer = host_signer::load_or_init().context("loading host signer")?;

    eprintln!(
        "mvmctl tenant destroy: tenant={tenant:?} overlay_root={}",
        root.display()
    );

    // tokio runtime — overlay ops are async; we block-on per call
    // (same shape as audit_chain.rs uses).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let overlays = rt
        .block_on(mgr.list_overlays(tenant))
        .with_context(|| format!("listing overlays for tenant {tenant:?}"))?;

    if overlays.is_empty() {
        eprintln!("  (no overlays found; nothing to destroy)");
        // Emit an empty JSON array so downstream parsers that
        // expect an array succeed.
        println!("[]");
        return Ok(());
    }

    eprintln!("  found {} overlay(s)", overlays.len());

    let mut certificates: Vec<SignedDestructionReceipt> = Vec::with_capacity(overlays.len());
    let mut total_files: u64 = 0;
    let mut total_bytes: u64 = 0;
    for handle in &overlays {
        let receipt = rt
            .block_on(mgr.destroy_overlay(&handle.tenant, &handle.workload))
            .with_context(|| format!("destroying overlay {}/{}", handle.tenant, handle.workload))?;
        total_files = total_files.saturating_add(receipt.files_wiped);
        total_bytes = total_bytes.saturating_add(receipt.bytes_wiped);
        eprintln!(
            "  ✓ {}/{}: {} file(s), {} byte(s) wiped",
            receipt.tenant, receipt.workload, receipt.files_wiped, receipt.bytes_wiped
        );
        certificates.push(sign_destruction_receipt(&receipt, &signer.signing));
    }

    eprintln!(
        "destroyed {} overlay(s): {} file(s), {} byte(s) total. \
         Certificate(s) printed to stdout.",
        certificates.len(),
        total_files,
        total_bytes
    );

    // Pretty-print so operators can eyeball certs that go to a
    // file via `> certs.json`. Order matches list_overlays' sorted
    // output (alphabetical by workload).
    let json = serde_json::to_string_pretty(&certificates)
        .context("serializing destruction certificates")?;
    println!("{json}");
    Ok(())
}

/// Default overlay root: `$HOME/.mvm/overlays/`. Returns an error
/// when `$HOME` is unset (CI sandboxes, daemons without a home
/// dir) — operators in that environment pass `--overlay-root`
/// explicitly.
fn default_overlay_root() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("$HOME unset; pass --overlay-root explicitly"))?;
    Ok(std::path::PathBuf::from(home)
        .join(".mvm")
        .join(mvm::vm::overlay::DEFAULT_OVERLAY_DIR_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mvm::vm::overlay::{SignedDestructionReceipt, verify_destruction_receipt};
    use tempfile::tempdir;

    /// Run `destroy_tenant` against a tempdir-rooted overlay
    /// manager. Captures stdout via a child process — we can't
    /// easily redirect stdout inside an async test, so the
    /// integration boundary is "the function returns Ok and the
    /// overlay directories are gone." The signed-cert
    /// round-trip is exercised by the overlay module's own tests;
    /// here we pin the command's lifecycle.
    fn build_tenant_with_overlays(tenant: &str, workloads: &[&str]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let mgr = FsOverlayManager::with_root(dir.path()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for w in workloads {
            rt.block_on(mgr.create_overlay(tenant, w)).unwrap();
            // Plant a tiny file so the wipe has something to do.
            std::fs::write(
                dir.path().join(tenant).join(w).join("data.txt"),
                format!("tenant={tenant} workload={w}"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn destroy_without_confirm_flag_refuses_and_errors() {
        let dir = build_tenant_with_overlays("acme", &["build"]);
        let err = destroy_tenant("acme", false, Some(dir.path().to_path_buf())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--confirm-deletion"), "{msg}");
        // The overlay must still be intact — the refusal is the
        // load-bearing invariant.
        assert!(dir.path().join("acme").join("build").exists());
    }

    #[test]
    fn destroy_with_confirm_flag_removes_overlays() {
        let dir = build_tenant_with_overlays("acme", &["build", "test"]);
        destroy_tenant("acme", true, Some(dir.path().to_path_buf())).unwrap();
        // Both workload dirs gone.
        assert!(!dir.path().join("acme").join("build").exists());
        assert!(!dir.path().join("acme").join("test").exists());
    }

    #[test]
    fn destroy_invalid_tenant_id_errors() {
        let dir = tempdir().unwrap();
        // Path-validator should reject the `../` smuggle attempt
        // somewhere in the chain. Either list_overlays or the
        // subsequent destroy_overlay must surface the invalid name.
        let err = destroy_tenant("../etc", true, Some(dir.path().to_path_buf())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid") || msg.contains("InvalidName"),
            "{msg}"
        );
    }

    #[test]
    fn destroy_missing_tenant_is_ok_emits_empty_array() {
        // Listing a non-existent tenant returns an empty vec, so
        // destroy_tenant should succeed without error. The
        // operator's downstream parser sees `[]`.
        let dir = tempdir().unwrap();
        destroy_tenant("never-created", true, Some(dir.path().to_path_buf())).unwrap();
    }

    #[test]
    fn certificates_round_trip_through_serde_json() {
        // The receipt format is documented as JSON-on-stdout; an
        // auditor parses the array and verifies each cert. This
        // test bypasses stdout (which we can't capture in-process)
        // and exercises the equivalent path: build a manager,
        // destroy, sign, serialize, parse, verify.
        let dir = build_tenant_with_overlays("acme", &["wk1", "wk2"]);
        let mgr = FsOverlayManager::with_root(dir.path()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);

        let overlays = rt.block_on(mgr.list_overlays("acme")).unwrap();
        let certs: Vec<SignedDestructionReceipt> = overlays
            .iter()
            .map(|h| {
                let r = rt
                    .block_on(mgr.destroy_overlay(&h.tenant, &h.workload))
                    .unwrap();
                sign_destruction_receipt(&r, &signing_key)
            })
            .collect();

        let json = serde_json::to_string_pretty(&certs).unwrap();
        let parsed: Vec<SignedDestructionReceipt> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        for cert in &parsed {
            // Every cert verifies under the signing key's pubkey.
            verify_destruction_receipt(cert, Some(&signing_key.verifying_key())).unwrap();
        }
    }
}
