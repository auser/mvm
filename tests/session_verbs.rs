//! Plan 51: integration test for `mvmctl session` verbs.
//!
//! Exercises the full lifecycle (start → set-timeout → info → kill →
//! stop) against a temp `MVM_DATA_DIR` so the test doesn't pollute
//! the developer's `~/.mvm/sessions/`.
//!
//! The verbs are bookkeeping-only in v1 (see Plan 51 / `mvm-core/src/
//! session.rs`); per-session VM materialization integration ships in
//! a follow-up. This test pins the on-disk session-record contract +
//! the JSON output shape mvmforge's `Session.info()` consumes.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn mvm_with_data_dir(data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("mvmctl").unwrap();
    cmd.env("MVM_DATA_DIR", data_dir);
    cmd
}

#[test]
fn session_start_emits_session_id() {
    let tmp = TempDir::new().unwrap();
    let out = mvm_with_data_dir(tmp.path())
        .args(["session", "start", "adder"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(out).unwrap();
    let id = id.trim();
    assert!(id.starts_with("ses-"), "session id format: {id}");
    assert!(id.len() > 4);

    // Record file persisted at <data_dir>/sessions/<id>.json.
    let path = tmp.path().join("sessions").join(format!("{id}.json"));
    assert!(
        path.exists(),
        "session record should persist at {}",
        path.display()
    );
}

#[test]
fn session_info_emits_canonical_json() {
    let tmp = TempDir::new().unwrap();
    let id = mvm_with_data_dir(tmp.path())
        .args(["session", "start", "adder"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id).unwrap().trim().to_string();

    let info_output = mvm_with_data_dir(tmp.path())
        .args(["session", "info", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&info_output).expect("info emits valid JSON");
    assert_eq!(json["session_id"], id);
    assert_eq!(json["workload_id"], "adder");
    assert_eq!(json["status"], "created");
    assert_eq!(json["mode"], "prod");
    assert_eq!(json["idle_timeout_secs"], 300);
    assert_eq!(json["invoke_count"], 0);
    assert!(json.get("created_at").is_some());
}

#[test]
fn session_set_timeout_clamps_and_persists() {
    let tmp = TempDir::new().unwrap();
    let id_bytes = mvm_with_data_dir(tmp.path())
        .args(["session", "start", "wl"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_bytes).unwrap().trim().to_string();

    // Within bounds: 600 is preserved.
    mvm_with_data_dir(tmp.path())
        .args(["session", "set-timeout", "600", &id])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(
        &mvm_with_data_dir(tmp.path())
            .args(["session", "info", &id])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(json["idle_timeout_secs"], 600);

    // Above 86400 clamps to 86400.
    mvm_with_data_dir(tmp.path())
        .args(["session", "set-timeout", "999999", &id])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(
        &mvm_with_data_dir(tmp.path())
            .args(["session", "info", &id])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(json["idle_timeout_secs"], 86400);
}

#[test]
fn session_kill_marks_status_killed() {
    let tmp = TempDir::new().unwrap();
    let id = String::from_utf8(
        mvm_with_data_dir(tmp.path())
            .args(["session", "start", "wl"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    mvm_with_data_dir(tmp.path())
        .args(["session", "kill", &id])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(
        &mvm_with_data_dir(tmp.path())
            .args(["session", "info", &id])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(json["status"], "killed");
}

#[test]
fn session_stop_removes_record_and_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let id = String::from_utf8(
        mvm_with_data_dir(tmp.path())
            .args(["session", "start", "wl"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    mvm_with_data_dir(tmp.path())
        .args(["session", "stop", &id])
        .assert()
        .success();

    // Second stop is idempotent (returns 0 even though no record).
    mvm_with_data_dir(tmp.path())
        .args(["session", "stop", &id])
        .assert()
        .success();

    // info on missing id errors (non-zero) with a clear message.
    mvm_with_data_dir(tmp.path())
        .args(["session", "info", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session not found"));
}

#[test]
fn session_set_timeout_on_missing_session_errors() {
    let tmp = TempDir::new().unwrap();
    mvm_with_data_dir(tmp.path())
        .args(["session", "set-timeout", "60", "ses-nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session not found"));
}

#[test]
fn session_kill_on_missing_session_errors() {
    let tmp = TempDir::new().unwrap();
    mvm_with_data_dir(tmp.path())
        .args(["session", "kill", "ses-nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session not found"));
}

#[test]
fn session_start_with_dev_mode_persists_mode() {
    let tmp = TempDir::new().unwrap();
    let id = String::from_utf8(
        mvm_with_data_dir(tmp.path())
            .args(["session", "start", "wl", "--mode", "dev"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let json: Value = serde_json::from_slice(
        &mvm_with_data_dir(tmp.path())
            .args(["session", "info", &id])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(json["mode"], "dev");
}

// ── Plan 52 phase 3+4: attach / exec / run-code ─────────────────────

fn start_session(tmp: &TempDir, mode: &str) -> String {
    String::from_utf8(
        mvm_with_data_dir(tmp.path())
            .args(["session", "start", "wl", "--mode", mode])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}

#[test]
fn session_attach_increments_invoke_count() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "prod");

    mvm_with_data_dir(tmp.path())
        .args(["session", "attach", &id])
        .assert()
        .success();
    mvm_with_data_dir(tmp.path())
        .args(["session", "attach", &id])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(
        &mvm_with_data_dir(tmp.path())
            .args(["session", "info", &id])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(json["invoke_count"], 2);
    assert_eq!(json["status"], "running");
    assert!(
        json.get("last_invoke_at")
            .and_then(|v| v.as_str())
            .is_some(),
        "attach should set last_invoke_at"
    );
}

#[test]
fn session_attach_refuses_killed_session() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "prod");
    mvm_with_data_dir(tmp.path())
        .args(["session", "kill", &id])
        .assert()
        .success();

    mvm_with_data_dir(tmp.path())
        .args(["session", "attach", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("killed"));
}

#[test]
fn session_exec_refuses_prod_session() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "prod");

    mvm_with_data_dir(tmp.path())
        .args(["session", "exec", &id, "--", "ls", "/"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("dev-only"));
}

#[test]
fn session_exec_succeeds_on_dev_session() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "dev");

    mvm_with_data_dir(tmp.path())
        .args(["session", "exec", &id, "--", "ls", "/"])
        .assert()
        .success();
}

#[test]
fn session_run_code_refuses_prod_session() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "prod");

    mvm_with_data_dir(tmp.path())
        .args(["session", "run-code", &id, "print(1)"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("dev-only"));
}

#[test]
fn session_run_code_succeeds_on_dev_session() {
    let tmp = TempDir::new().unwrap();
    let id = start_session(&tmp, "dev");

    mvm_with_data_dir(tmp.path())
        .args(["session", "run-code", &id, "print(1)"])
        .assert()
        .success()
        .stderr(predicate::str::contains("code_sha256="));
}

#[test]
fn session_attach_on_missing_errors() {
    let tmp = TempDir::new().unwrap();
    mvm_with_data_dir(tmp.path())
        .args(["session", "attach", "ses-nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session not found"));
}
