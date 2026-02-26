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
        "shell",
        "sync",
        "status",
        "destroy",
        "upgrade",
        "run",
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
fn test_upgrade_help() {
    mvm()
        .args(["upgrade", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("latest version"));
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

#[test]
fn test_shell_help() {
    mvm()
        .args(["shell", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Lima VM"))
        .stdout(predicate::str::contains("--project"));
}

#[test]
fn test_shell_listed_in_help() {
    mvm()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("shell"));
}

#[test]
fn test_sync_listed_in_help() {
    mvm()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("sync"));
}

#[test]
fn test_sync_help() {
    mvm()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--debug"))
        .stdout(predicate::str::contains("--skip-deps"));
}

#[test]
fn test_build_help_shows_flake_options() {
    mvm()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--flake"))
        .stdout(predicate::str::contains("--profile"))
        .stdout(predicate::str::contains("--watch"));
}

#[test]
fn test_build_listed_in_help() {
    mvm()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("build"));
}

#[test]
fn test_ssh_config_prints_entry() {
    mvm()
        .arg("ssh-config")
        .assert()
        .success()
        .stdout(predicate::str::contains("Host mvm"))
        .stdout(predicate::str::contains("IdentityFile"))
        .stdout(predicate::str::contains("StrictHostKeyChecking no"));
}

#[test]
fn test_run_listed_in_help() {
    mvm()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"));
}

#[test]
fn test_run_help_shows_flags() {
    mvm()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--flake"))
        .stdout(predicate::str::contains("--name"))
        .stdout(predicate::str::contains("--profile"))
        .stdout(predicate::str::contains("--cpus"))
        .stdout(predicate::str::contains("--memory"));
}

#[test]
fn test_stop_help_shows_flags() {
    mvm()
        .args(["stop", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--all"))
        .stdout(predicate::str::contains("name"));
}

// ---------------------------------------------------------------------------
// Template command structure
// ---------------------------------------------------------------------------

#[test]
fn test_template_lifecycle_commands() {
    // Top-level help lists template subcommand
    mvm()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("template"));

    // Template help shows lifecycle subcommands
    mvm()
        .args(["template", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("create"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("info"))
        .stdout(predicate::str::contains("build"))
        .stdout(predicate::str::contains("delete"))
        .stdout(predicate::str::contains("push"))
        .stdout(predicate::str::contains("pull"))
        .stdout(predicate::str::contains("verify"));

    // Template create without required args fails
    mvm()
        .args(["template", "create"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));

    // Template build requires a name argument
    mvm()
        .args(["template", "build"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));

    // Template info requires a name argument
    mvm()
        .args(["template", "info"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));

    // Template delete requires a name argument
    mvm()
        .args(["template", "delete"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

// ---------------------------------------------------------------------------
// VM vsock commands
// ---------------------------------------------------------------------------

#[test]
fn test_vm_help_lists_subcommands() {
    mvm()
        .args(["vm", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ping"))
        .stdout(predicate::str::contains("status"));
}

#[test]
fn test_vm_ping_requires_name() {
    mvm()
        .args(["vm", "ping"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

#[test]
fn test_vm_status_requires_name() {
    mvm()
        .args(["vm", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

#[test]
fn test_vm_ping_nonexistent_fails_gracefully() {
    // Ping a VM that doesn't exist — should fail with an error message, not panic
    let assert = mvm().args(["vm", "ping", "nonexistent-vm"]).assert();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Should mention the VM name or indicate it's not found
    assert!(
        combined.contains("nonexistent-vm")
            || combined.contains("not found")
            || combined.contains("No running")
            || combined.contains("error")
            || combined.contains("Error")
            || combined.contains("limactl"),
        "vm ping should fail gracefully, got: {}",
        combined
    );
}

#[test]
fn test_vm_status_nonexistent_fails_gracefully() {
    let assert = mvm().args(["vm", "status", "nonexistent-vm"]).assert();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("nonexistent-vm")
            || combined.contains("not found")
            || combined.contains("No running")
            || combined.contains("error")
            || combined.contains("Error")
            || combined.contains("limactl"),
        "vm status should fail gracefully, got: {}",
        combined
    );
}
