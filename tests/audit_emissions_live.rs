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
//! - `mvmctl manifest rm <path> --force` → `SlotRemove`
//!   (idempotent against a missing slot — `--force` is the cleanup
//!   contract; the stub `mvm.toml` is enough to canonicalise the
//!   path key)
//! - `mvmctl config set <key> <value>` → `ConfigChange`
//! - `mvmctl config show` → **no** audit entry
//! - `mvmctl cleanup --keep 5` → `SlotPrune`
//!   (`source=cleanup removed=N`; the VM-dependent Step 1 / Step 3
//!   degrade to warnings when the dev VM isn't reachable, but
//!   Step 2 — the build-cache prune — runs on host fs and the
//!   audit emit always fires)
//! - `mvmctl up --hypervisor mock --detach --no-supervisor` (with
//!   `MVM_DIRECT_BOOT=1` + stub kernel/rootfs files) → `VmStart`
//!   (end-to-end exercise of the launchd-spawned direct-boot path
//!   against the in-memory `MockBackend`. The mock makes the
//!   backend dispatch hermetic; `MVM_DIRECT_BOOT` skips the
//!   build + template lookup. Together they let `mvmctl up` run
//!   to completion on a CI runner with no KVM / Nix / Apple
//!   Container / Docker / microsandbox)
//! - `mvmctl set-ttl <vm> <duration>` (after `up --hypervisor mock`)
//!   → `VmTtlSet` (chains off the up-via-mock fixture; the verb
//!   operates on the persistent name registry that `up` populates)
//! - `mvmctl down` (no args, empty sandbox) → `VmStop`
//!   (`stop_all` is tolerant of an empty VM registry and emits
//!   with `detail=stop_all_ok`)
//! - `mvmctl down <name>` (empty sandbox) → `VmStop`
//!   (Firecracker's `stop_vm` is tolerant of missing VMs;
//!   audit emits `detail=ok`)
//! - `mvmctl snapshot rm <name>` → `SnapshotDelete`
//!   (test pre-creates `~/.mvm/instances/<name>/snapshot/` so the
//!   bail-when-missing branch doesn't short-circuit the emit)
//! - `mvmctl snapshot ls` → **no** audit entry
//! - `mvmctl audit tail` / `audit verify` → **no** audit entry
//! - `mvmctl attest status` → **no** audit entry
//! - `mvmctl ls` / `metrics` / `catalog list` → **no** audit entry
//!   (top-level ReadOnly verbs — three more rows from
//!   `AUDIT_POSTURE` pinned against a future regression that
//!   adds an emit to a read-only path)
//! - `mvmctl uninstall --yes --dry-run` → **no** audit entry
//!   (the positive `Uninstall` path is real-system-destructive
//!   and not safely-hermetic, but the dry-run path is read-only
//!   by contract and can be pinned)
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
fn manifest_rm_emits_slot_remove_audit_entry() {
    // `manifest rm <path> --force` removes the slot keyed on the
    // canonicalised manifest path. The `--force` flag makes
    // `template_delete_slot` idempotent against missing slots, so
    // the test works against a fresh sandbox: write a stub
    // `mvm.toml`, then drive `manifest rm` — the audit entry lands
    // even though the slot directory was never created.
    let sandbox = AuditSandbox::new();
    let manifest_path = sandbox.home_path().join("mvm.toml");
    std::fs::write(&manifest_path, "[meta]\nname = \"live-test-rm\"\n")
        .expect("write stub mvm.toml");

    let output = sandbox
        .mvmctl()
        .args([
            "manifest",
            "rm",
            manifest_path.to_str().expect("utf-8 tempdir path"),
            "--force",
        ])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl manifest rm --force failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "slot_remove");
    assert!(
        hits >= 1,
        "expected ≥1 slot_remove entry in audit log, got {hits}. \
         Full log:\n{log}"
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

#[test]
fn config_set_emits_config_change_audit_entry() {
    // `mvmctl config set <key> <value>` writes to
    // `~/.mvm/config.toml` and emits `ConfigChange` — config file
    // mutations are the only after-the-fact record of operator
    // intent on settings that change runtime behavior (default
    // backend, network policy, etc.).
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["config", "set", "default_cpus", "4"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl config set failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "config_change");
    assert!(
        hits >= 1,
        "expected ≥1 config_change entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
    // The key + value should also land in the detail field so an
    // operator scanning the audit log can see what changed.
    assert!(
        log.contains("key=default_cpus value=4"),
        "config_change detail must carry the key+value pair. \
         Full log:\n{log}"
    );
}

#[test]
fn config_show_does_not_emit_audit_entry() {
    // Negative: `config show` is read-only. Pins the
    // AUDIT_POSTURE classification (Emits at the top level, but
    // only `set` actually mutates).
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["config", "show"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl config show failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "config_change");
    assert_eq!(
        hits, 0,
        "read-only `config show` must not emit; got {hits} \
         config_change entry/entries. Full log:\n{log}"
    );
}

#[test]
fn cleanup_emits_slot_prune_audit_entry_even_with_no_builds() {
    // `mvmctl cleanup --keep 5` is the highest-friction Emits row
    // promoted to a live test: it runs three steps, two of which
    // (`run_in_vm` for /tmp cleanup + nix-collect-garbage) need a
    // running dev VM. Pre-refactor, the verb panicked out before
    // reaching the audit emit when the VM was unreachable. The
    // host-fallback in `cleanup_old_dev_builds` (now plain
    // `std::fs::read_dir` / `remove_dir_all`) lets Step 2 succeed
    // against `~/.mvm/dev/builds/` directly; the VM-dependent
    // steps degrade to warnings, and the emit lands at the end
    // regardless. The test asserts the empty-cache case (zero
    // build dirs to prune) — `count=0` is the Plan 37 §6
    // every-attempt-emits invariant in action.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["cleanup", "--keep", "5"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl cleanup --keep 5 failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "slot_prune");
    assert!(
        hits >= 1,
        "expected ≥1 slot_prune entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
    assert!(
        log.contains("source=cleanup"),
        "slot_prune detail must carry source=cleanup to disambiguate \
         from manifest-prune emits. Full log:\n{log}"
    );
}

/// Bring up an `--hypervisor mock` VM in the sandbox via the
/// `MVM_DIRECT_BOOT` direct-boot path. Returns when the VM is
/// registered in the name registry (which `up` does before
/// dispatching to the backend). Used as a fixture by tests of
/// state-changing verbs that operate on a registered VM
/// (`set-ttl`, future `pause`/`resume`/etc. work).
///
/// Pass-through env: `MVM_DIRECT_BOOT=1` + stub kernel/rootfs
/// files skip the build + template-lookup pre-flight that needs
/// real Nix; `--hypervisor mock` routes backend dispatch to
/// [`mvm_backend::MockBackend`]; `--detach` short-circuits the
/// Ctrl+C loop; `--no-supervisor` skips plan-64 admission.
fn bring_up_mock_vm(sandbox: &AuditSandbox, name: &str) {
    let stub_dir = sandbox.home_path().join("stub");
    std::fs::create_dir_all(&stub_dir).expect("mkdir stub");
    let kernel = stub_dir.join("vmlinux");
    let rootfs = stub_dir.join("rootfs.ext4");
    if !kernel.exists() {
        std::fs::write(&kernel, b"fake-kernel").expect("write stub kernel");
    }
    if !rootfs.exists() {
        std::fs::write(&rootfs, b"fake-rootfs").expect("write stub rootfs");
    }
    let output = sandbox
        .mvmctl()
        .env("MVM_DIRECT_BOOT", "1")
        .env("MVM_KERNEL_PATH", &kernel)
        .env("MVM_ROOTFS_PATH", &rootfs)
        .args([
            "up",
            "--hypervisor",
            "mock",
            "--name",
            name,
            "--no-supervisor",
            "--detach",
        ])
        .output()
        .expect("spawn mvmctl up");
    assert!(
        output.status.success(),
        "fixture: bring_up_mock_vm({name}) failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn up_with_mock_backend_emits_vm_start_audit_entry() {
    // End-to-end test of `mvmctl up` against the in-memory
    // `MockBackend`. Pre-MockBackend this row needed a real
    // Firecracker / Apple Container / Docker / microsandbox to
    // exercise — none of which are hermetic on a CI runner. The
    // MockBackend substrate + the `MVM_DIRECT_BOOT` direct-boot
    // path (see `bring_up_mock_vm`) together close that gap.
    let sandbox = AuditSandbox::new();
    bring_up_mock_vm(&sandbox, "test-up-vm");

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "vm_start");
    assert!(
        hits >= 1,
        "expected ≥1 vm_start entry, got {hits}. Full log:\n{log}"
    );
    assert!(
        log.contains("\"vm_name\":\"test-up-vm\""),
        "vm_start must carry vm_name=test-up-vm. Full log:\n{log}"
    );
}

#[test]
fn set_ttl_emits_vm_ttl_set_audit_entry() {
    // `mvmctl set-ttl <vm> <duration>` operates on the persistent
    // name registry that `mvmctl up` populates. Bring up a mock
    // VM first (registers it), then update its TTL — the verb
    // emits `vm_ttl_set` with `expires_at=<RFC3339>` in detail.
    let sandbox = AuditSandbox::new();
    bring_up_mock_vm(&sandbox, "test-ttl-vm");

    let output = sandbox
        .mvmctl()
        .args(["set-ttl", "test-ttl-vm", "1h"])
        .output()
        .expect("spawn mvmctl set-ttl");
    assert!(
        output.status.success(),
        "mvmctl set-ttl failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "vm_ttl_set");
    assert!(
        hits >= 1,
        "expected ≥1 vm_ttl_set entry, got {hits}. Full log:\n{log}"
    );
    assert!(
        log.contains("\"vm_name\":\"test-ttl-vm\""),
        "vm_ttl_set must carry vm_name=test-ttl-vm. Full log:\n{log}"
    );
    assert!(
        log.contains("expires_at="),
        "vm_ttl_set detail must record expires_at. Full log:\n{log}"
    );
}

#[test]
fn set_ttl_clear_emits_vm_ttl_set_with_cleared_detail() {
    // Negative-shape complement: `set-ttl --clear` removes the
    // TTL and emits the same `vm_ttl_set` kind but with
    // `detail=expires_at=cleared`. Pins both the verb's "set"
    // and "clear" paths in one suite.
    let sandbox = AuditSandbox::new();
    bring_up_mock_vm(&sandbox, "test-ttl-clear-vm");

    let output = sandbox
        .mvmctl()
        .args(["set-ttl", "test-ttl-clear-vm", "--clear"])
        .output()
        .expect("spawn mvmctl set-ttl --clear");
    assert!(
        output.status.success(),
        "mvmctl set-ttl --clear failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.contains("expires_at=cleared"),
        "set-ttl --clear must record expires_at=cleared. Full log:\n{log}"
    );
}

#[test]
fn down_no_args_emits_vm_stop_audit_entry() {
    // `mvmctl down` (no args, empty registry) calls `backend.stop_all`,
    // which Firecracker satisfies as a no-op when no VMs are running.
    // The verb emits `vm_stop` with `detail=stop_all_ok` regardless
    // — Plan 37 §6: every state-changing CLI verb emits one record
    // per attempt, even no-ops.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["down"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl down failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "vm_stop");
    assert!(
        hits >= 1,
        "expected ≥1 vm_stop entry, got {hits}. Full log:\n{log}"
    );
    assert!(
        log.contains("stop_all_ok"),
        "vm_stop detail must record stop_all outcome. Full log:\n{log}"
    );
}

#[test]
fn down_with_name_emits_vm_stop_for_that_name() {
    // `mvmctl down <vm>` against a fresh sandbox: Firecracker's
    // `stop_vm` is tolerant of missing VMs (returns Ok), the verb
    // emits `vm_stop` with `vm_name=<vm>` and `detail=ok`.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["down", "ghost-vm"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl down ghost-vm failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "vm_stop");
    assert!(
        hits >= 1,
        "expected ≥1 vm_stop entry, got {hits}. Full log:\n{log}"
    );
    assert!(
        log.contains("\"vm_name\":\"ghost-vm\""),
        "vm_stop must carry vm_name=ghost-vm. Full log:\n{log}"
    );
}

#[test]
fn snapshot_rm_emits_snapshot_delete_audit_entry() {
    // `mvmctl snapshot rm <vm>` removes the snapshot directory and
    // emits `SnapshotDelete`. `delete_instance_snapshot` returns
    // `Ok(false)` when the directory is missing — the CLI then
    // bails *before* the emit point. To exercise the emit branch
    // hermetically, pre-create the snapshot dir with stub bytes.
    // No real Firecracker / VM is involved.
    let sandbox = AuditSandbox::new();
    let snap_dir = sandbox
        .home_path()
        .join(".mvm")
        .join("instances")
        .join("test-snap")
        .join("snapshot");
    std::fs::create_dir_all(&snap_dir).expect("mkdir snapshot dir");
    std::fs::write(snap_dir.join("vmstate.bin"), b"stub").expect("write vmstate stub");

    let output = sandbox
        .mvmctl()
        .args(["snapshot", "rm", "test-snap"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl snapshot rm failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "snapshot_delete");
    assert!(
        hits >= 1,
        "expected ≥1 snapshot_delete entry in audit log, got {hits}. \
         Full log:\n{log}"
    );
    // The vm_name field should carry the snapshot identity so
    // operator searches by VM name find the matching emit.
    assert!(
        log.contains("\"vm_name\":\"test-snap\""),
        "snapshot_delete must carry vm_name=test-snap. Full log:\n{log}"
    );
}

#[test]
fn snapshot_ls_does_not_emit_audit_entry() {
    // Negative: `snapshot ls` is read-only. `SNAPSHOT_SUB` in
    // `audit_total_coverage.rs` classifies it as `ReadOnly`; this
    // test pins that against a future regression that adds an
    // emit to the list path.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["snapshot", "ls"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl snapshot ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "snapshot_delete");
    assert_eq!(
        hits, 0,
        "read-only `snapshot ls` must not emit snapshot_delete; \
         got {hits}. Full log:\n{log}"
    );
}

#[test]
fn audit_tail_does_not_emit_local_audit_entry() {
    // Negative: `audit tail` reads the LocalAudit stream. The audit
    // CLI itself is ReadOnly (classification in
    // `audit_total_coverage.rs`); reading the log must not add to it.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["audit", "tail"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl audit tail failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The LocalAudit stream (`<state>/log/audit.jsonl`) must be
    // empty — tail reads but does not write. The plan-64 chain at
    // `~/.mvm/audit/<tenant>.jsonl` always gains cmd.* entries
    // from the audit emitter middleware, which is by design and
    // separate from the LocalAudit stream this lint guards.
    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `audit tail` must not write to the LocalAudit \
         stream. Full log:\n{log}"
    );
}

#[test]
fn audit_verify_does_not_emit_local_audit_entry() {
    // Negative: `audit verify` validates the plan-64 chain.
    // Read-only against the LocalAudit stream. Note the verify
    // command itself appends cmd.* chain entries via the emitter
    // middleware — that's a separate stream and not in scope here.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["audit", "verify"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl audit verify failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `audit verify` must not write to the LocalAudit \
         stream. Full log:\n{log}"
    );
}

#[test]
fn top_level_ls_does_not_emit_audit_entry() {
    // `mvmctl ls` reports running VMs. Pure read. Top-level
    // `("ls", AuditPosture::ReadOnly)` row in `AUDIT_POSTURE`.
    // Pinning the empty-sandbox case — output is "No running VMs."
    // and the audit log stays empty.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["ls"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `mvmctl ls` must not write to the LocalAudit \
         stream. Full log:\n{log}"
    );
}

#[test]
fn metrics_does_not_emit_audit_entry() {
    // `mvmctl metrics` prints Prometheus exposition. Pure read.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["metrics"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl metrics failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `mvmctl metrics` must not write to the LocalAudit \
         stream. Full log:\n{log}"
    );
}

#[test]
fn catalog_list_does_not_emit_audit_entry() {
    // `mvmctl catalog list` enumerates bundled images. The catalog
    // is compiled in; no disk reads beyond mvmctl's binary itself.
    // Pure read.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["catalog", "list"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl catalog list failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `mvmctl catalog list` must not write to the \
         LocalAudit stream. Full log:\n{log}"
    );
}

