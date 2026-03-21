use super::harness::mvmctl;

/// `ls` on a clean system should list VMs or show "No running".
#[test]
fn status_on_clean_system_produces_meaningful_output() {
    let assert = mvmctl().arg("ls").assert();
    let output = assert.get_output();
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(code, 2, "ls should not produce a parse error");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("NAME")
            || combined.contains("No running")
            || combined.contains("limactl"),
        "ls should produce meaningful output, got: {combined}"
    );
}

/// `status --help` (alias for ls) should always succeed.
#[test]
fn status_help_always_succeeds() {
    mvmctl().args(["status", "--help"]).assert().success();
}

/// `ps` alias should behave identically to `ls`.
#[test]
fn ps_alias_behaves_like_status() {
    let ps_code = mvmctl()
        .arg("ps")
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    let ls_code = mvmctl()
        .arg("ls")
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_eq!(
        ps_code, ls_code,
        "ps alias should exit with same code as ls"
    );
}
