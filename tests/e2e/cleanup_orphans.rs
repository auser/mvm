use super::harness::{assert_parse_ok, mvmctl};
use predicates::prelude::*;

/// `cleanup-orphans --dry-run` should parse successfully (exit code != 2).
/// It may exit 1 if no Lima VM is running — that is a runtime condition, not a
/// parse error.
#[test]
fn cleanup_orphans_dry_run_parses_ok() {
    let code = mvmctl()
        .args(["cleanup-orphans", "--dry-run"])
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_ne!(
        code, 2,
        "cleanup-orphans --dry-run must not be a parse error"
    );
}

/// `cleanup-orphans` without flags should not fail with a parse error.
#[test]
fn cleanup_orphans_no_flags_parses_ok() {
    assert_parse_ok(&["cleanup-orphans"]);
}

/// Help output should mention the dry-run flag.
#[test]
fn cleanup_orphans_help_mentions_dry_run() {
    mvmctl()
        .args(["cleanup-orphans", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"));
}
