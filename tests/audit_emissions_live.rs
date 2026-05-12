//! Plan 60 Phase 4 — live drive-and-assert tests for the
//! `AuditPosture::Emits` rows in `tests/audit_total_coverage.rs`.
//!
//! The classification scaffold in `audit_total_coverage.rs`
//! declares which subcommands MUST emit an audit entry on success.
//! This file *executes* a handful of the easiest-to-fixture
//! subcommands end-to-end and asserts the named `LocalAuditKind`
//! actually appears in the audit log.
//!
//! Coverage today (intentionally minimal; expand per-row as commands
//! gain hermetic fixtures):
//!
//! - `mvmctl cache prune` → `CachePrune`
//! - `mvmctl cache prune --dry-run` → **no** audit entry
//!   (dry-runs are read-only; pinning the negative)
//! - `mvmctl network create <name>` → `NetworkCreate`
//!
//! ## Hermetic setup
//!
//! Each test spawns the real `mvmctl` binary via `assert_cmd` with
//! `HOME` and `MVM_DATA_DIR` / `MVM_STATE_DIR` / `MVM_CACHE_DIR`
//! pointed at a per-test `tempfile::tempdir()`. The audit log
//! resolves to `<tempdir>/.local/state/mvm/log/audit.jsonl` (the
//! XDG-state path `mvm_core::policy::audit::default_audit_log()`
//! returns when no legacy `~/.mvm/log/` exists). Tests read that
//! file and assert the expected `LocalAuditKind` (in its
//! `serde(rename_all = "snake_case")` form, e.g. `"cache_prune"`)
//! appears.
//!
//! ## Why subprocess, not in-process
//!
//! `mvm_core::audit::emit` writes to a path computed from env vars
//! at call time. Running the command in-process and asserting on
//! the audit file would either need a process-global env mutex
//! (brittle under parallel `cargo test`) or in-process emit-to-path
//! plumbing. The subprocess gets its own env, which is naturally
//! hermetic across `cargo test`'s default thread-per-test
//! parallelism.

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A test sandbox: tempdir + the env vars wired to point every
/// mvmctl state path inside it.
struct AuditSandbox {
    home: TempDir,
}

impl AuditSandbox {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    /// Resolve the audit log path the subprocess will write to.
    /// `mvm_core::policy::audit::default_audit_log()` returns the
    /// XDG state path (`<state>/log/audit.jsonl`) when no legacy
    /// `<data>/log/audit.jsonl` exists. Since the tempdir is empty,
    /// the state path wins.
    fn audit_log_path(&self) -> PathBuf {
        self.home_path()
            .join(".local")
            .join("state")
            .join("mvm")
            .join("log")
            .join("audit.jsonl")
    }

    /// Build a Command pre-wired with `HOME` overridden so every
    /// mvmctl-derived path lands inside the sandbox.
    fn mvmctl(&self) -> Command {
        #[allow(deprecated)]
        let mut c = Command::cargo_bin("mvmctl").expect("cargo_bin mvmctl");
        // HOME drives every state dir helper in mvm_core::config —
        // mvm_data_dir, mvm_state_dir, mvm_cache_dir, mvm_share_dir,
        // mvm_config_dir all cascade off it when no XDG_* / MVM_*
        // override is set. Clearing those guarantees HOME wins so
        // the test runner's own XDG env doesn't leak into the
        // subprocess.
        c.env("HOME", self.home_path())
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MVM_DATA_DIR")
            .env_remove("MVM_STATE_DIR")
            .env_remove("MVM_CACHE_DIR")
            .env_remove("MVM_SHARE_DIR")
            .env_remove("MVM_CONFIG_DIR");
        c
    }
}

/// Read the audit log into a string. Returns "" if the file doesn't
/// exist (an unaudited call leaves no file behind).
fn read_audit_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Count occurrences of `serde(rename_all = "snake_case")` form of
/// `kind` in the audit log. The on-disk JSON shape is
/// `{"kind":"cache_prune", ...}`, so a `kind` of `"cache_prune"`
/// matches one entry per occurrence.
fn count_entries_with_kind(log: &str, kind: &str) -> usize {
    let needle = format!("\"kind\":\"{kind}\"");
    log.matches(&needle).count()
}

#[test]
fn cache_prune_emits_cache_prune_audit_entry() {
    let sandbox = AuditSandbox::new();

    // Run `mvmctl cache prune` against an empty cache dir. The
    // command short-circuits with "Cache directory does not exist"
    // but still emits the audit entry — Plan 37 §6 invariant: every
    // state-changing CLI verb emits one record per attempt, success
    // or no-op.
    let output = sandbox
        .mvmctl()
        .args(["cache", "prune"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl cache prune failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "cache_prune");
    assert!(
        hits >= 1,
        "expected ≥1 cache_prune entry in audit log, got {hits}. \
         Full log content:\n{log}"
    );
}

#[test]
fn cache_prune_dry_run_does_not_emit_audit_entry() {
    // Pinning the negative: dry-run is read-only and must NOT
    // leave an audit entry. If this test fails, the dry-run path
    // grew an emission it shouldn't have.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["cache", "prune", "--dry-run"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl cache prune --dry-run failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "cache_prune");
    assert_eq!(
        hits, 0,
        "dry-run must not write audit entries, got {hits} cache_prune \
         entry/entries. Full log:\n{log}"
    );
}

#[test]
fn network_create_emits_network_create_audit_entry() {
    let sandbox = AuditSandbox::new();

    let output = sandbox
        .mvmctl()
        .args(["network", "create", "test-audit-net"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl network create failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "network_create");
    assert!(
        hits >= 1,
        "expected ≥1 network_create entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}
