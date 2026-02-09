use assert_cmd::Command;
use predicates::prelude::*;

fn mvm() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("mvm").unwrap()
}

#[test]
fn test_help_exits_successfully() {
    mvm().arg("--help").assert().success();
}

#[test]
fn test_version_exits_successfully() {
    mvm()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("mvm"));
}

#[test]
fn test_no_args_shows_usage() {
    mvm()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn test_unknown_subcommand_fails() {
    mvm()
        .arg("nonexistent")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn test_help_lists_all_subcommands() {
    let assert = mvm().arg("--help").assert().success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    for cmd in [
        "bootstrap",
        "setup",
        "dev",
        "start",
        "stop",
        "ssh",
        "status",
        "destroy",
    ] {
        assert!(
            output.contains(cmd),
            "Help output should list '{}' subcommand",
            cmd
        );
    }
}

#[test]
fn test_bootstrap_help() {
    mvm()
        .args(["bootstrap", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Homebrew"));
}

#[test]
fn test_setup_help() {
    mvm()
        .args(["setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Firecracker"));
}

#[test]
fn test_dev_help() {
    mvm()
        .args(["dev", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-bootstrapping"));
}

#[test]
fn test_status_runs_without_lima() {
    // status should work even without Lima — it reports "Not created"
    let assert = mvm().arg("status").assert();
    // It either succeeds (showing status) or fails because limactl is missing,
    // but it should never panic
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("status")
            || combined.contains("limactl")
            || combined.contains("Not created"),
        "status should produce meaningful output, got: {}",
        combined
    );
}
