//! Plan 60 Phase 4 — every CLI subcommand must declare its audit
//! posture.
//!
//! Plan 60 §"Phase 4 — Persistent observability" exit test
//! `every_command_emits_audit_entry` is the eventual goal: drive
//! every `mvmctl` subcommand end-to-end and assert ≥1 audit entry
//! per. That end-to-end coverage needs hermetic test fixtures for
//! every command (many need a running VM, lima, or network), so it
//! grows incrementally as commands gain testable setups.
//!
//! What this scaffold ships *now* is the **enforcement that every
//! command has a declared audit posture**. The test walks
//! `mvm_cli::cli_command()` to enumerate every subcommand and checks
//! each against a static [`AUDIT_POSTURE`] table. Adding a new CLI
//! subcommand without a corresponding entry in the table is a
//! compile-time-equivalent failure: the test fails until the new
//! command is classified.
//!
//! Each subcommand is classified as one of:
//!
//! - [`AuditPosture::Emits`] — the command MUST emit ≥1 audit entry on
//!   success. The entry kind is named (`LocalAuditKind::*` or a
//!   `plan.*` chain event). Future work: drive the command and assert
//!   the named entry appears.
//! - [`AuditPosture::ReadOnly`] — the command only reads host state.
//!   No audit entry expected. Examples: `ls`, `logs`, `diff`,
//!   `metrics`, `audit tail`, `doctor`. Tightening to "ReadOnly
//!   commands emit a `cmd.read` audit entry" is a separate slice.
//! - [`AuditPosture::DelegatesToSub`] — the verb is a subcommand
//!   group (e.g., `manifest`, `network`, `volume`); the leaves of
//!   each subgroup carry their own audit posture. This scaffold
//!   covers the top-level Commands enum only; per-subgroup coverage
//!   ships in follow-on slices.
//! - [`AuditPosture::InteractiveOrControl`] — interactive PTY surface
//!   (`console`, `exec`, `dev`), shell/installer surfaces (`bootstrap`,
//!   `init`, `shell-init`), or pure control-plane commands (`mcp`)
//!   that have their own audit channels (e.g., MCP emits
//!   `McpToolsCallRun` per tool call). The top-level command itself
//!   doesn't emit a single entry; the inner protocol does.
//!
//! Adding a new posture variant or refining the table is a
//! deliberate change — the test is the source of truth.

use std::collections::BTreeMap;

/// Audit classification for one CLI subcommand. See module docs for
/// the meaning of each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditPosture {
    Emits(&'static str),
    ReadOnly,
    DelegatesToSub,
    InteractiveOrControl,
}

/// Every top-level `mvmctl` subcommand keyed by its clap name.
///
/// Order matches the `Commands` enum in
/// `crates/mvm-cli/src/commands/mod.rs`. Adding a new command? Add
/// an entry here — the test below fails until you do.
const AUDIT_POSTURE: &[(&str, AuditPosture)] = &[
    // Environment / installer surfaces.
    ("bootstrap", AuditPosture::InteractiveOrControl),
    ("dev", AuditPosture::InteractiveOrControl),
    ("cleanup", AuditPosture::Emits("SlotPrune")),
    ("update", AuditPosture::Emits("UpdateInstall")),
    ("doctor", AuditPosture::ReadOnly),
    ("shell-init", AuditPosture::InteractiveOrControl),
    ("uninstall", AuditPosture::Emits("Uninstall")),
    ("init", AuditPosture::InteractiveOrControl),
    // VM lifecycle.
    ("up", AuditPosture::Emits("plan.admitted+plan.launched+VmStart")),
    ("down", AuditPosture::Emits("VmStop")),
    ("logs", AuditPosture::ReadOnly),
    ("forward", AuditPosture::ReadOnly),
    ("ls", AuditPosture::ReadOnly),
    ("diff", AuditPosture::ReadOnly),
    ("console", AuditPosture::InteractiveOrControl),
    ("exec", AuditPosture::InteractiveOrControl),
    ("invoke", AuditPosture::Emits("plan.admitted+plan.launched")),
    ("session", AuditPosture::DelegatesToSub),
    ("set-ttl", AuditPosture::Emits("VmTtlSet")),
    ("fs", AuditPosture::Emits("VmFsMutate")),
    ("proc", AuditPosture::DelegatesToSub),
    ("pause", AuditPosture::Emits("VmStop")),
    ("resume", AuditPosture::Emits("VmStart")),
    ("snapshot", AuditPosture::DelegatesToSub),
    ("volume", AuditPosture::DelegatesToSub),
    // Build / artifact / registry.
    ("manifest", AuditPosture::DelegatesToSub),
    ("storage", AuditPosture::DelegatesToSub),
    ("build", AuditPosture::Emits("TemplateBuild")),
    ("validate", AuditPosture::ReadOnly),
    ("catalog", AuditPosture::ReadOnly),
    // Operational surfaces.
    ("metrics", AuditPosture::ReadOnly),
    ("config", AuditPosture::Emits("ConfigChange")),
    ("audit", AuditPosture::ReadOnly),
    ("network", AuditPosture::DelegatesToSub),
    ("cache", AuditPosture::DelegatesToSub),
    ("mcp", AuditPosture::InteractiveOrControl),
    ("secret", AuditPosture::DelegatesToSub),
    ("attest", AuditPosture::DelegatesToSub),
    // Policy bundle inspector — `show` / `verify` are read-only;
    // `update` is stubbed pending mvmd-signed plan flow (Phase 8).
    ("policy", AuditPosture::DelegatesToSub),
];