#[test]
fn uninstall_dry_run_does_not_emit_audit_entry() {
    // `mvmctl uninstall --yes` emits `Uninstall` at the end, but
    // its three filesystem mutations (`/var/lib/mvm`, `~/.mvm/`,
    // `/usr/local/bin/mvmctl`) are real system paths that can't
    // safely be exercised in a hermetic test — a dev with an
    // actual install on the local machine would have sudo block
    // the test mid-run. The dry-run path returns before any of
    // those steps and (per the implementation) before the audit
    // emit; this test pins that contract.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["uninstall", "--yes", "--dry-run"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl uninstall --yes --dry-run failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    let hits = count_entries_with_kind(&log, "uninstall");
    assert_eq!(
        hits, 0,
        "dry-run must not write uninstall audit entries, got {hits}. \
         Full log:\n{log}"
    );
}

#[test]
fn attest_status_does_not_emit_local_audit_entry() {
    // Negative: `attest status` reports the host's attestation
    // identity — pure read. `ATTEST_SUB` classifies all three
    // leaves (export / verify / status) as ReadOnly.
    let sandbox = AuditSandbox::new();
    let output = sandbox
        .mvmctl()
        .args(["attest", "status"])
        .output()
        .expect("spawn mvmctl");
    assert!(
        output.status.success(),
        "mvmctl attest status failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = read_audit_log(&sandbox.audit_log_path());
    assert!(
        log.is_empty(),
        "read-only `attest status` must not write to the LocalAudit \
         stream. Full log:\n{log}"
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
