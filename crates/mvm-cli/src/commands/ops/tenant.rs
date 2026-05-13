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
    FsOverlayManager, OverlayManager, SignedDestructionReceipt, cert_fingerprint,
    sign_destruction_receipt,
};
use mvm_core::user_config::MvmConfig;
use mvm_supervisor::{EventCategory, Recorder};

use super::Cli;
use crate::commands::cmd_audit;
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

/// Plan 60 Phase 7a — audit-chain cross-reference event. Each
/// successful workload destroy emits one chain-signed entry under
/// [`EventCategory::Lifecycle`] with the per-workload fields plus
/// the cert's SHA-256 fingerprint. An auditor holding both the
/// chain (via `mvmctl audit show`) and the on-disk certs can
/// confirm they refer to the same event by recomputing the
/// fingerprint via [`mvm::vm::overlay::cert_fingerprint`].
fn emit_chain_event(
    recorder: &Recorder,
    signed: &SignedDestructionReceipt,
    rt: &tokio::runtime::Runtime,
) {
    let fingerprint = cert_fingerprint(signed);
    let extras: Vec<(String, String)> = vec![
        ("tenant".to_string(), signed.receipt.tenant.clone()),
        ("workload".to_string(), signed.receipt.workload.clone()),
        (
            "files_wiped".to_string(),
            signed.receipt.files_wiped.to_string(),
        ),
        (
            "bytes_wiped".to_string(),
            signed.receipt.bytes_wiped.to_string(),
        ),
        ("cert_fingerprint".to_string(), fingerprint),
    ];
    // Best-effort emission. The certificate is the load-bearing
    // evidence; the chain anchor is a cross-reference. If the
    // chain emission fails (signer wedged, audit dir not
    // writable, …), the destruction still succeeded — log the
    // failure but don't fail the operator's command.
    if let Err(e) = rt.block_on(recorder.record_unbound(
        EventCategory::Lifecycle,
        "lifecycle.tenant.destroyed",
        extras,
    )) {
        tracing::warn!(
            error = %e,
            tenant = %signed.receipt.tenant,
            workload = %signed.receipt.workload,
            "lifecycle.tenant.destroyed chain emission failed; certificate \
             is still valid as standalone evidence"
        );
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

    // Audit-chain cross-reference recorder. Best-effort — see
    // `emit_chain_event`'s doc.
    let chain_recorder = cmd_audit::build_cmd_recorder();

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
        let signed = sign_destruction_receipt(&receipt, &signer.signing);
        if let Some(ref rec) = chain_recorder {
            emit_chain_event(rec, &signed, &rt);
        }
        certificates.push(signed);
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

    // ──────────────────────────────────────────────────────────
    // Audit chain emission
    //
    // `emit_chain_event` is the load-bearing piece; we exercise
    // it directly with a CapturingAuditSigner so the assertion
    // is self-contained (the integration with the real signer
    // is covered by the existing host_signer + audit_chain
    // tests, and by the operator workflow at runtime).
    // ──────────────────────────────────────────────────────────

    use mvm::vm::overlay::{DestructionReceipt, cert_fingerprint, sign_destruction_receipt};
    use mvm_plan::TenantId;
    use mvm_supervisor::CapturingAuditSigner;
    use std::sync::Arc;

    fn capturing_recorder() -> (Recorder, Arc<CapturingAuditSigner>) {
        let signer = Arc::new(CapturingAuditSigner::new());
        let rec = Recorder::new(signer.clone(), TenantId("local".to_string()));
        (rec, signer)
    }

    fn synthetic_signed() -> SignedDestructionReceipt {
        let receipt = DestructionReceipt {
            tenant: "acme".to_string(),
            workload: "build-runner".to_string(),
            destroyed_at: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                .unwrap(),
            files_wiped: 42,
            bytes_wiped: 1024,
        };
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        sign_destruction_receipt(&receipt, &key)
    }

    #[test]
    fn emit_chain_event_writes_lifecycle_event_with_canonical_labels() {
        let (rec, signer) = capturing_recorder();
        let signed = synthetic_signed();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        emit_chain_event(&rec, &signed, &rt);

        let entries = signer.entries();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.event, "lifecycle.tenant.destroyed");
        assert_eq!(entry.labels.get("tenant").map(String::as_str), Some("acme"));
        assert_eq!(
            entry.labels.get("workload").map(String::as_str),
            Some("build-runner")
        );
        assert_eq!(
            entry.labels.get("files_wiped").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            entry.labels.get("bytes_wiped").map(String::as_str),
            Some("1024")
        );
    }

    #[test]
    fn emit_chain_event_carries_matching_cert_fingerprint() {
        // The chain entry's `cert_fingerprint` label must match
        // the value `cert_fingerprint(&signed)` returns — that's
        // the load-bearing cross-reference. An auditor with the
        // chain entry + the certificate computes
        // cert_fingerprint(parsed_cert) and asserts equality.
        let (rec, signer) = capturing_recorder();
        let signed = synthetic_signed();
        let expected = cert_fingerprint(&signed);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        emit_chain_event(&rec, &signed, &rt);

        let entries = signer.entries();
        assert_eq!(
            entries[0]
                .labels
                .get("cert_fingerprint")
                .map(String::as_str),
            Some(expected.as_str())
        );
        // Fingerprint shape sanity.
        assert_eq!(expected.len(), 64); // SHA-256 hex
    }

    #[test]
    fn emit_chain_event_fingerprint_differs_per_cert() {
        // Two different workload destroys produce two different
        // fingerprints in the chain — operators don't get a
        // single chain entry that could be replayed against a
        // different cert.
        let (rec, signer) = capturing_recorder();
        let signed_a = synthetic_signed();
        let mut signed_b = signed_a.clone();
        signed_b.receipt.workload = "code-eval".to_string();
        // Re-sign with the same key so the cert is internally
        // consistent; only the labels + fingerprint change.
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        signed_b = sign_destruction_receipt(&signed_b.receipt, &key);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        emit_chain_event(&rec, &signed_a, &rt);
        emit_chain_event(&rec, &signed_b, &rt);

        let entries = signer.entries();
        assert_eq!(entries.len(), 2);
        let fp_a = entries[0].labels.get("cert_fingerprint").unwrap();
        let fp_b = entries[1].labels.get("cert_fingerprint").unwrap();
        assert_ne!(fp_a, fp_b, "fingerprints must differ per cert");
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
