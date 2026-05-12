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
//! - `mvmctl network remove <name>` → `NetworkRemove`
//! - `mvmctl manifest prune --orphans` (empty registry) → `SlotPrune`
//!   (emitted with `count=0` — Plan 37 §6 invariant: every state-
//!   changing verb emits one record per attempt, even on no-op)
//! - `mvmctl manifest prune --orphans --dry-run` → **no** audit entry
//! - `mvmctl storage gc --apply --mock` (empty pool) → `StorageGc`
//!   (no-op attempt emits with `count=0` / `pool_unavailable=…`)
//! - `mvmctl storage gc --mock` (dry-run) → **no** audit entry
//! - `mvmctl manifest tag add <tpl> <tag>` → `ManifestTagAdd`
//! - `mvmctl manifest tag rm <tpl> <tag>` → `ManifestTagRemove`
//! - `mvmctl manifest tag ls <tpl>` → **no** audit entry
//! - `mvmctl manifest alias set <tpl> <alias> <rev>` → `ManifestAliasSet`
//! - `mvmctl manifest alias rm <tpl> <alias>` → `ManifestAliasRemove`
//! - `mvmctl manifest alias ls <tpl>` → **no** audit entry
//! - `mvmctl secret put / get / ls / rm` → secret-side audit JSONL
//!   at `~/.mvm/audit/secrets.jsonl` carries one entry per call
//!   with `"action":"put"` / `"get"` / `"list"` / `"delete"`. The
//!   CLI verb and on-disk action name are decoupled — `ls` →
//!   `"list"`, `rm` → `"delete"` — so the negative tests also pin
//!   the rename surface against accidental drift. CI provides the
//!   Linux Secret Service via dbus-run-session + gnome-keyring
//!   (see `.github/workflows/ci.yml`); macOS uses the system
//!   Keychain natively.
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

    /// The `mvmctl secret` command writes its own per-action JSONL
    /// to `~/.mvm/audit/secrets.jsonl` (distinct from the
    /// `LocalAudit` stream). Entries have shape
    /// `{"action":"put","tenant":"...","name":"...","outcome":"ok",...}`.
    fn secret_audit_log_path(&self) -> PathBuf {
        self.home_path()
            .join(".mvm")
            .join("audit")
            .join("secrets.jsonl")
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

#[test]
fn network_remove_emits_network_remove_audit_entry() {
    // Create a network first, then remove it. Two audit entries
    // are expected: one `network_create`, one `network_remove`.
    let sandbox = AuditSandbox::new();

    let create = sandbox
        .mvmctl()
        .args(["network", "create", "test-rm-audit-net"])
        .output()
        .expect("spawn mvmctl create");
    assert!(
        create.status.success(),
        "create failed: stderr={}",
        String::from_utf8_lossy(&create.stderr)
    );

    let remove = sandbox
        .mvmctl()
        .args(["network", "remove", "test-rm-audit-net"])
        .output()
        .expect("spawn mvmctl remove");
    assert!(
        remove.status.success(),
        "remove failed: stderr={}",
        String::from_utf8_lossy(&remove.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "network_remove");
    assert!(
        hits >= 1,
        "expected ≥1 network_remove entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn manifest_prune_orphans_emits_slot_prune_audit_entry() {
    // Plan 37 §6 invariant: a state-changing verb emits one audit
    // record per attempt, even when the body of work is a no-op.
    // Running `manifest prune --orphans` against an empty registry
    // walks zero slots but still emits one `slot_prune` entry.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["manifest", "prune", "--orphans"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest prune --orphans failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "slot_prune");
    assert!(
        hits >= 1,
        "expected ≥1 slot_prune entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn manifest_prune_orphans_dry_run_does_not_emit_audit_entry() {
    // Negative complement to `manifest_prune_orphans_emits_slot_prune…`.
    // Plan 37 §6 says state-changing verbs emit on every attempt; the
    // dry-run path is read-only by contract and must NOT emit. The
    // implementation routes dry-run to `run_dry` (manifest/prune.rs)
    // which returns before reaching the `audit::emit` call — this
    // test pins that against a future regression that moves the
    // emit above the dry-run branch.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["manifest", "prune", "--orphans", "--dry-run"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest prune --orphans --dry-run failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "slot_prune");
    assert_eq!(
        hits, 0,
        "dry-run must not write audit entries, got {hits} slot_prune \
         entry/entries. Full log:\n{log}"
    );
}

#[test]
fn storage_gc_apply_emits_storage_gc_audit_entry_even_on_empty_pool() {
    // Plan 37 §6 invariant: a state-changing verb emits one audit
    // record per attempt, even when the body of work is a no-op.
    // Running `mvmctl storage gc --apply --mock` against a fresh
    // in-memory MockBackend lists zero volumes — but `--apply`
    // is the operator's commit signal, so the attempt must still
    // surface in the audit log. Failure here means the empty-pool
    // early-return in storage/gc.rs is skipping the emit.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["storage", "gc", "--apply", "--mock"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl storage gc --apply --mock failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "storage_gc");
    assert!(
        hits >= 1,
        "expected ≥1 storage_gc entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn storage_gc_dry_run_does_not_emit_audit_entry() {
    // Negative complement: dry-run is read-only and must not emit.
    // Plain `mvmctl storage gc --mock` (no `--apply`) is the dry-run
    // surface — pin it as a no-emit invariant against a future
    // regression that elevates the dry-run path into the emit branch.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["storage", "gc", "--mock"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl storage gc --mock failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "storage_gc");
    assert_eq!(
        hits, 0,
        "dry-run must not write audit entries, got {hits} storage_gc \
         entry/entries. Full log:\n{log}"
    );
}

#[test]
fn manifest_tag_add_emits_manifest_tag_add_audit_entry() {
    // `manifest tag add <template> <tag>` writes to
    // `~/.mvm/templates/<template>/tags.json` and emits
    // `ManifestTagAdd`. `TemplateTags::load` is forgiving — missing
    // templates yield an empty catalog — so the test runs against a
    // fresh sandbox without any pre-existing slot.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["manifest", "tag", "add", "test-tmpl", "live-test-tag"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest tag add failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "manifest_tag_add");
    assert!(
        hits >= 1,
        "expected ≥1 manifest_tag_add entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn manifest_tag_rm_emits_manifest_tag_remove_audit_entry() {
    // Add a tag, then remove it. Two audit entries expected — one
    // `manifest_tag_add`, one `manifest_tag_remove` — but this test
    // pins only the remove half (the add half has its own test
    // above).
    let sandbox = AuditSandbox::new();
    let add = sandbox
        .mvmctl()
        .args(["manifest", "tag", "add", "test-tmpl", "to-remove"])
        .output()
        .expect("spawn mvmctl add");
    assert!(
        add.status.success(),
        "mvmctl manifest tag add failed: stderr={}",
        String::from_utf8_lossy(&add.stderr)
    );
    let rm = sandbox
        .mvmctl()
        .args(["manifest", "tag", "rm", "test-tmpl", "to-remove"])
        .output()
        .expect("spawn mvmctl rm");
    assert!(
        rm.status.success(),
        "mvmctl manifest tag rm failed: stderr={}",
        String::from_utf8_lossy(&rm.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "manifest_tag_remove");
    assert!(
        hits >= 1,
        "expected ≥1 manifest_tag_remove entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn manifest_tag_ls_does_not_emit_audit_entry() {
    // Negative complement: `manifest tag ls` is read-only and must
    // NOT emit. Pins the `MANIFEST_TAG` table's `ReadOnly` row in
    // `tests/audit_total_coverage.rs` against a future regression
    // that adds an emit to the list path.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["manifest", "tag", "ls", "test-tmpl"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest tag ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let add_hits = count_entries_with_kind(&log, "manifest_tag_add");
    let rm_hits = count_entries_with_kind(&log, "manifest_tag_remove");
    assert_eq!(
        add_hits + rm_hits,
        0,
        "read-only `manifest tag ls` must not emit; got {add_hits} add \
         and {rm_hits} remove entries. Full log:\n{log}"
    );
}

#[test]
fn manifest_alias_rm_emits_manifest_alias_remove_audit_entry() {
    // Set an alias, then remove it. Pins the remove half of the
    // alias subgroup against a future regression that swaps the
    // emit kind or drops it entirely.
    let sandbox = AuditSandbox::new();
    let set = sandbox
        .mvmctl()
        .args([
            "manifest",
            "alias",
            "set",
            "test-tmpl",
            "to-remove",
            "abc123def456abc123def456abc123de",
        ])
        .output()
        .expect("spawn mvmctl set");
    assert!(
        set.status.success(),
        "mvmctl manifest alias set failed: stderr={}",
        String::from_utf8_lossy(&set.stderr)
    );
    let rm = sandbox
        .mvmctl()
        .args(["manifest", "alias", "rm", "test-tmpl", "to-remove"])
        .output()
        .expect("spawn mvmctl rm");
    assert!(
        rm.status.success(),
        "mvmctl manifest alias rm failed: stderr={}",
        String::from_utf8_lossy(&rm.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "manifest_alias_remove");
    assert!(
        hits >= 1,
        "expected ≥1 manifest_alias_remove entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn manifest_alias_ls_does_not_emit_audit_entry() {
    // Negative complement: `manifest alias ls` is read-only. Pins
    // the `MANIFEST_ALIAS` table's `ls → ReadOnly` row against a
    // future regression that adds an emit to the list path.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["manifest", "alias", "ls", "test-tmpl"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest alias ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let set_hits = count_entries_with_kind(&log, "manifest_alias_set");
    let rm_hits = count_entries_with_kind(&log, "manifest_alias_remove");
    assert_eq!(
        set_hits + rm_hits,
        0,
        "read-only `manifest alias ls` must not emit; got {set_hits} set \
         and {rm_hits} remove entries. Full log:\n{log}"
    );
}

#[test]
fn manifest_alias_set_emits_manifest_alias_set_audit_entry() {
    // `manifest alias set <template> <alias> <rev>` writes to the
    // same `tags.json` and emits `ManifestAliasSet`. Same
    // forgiving-load story as `manifest tag add` above.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args([
            "manifest",
            "alias",
            "set",
            "test-tmpl",
            "latest",
            "abc123def456abc123def456abc123de",
        ])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest alias set failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "manifest_alias_set");
    assert!(
        hits >= 1,
        "expected ≥1 manifest_alias_set entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
}

/// Common setup: put a secret into the sandbox so subsequent
/// `get` / `ls` / `rm` have something to operate on.
fn put_a_secret(sandbox: &AuditSandbox, tenant: &str, name: &str, value: &str) {
    let output = sandbox
        .mvmctl()
        .args(["secret", "put", name, "--tenant", tenant, "--value", value])
        .output()
        .expect("spawn mvmctl put");
    assert!(
        output.status.success(),
        "secret put pre-step failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn secret_put_emits_put_action_in_secret_audit_log() {
    // The `mvmctl secret` command writes per-action JSONL to a
    // separate audit file (`~/.mvm/audit/secrets.jsonl`); the
    // shape is `{"action":"put","tenant":...,"name":...,"outcome":"ok",...}`.
    // This pins the entry shape so a regression that flips
    // "action" → "verb" or relocates the file gets caught.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args([
            "secret",
            "put",
            "api-key",
            "--tenant",
            "test-tenant",
            "--value",
            "deadbeef",
        ])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl secret put failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(sandbox.secret_audit_log_path()).unwrap_or_default();
    assert!(
        log.contains("\"action\":\"put\""),
        "expected an 'action':'put' entry in secrets audit log. Full log:\n{log}"
    );
    assert!(
        log.contains("\"tenant\":\"test-tenant\""),
        "audit entry must record the tenant. Full log:\n{log}"
    );
    assert!(
        log.contains("\"outcome\":\"ok\""),
        "audit entry must record outcome=ok on success. Full log:\n{log}"
    );
}

#[test]
fn secret_get_emits_get_action_in_secret_audit_log() {
    // Put first, then get with `--force` to bypass the TTY guard
    // (subprocess stdout is a pipe, not a TTY — the guard would
    // otherwise refuse). Assert a `get` entry lands in the
    // per-action audit JSONL.
    let sandbox = AuditSandbox::new();
    put_a_secret(&sandbox, "test-tenant", "api-key", "deadbeef");

    let output = sandbox
        .mvmctl()
        .args([
            "secret",
            "get",
            "api-key",
            "--tenant",
            "test-tenant",
            "--force",
        ])
        .output()
        .expect("spawn mvmctl get");
    assert!(
        output.status.success(),
        "mvmctl secret get failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(sandbox.secret_audit_log_path()).unwrap_or_default();
    assert!(
        log.contains("\"action\":\"get\""),
        "expected an 'action':'get' entry in secrets audit log. Full log:\n{log}"
    );
}

#[test]
fn secret_ls_emits_list_action_in_secret_audit_log() {
    // The clap verb is `ls` but `cmd_ls` records `action:"list"`
    // on-disk. The audit JSONL's `action` field is the *operation
    // name*, not the CLI verb. Pin both — flipping either side
    // without updating this test would mask a real audit shape
    // change.
    let sandbox = AuditSandbox::new();
    put_a_secret(&sandbox, "test-tenant", "api-key", "deadbeef");

    let output = sandbox
        .mvmctl()
        .args(["secret", "ls", "--tenant", "test-tenant"])
        .output()
        .expect("spawn mvmctl ls");
    assert!(
        output.status.success(),
        "mvmctl secret ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(sandbox.secret_audit_log_path()).unwrap_or_default();
    assert!(
        log.contains("\"action\":\"list\""),
        "expected an 'action':'list' entry in secrets audit log. Full log:\n{log}"
    );
}

#[test]
fn secret_rm_emits_delete_action_in_secret_audit_log() {
    // Same op-name vs CLI-verb decoupling as `ls` above: clap
    // surface is `rm`, audit action is `"delete"`.
    let sandbox = AuditSandbox::new();
    put_a_secret(&sandbox, "test-tenant", "api-key", "deadbeef");

    let output = sandbox
        .mvmctl()
        .args(["secret", "rm", "api-key", "--tenant", "test-tenant"])
        .output()
        .expect("spawn mvmctl rm");
    assert!(
        output.status.success(),
        "mvmctl secret rm failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(sandbox.secret_audit_log_path()).unwrap_or_default();
    assert!(
        log.contains("\"action\":\"delete\""),
        "expected an 'action':'delete' entry in secrets audit log. Full log:\n{log}"
    );
}
