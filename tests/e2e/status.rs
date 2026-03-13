use super::harness::mvmctl;

/// `status` on a clean system (no Lima VM) should exit 0 or 1 but never panic
/// or emit an exit code of 2 (Clap parse error).
#[test]
fn status_on_clean_system_produces_meaningful_output() {
    let assert = mvmctl().arg("status").assert();
    let output = assert.get_output();
    let code = output.status.code().unwrap_or(-1);

    // Exit code 2 means Clap parse error — never acceptable.
    assert_ne!(code, 2, "status should not produce a parse error");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Should produce some meaningful output regardless of environment.
    assert!(
        combined.contains("status")
            || combined.contains("Lima")
            || combined.contains("Not created")
            || combined.contains("limactl")
            || combined.contains("mvm"),
        "status should produce meaningful output, got: {combined}"
    );
}

/// `status --help` should always succeed.
#[test]
fn status_help_always_succeeds() {
    mvmctl().args(["status", "--help"]).assert().success();
}

/// `ps` alias should behave identically to `status`.
#[test]
fn ps_alias_behaves_like_status() {
    let ps_code = mvmctl()
        .arg("ps")
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    let status_code = mvmctl()
        .arg("status")
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_eq!(
        ps_code, status_code,
        "ps alias should exit with same code as status"
    );
}