#[test]
fn every_top_level_subcommand_has_audit_posture_declared() {
    let cmd = mvm_cli::commands::cli_command();
    let declared: BTreeMap<&'static str, AuditPosture> = AUDIT_POSTURE.iter().copied().collect();

    // Enumerate the clap subcommand names.
    let clap_names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();

    // Each clap name must be present in the table.
    let mut missing_in_table: Vec<&str> = Vec::new();
    for name in &clap_names {
        if !declared.contains_key(name.as_str()) {
            missing_in_table.push(name.as_str());
        }
    }
    assert!(
        missing_in_table.is_empty(),
        "{} CLI subcommand(s) lack an audit-posture declaration in \
         tests/audit_total_coverage.rs::AUDIT_POSTURE: {:?}. \
         Add an entry (Emits | ReadOnly | DelegatesToSub | \
         InteractiveOrControl) before merging the new command.",
        missing_in_table.len(),
        missing_in_table
    );

    // Each table entry must correspond to a real clap subcommand —
    // catches a rename that left a stale row behind.
    let clap_set: std::collections::BTreeSet<&str> =
        clap_names.iter().map(String::as_str).collect();
    let mut stale_in_table: Vec<&str> = Vec::new();
    for name in declared.keys() {
        if !clap_set.contains(name) {
            stale_in_table.push(name);
        }
    }
    assert!(
        stale_in_table.is_empty(),
        "{} stale entry/entries in AUDIT_POSTURE for subcommand(s) the \
         clap tree no longer exposes: {:?}. Remove the stale row(s) \
         or rename the entry to match the new clap subcommand name.",
        stale_in_table.len(),
        stale_in_table
    );
}

#[test]
fn audit_posture_table_has_no_duplicate_subcommand_names() {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for (name, _) in AUDIT_POSTURE.iter() {
        assert!(
            seen.insert(*name),
            "duplicate AUDIT_POSTURE entry for subcommand {name:?}"
        );
    }
}

#[test]
fn audit_posture_emits_entries_reference_known_audit_kinds() {
    // Best-effort lint: every `Emits(name)` string should mention at
    // least one token that maps to a real audit category — either a
    // `LocalAuditKind` variant name (CamelCase) or a `plan.*` chain
    // event. This catches typos like `Emits("VmStrt")` without
    // needing reflection into the LocalAuditKind enum.
    //
    // The check uses a static allowlist; expanding the allowlist is
    // a deliberate change tied to the actual audit-emission code.
    const KNOWN_TOKENS: &[&str] = &[
        // LocalAuditKind variants the current Emits rows reference.
        "VmStart",
        "VmStop",
        "VmFsMutate",
        "VmTtlSet",
        "TemplateBuild",
        "ConfigChange",
        "Uninstall",
        "UpdateInstall",
        "SlotPrune",
        // Plan-64 audit-chain events.
        "plan.admitted",
        "plan.launched",
    ];
    for (name, posture) in AUDIT_POSTURE {
        if let AuditPosture::Emits(spec) = posture {
            let hit = KNOWN_TOKENS.iter().any(|tok| spec.contains(tok));
            assert!(
                hit,
                "AUDIT_POSTURE[{name:?}] = Emits({spec:?}) names no \
                 known audit category — typo, or the allowlist in \
                 audit_total_coverage.rs::audit_posture_emits_entries_\
                 reference_known_audit_kinds needs the new token added \
                 alongside the new LocalAuditKind variant."
            );
        }
    }
}
