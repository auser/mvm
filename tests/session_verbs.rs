//! CLI-level tests for `mvmctl session ls / info / kill / set-timeout`.
//!
//! Phase 3 / `specs/upstream-mvm-prompt.md` deliverable D. The verbs
//! all operate on the on-disk session table at
//! `$XDG_RUNTIME_DIR/mvm/sessions/<id>.json`; these tests pre-populate
//! the table via `mvm_core::session::write_session` and then drive the
//! CLI to assert the verbs read/write the right entries. No real VM
//! ever boots — `ls` / `info` / `set-timeout` don't touch the
//! substrate, and `kill` is exercised via the unit-tested `cmd_kill`
//! path elsewhere (it requires a real backend, which a unit test
//! environment doesn't have).

use assert_cmd::Command;
use mvm_core::session::{SessionMode, SessionRecord, SessionState, write_session};
use predicates::prelude::*;

/// `cargo test` runs tests in parallel within a binary. Several tests
/// here mutate `MVM_RUNTIME_DIR` in the parent process to populate the
/// on-disk table via `mvm_core::session::write_session` before
/// spawning the mvmctl child. Without serialization the env races
/// across threads. Hold this mutex for the duration of any test that
/// touches the parent process's env.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn mvm_with_runtime_dir(runtime_dir: &std::path::Path) -> Command {
    #[allow(deprecated)]
    let mut cmd = Command::cargo_bin("mvmctl").unwrap();
    cmd.env("MVM_RUNTIME_DIR", runtime_dir);
    cmd
}

/// Helper: pre-populate the on-disk session table at `runtime_dir`
/// while holding the env-lock so concurrent tests don't observe a
/// transient `MVM_RUNTIME_DIR` value. Returns the id of the written
/// record.
fn populate_record(
    runtime_dir: &std::path::Path,
    record: &SessionRecord,
) -> std::sync::MutexGuard<'static, ()> {
    let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var("MVM_RUNTIME_DIR").ok();
    // SAFETY: lock above serializes env mutation across this test
    // binary; restoring the prior value on drop is the caller's
    // responsibility (we hold the guard for them via the return
    // value, but the tests below explicitly restore).
    unsafe {
        std::env::set_var("MVM_RUNTIME_DIR", runtime_dir);
    }
    write_session(record).expect("write");
    // Restore prior env so other helpers/tests don't observe the
    // override; the caller-held guard keeps the mutex.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("MVM_RUNTIME_DIR", v),
            None => std::env::remove_var("MVM_RUNTIME_DIR"),
        }
    }
    lock
}

#[test]
fn session_ls_empty_reports_no_sessions() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "ls"])
        .assert()
        .success()
        // `ui::info` writes to stdout with `[mvm]` prefix.
        .stdout(predicate::str::contains("No active sessions"));
}

#[test]
fn session_ls_json_emits_array() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "ls", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("[]"));
}

#[test]
fn session_ls_with_records_prints_table() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "openclaw", SessionMode::Prod);
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openclaw"))
        .stdout(predicate::str::contains("vm-1"));
}

#[test]
fn session_info_unknown_id_errors() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args([
            "session",
            "info",
            // 26-char base32 string that will never collide with a
            // real session id.
            "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session with id"));
}

#[test]
fn session_info_returns_record_json() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "openclaw", SessionMode::Dev);
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "info", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"vm_name\": \"vm-1\""))
        .stdout(predicate::str::contains("\"mode\": \"dev\""))
        .stdout(predicate::str::contains("\"state\": \"running\""));
}

#[test]
fn session_set_timeout_zero_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "set-timeout", "aaaaaaaaaaaaaaaaaaaaaaaaaa", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be > 0"));
}

#[test]
fn session_set_timeout_invalid_id_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "set-timeout", "TOO_SHORT", "60"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid session id"));
}

#[test]
fn session_set_timeout_updates_record() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "openclaw", SessionMode::Prod);
    let id = rec.id.to_string();
    // Hold the env-lock through the read-back at the end so a
    // concurrent test can't change `MVM_RUNTIME_DIR` between mvmctl's
    // write and our re-read.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var("MVM_RUNTIME_DIR").ok();
    // SAFETY: ENV_LOCK is held; serializes env mutation across this
    // binary's tests.
    unsafe {
        std::env::set_var("MVM_RUNTIME_DIR", temp.path());
    }
    write_session(&rec).unwrap();

    mvm_with_runtime_dir(temp.path())
        .args(["session", "set-timeout", &id, "999"])
        .assert()
        .success();

    let parsed = mvm_core::session::SessionId::parse(&id).unwrap();
    let reread = mvm_core::session::read_session(&parsed).unwrap().unwrap();
    assert_eq!(reread.idle_timeout_secs, 999);

    unsafe {
        match prev {
            Some(v) => std::env::set_var("MVM_RUNTIME_DIR", v),
            None => std::env::remove_var("MVM_RUNTIME_DIR"),
        }
    }
}

#[test]
fn session_kill_unknown_id_errors_without_backend_call() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "kill", "aaaaaaaaaaaaaaaaaaaaaaaaaa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session with id"));
}

#[test]
fn session_kill_non_running_session_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut rec = SessionRecord::new_running("vm-1", "openclaw", SessionMode::Prod);
    rec.state = SessionState::Killed;
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "kill", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}

// --- Phase 5a/5b: attach / exec / run-code ----------------------------------

#[test]
fn session_attach_unknown_id_errors() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "attach", "aaaaaaaaaaaaaaaaaaaaaaaaaa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session with id"));
}

#[test]
fn session_attach_killed_session_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut rec = SessionRecord::new_running("vm-1", "wl", SessionMode::Prod);
    rec.state = SessionState::Killed;
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "attach", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}

#[test]
fn session_exec_on_prod_session_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "wl", SessionMode::Prod);
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "exec", &id, "--", "ls"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("dev-only"));
}

#[test]
fn session_run_code_on_prod_session_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "wl", SessionMode::Prod);
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "run-code", &id, "print(1)"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("dev-only"));
}

#[test]
fn session_console_unknown_id_errors() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "console", "aaaaaaaaaaaaaaaaaaaaaaaaaa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session with id"));
}

#[test]
fn session_reap_emits_count_message() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "reap"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Reaped 0 idle session(s)"));
}

#[test]
fn session_console_on_prod_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let rec = SessionRecord::new_running("vm-1", "wl", SessionMode::Prod);
    let id = rec.id.to_string();
    let _lock = populate_record(temp.path(), &rec);

    mvm_with_runtime_dir(temp.path())
        .args(["session", "console", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("dev-only"));
}

#[test]
fn session_exec_unknown_id_errors_before_mode_check() {
    let temp = tempfile::tempdir().unwrap();
    mvm_with_runtime_dir(temp.path())
        .args(["session", "exec", "aaaaaaaaaaaaaaaaaaaaaaaaaa", "--", "ls"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session with id"));
}
