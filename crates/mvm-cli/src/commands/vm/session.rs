//! `mvmctl session ls / info / kill / set-timeout` — session lifecycle
//! verbs (Phase 3 / mvmforge `specs/upstream-mvm-prompt.md` deliverable D).
//!
//! Session metadata is persisted at
//! `$XDG_RUNTIME_DIR/mvm/sessions/<id>.json` (see
//! `mvm_core::session` for the on-disk type and store helpers). These
//! verbs operate on whatever is currently in the table.
//!
//! ## Wiring status
//!
//! v1 ships the table + the verbs. Sessions are populated by
//! `crate::exec::boot_session_vm` (which now registers an entry per
//! booted VM) and removed by `tear_down_session_vm` (which marks
//! `state = Killed` for human-initiated `mvmctl session kill` calls or
//! removes the file on graceful exit). Because `mvmctl invoke` today
//! still boots-and-tears-down per call, sessions are short-lived; the
//! warm-process pool path (Phase 5) is what keeps a session
//! materialised across multiple invokes.
//!
//! ## What's deferred
//!
//! - `set-timeout` writes the new value into the on-disk record; the
//!   guest-agent-side enforcement (`UpdateIdleTimeout` vsock verb) is
//!   a Phase 5 follow-up. The CLI verb is wired now so SDKs can call
//!   it ahead of substrate-side enforcement.
//! - `kind="session-killed"` envelope on inflight `RunEntrypoint`
//!   calls is a guest-agent change (Phase 4c/5).

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};

use mvm_core::session::{self, SessionId, SessionState};
use mvm_core::user_config::MvmConfig;

use super::Cli;
use crate::ui;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum Cmd {
    /// List all active sessions.
    Ls(LsArgs),
    /// Print session metadata as JSON.
    Info(InfoArgs),
    /// Terminate a session immediately.
    Kill(KillArgs),
    /// Update the substrate-side idle timeout for a session.
    SetTimeout(SetTimeoutArgs),
    /// Re-attach to an existing session and dispatch a `RunEntrypoint`
    /// call into its VM. Phase 5 (`Session.attach()` from mvmforge SDK).
    Attach(AttachArgs),
    /// Run an arbitrary shell command against a dev-mode session.
    /// Refused on prod-mode sessions.
    Exec(ExecArgs),
    /// Run user code (interpreted by the wrapper's runtime) against a
    /// dev-mode session. Refused on prod-mode sessions.
    RunCode(RunCodeArgs),
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct LsArgs {
    /// Emit JSON array on stdout. Default: tab-separated table.
    #[arg(long)]
    pub json: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct InfoArgs {
    /// Session id to inspect.
    pub session_id: String,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct KillArgs {
    /// Session id to terminate.
    pub session_id: String,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct SetTimeoutArgs {
    /// Session id to update.
    pub session_id: String,
    /// New idle-reaper timeout in seconds. Must be > 0.
    pub seconds: u64,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct AttachArgs {
    /// Session id to dispatch into.
    pub session_id: String,
    /// Path to stdin payload, or `-` for mvmctl's own stdin. Default:
    /// no stdin (the wrapper sees an empty pipe).
    #[arg(long, value_name = "PATH")]
    pub stdin: Option<String>,
    /// Wall-clock timeout for the call, in seconds. Default 30.
    #[arg(long, default_value = "30")]
    pub timeout: u64,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct ExecArgs {
    /// Session id to dispatch into.
    pub session_id: String,
    /// Command and args to run inside the dev session.
    /// Use `--` before the command if it has flags that look like
    /// `mvmctl` flags (e.g. `mvmctl session exec <id> -- ls -la`).
    #[arg(required = true, last = true)]
    pub argv: Vec<String>,
    /// Wall-clock timeout for the call, in seconds. Default 30.
    #[arg(long, default_value = "30")]
    pub timeout: u64,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct RunCodeArgs {
    /// Session id to dispatch into.
    pub session_id: String,
    /// Code body to run. The wrapper interprets this in its native
    /// runtime (Python, Node, etc. — language is determined by the
    /// session's wrapper, not by the CLI).
    pub code: String,
    /// Wall-clock timeout for the call, in seconds. Default 30.
    #[arg(long, default_value = "30")]
    pub timeout: u64,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.command {
        Cmd::Ls(a) => cmd_ls(a),
        Cmd::Info(a) => cmd_info(a),
        Cmd::Kill(a) => cmd_kill(a),
        Cmd::SetTimeout(a) => cmd_set_timeout(a),
        Cmd::Attach(a) => cmd_attach(a),
        Cmd::Exec(a) => cmd_exec(a),
        Cmd::RunCode(a) => cmd_run_code(a),
    }
}

fn cmd_ls(args: LsArgs) -> Result<()> {
    let sessions = session::list_sessions().context("listing sessions")?;
    if args.json {
        println!("{}", serde_json::to_string(&sessions)?);
        return Ok(());
    }
    if sessions.is_empty() {
        ui::info("No active sessions.");
        return Ok(());
    }
    println!("ID\tWORKLOAD\tVM\tMODE\tSTATE\tINVOKES\tIDLE_TIMEOUT\tSTARTED_AT");
    for s in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.id,
            s.workload_id,
            s.vm_name,
            s.mode,
            s.state,
            s.invoke_count,
            s.idle_timeout_secs,
            s.started_at,
        );
    }
    Ok(())
}

fn cmd_info(args: InfoArgs) -> Result<()> {
    let id = SessionId::parse(&args.session_id)
        .with_context(|| format!("Invalid session id: {:?}", args.session_id))?;
    let record = session::read_session(&id)
        .context("reading session")?
        .ok_or_else(|| anyhow::anyhow!("no session with id {id}"))?;
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

fn cmd_kill(args: KillArgs) -> Result<()> {
    let id = SessionId::parse(&args.session_id)
        .with_context(|| format!("Invalid session id: {:?}", args.session_id))?;
    let record = session::read_session(&id)
        .context("reading session")?
        .ok_or_else(|| anyhow::anyhow!("no session with id {id}"))?;
    if record.state != SessionState::Running {
        bail!(
            "session {id} is not running (state: {}); cannot kill",
            record.state
        );
    }

    // Tear down the substrate VM. Backend-level errors are warned but
    // don't prevent us from updating the session record — the user
    // expects the session to be marked dead either way.
    crate::exec::tear_down_session_vm(crate::exec::SessionVm {
        vm_name: record.vm_name.clone(),
    });

    // Update the on-disk record to reflect the kill. The substrate may
    // still emit a few in-flight events as the VM tears down; an SDK
    // observing `state = Killed` knows to attribute those to a kill.
    session::update_session(&id, |r| {
        r.state = SessionState::Killed;
        Ok(())
    })
    .context("updating session record after kill")?;

    ui::info(&format!("Killed session {id} (vm {})", record.vm_name));
    Ok(())
}

fn cmd_set_timeout(args: SetTimeoutArgs) -> Result<()> {
    if args.seconds == 0 {
        bail!("--seconds must be > 0");
    }
    let id = SessionId::parse(&args.session_id)
        .with_context(|| format!("Invalid session id: {:?}", args.session_id))?;
    let updated = session::update_session(&id, |r| {
        r.idle_timeout_secs = args.seconds;
        Ok(())
    })
    .context("updating session timeout")?;
    ui::info(&format!(
        "Updated session {id} idle_timeout_secs={}",
        updated.idle_timeout_secs
    ));
    Ok(())
}

/// Look up a Running session by id, returning `(id, record)`. Common
/// prelude for `attach` / `exec` / `run-code`. Errors are mapped to
/// stable phrasing so SDKs can match on text.
fn require_running_session(raw_id: &str) -> Result<(SessionId, mvm_core::session::SessionRecord)> {
    let id = SessionId::parse(raw_id).with_context(|| format!("Invalid session id: {raw_id:?}"))?;
    let record = session::read_session(&id)
        .context("reading session")?
        .ok_or_else(|| anyhow::anyhow!("no session with id {id}"))?;
    if record.state != SessionState::Running {
        bail!(
            "session {id} is not running (state: {}); cannot dispatch",
            record.state
        );
    }
    Ok((id, record))
}

fn cmd_attach(args: AttachArgs) -> Result<()> {
    let (id, record) = require_running_session(&args.session_id)?;

    let stdin_bytes = super::invoke::read_stdin_payload(args.stdin.as_deref())?;
    ui::info(&format!(
        "attach: dispatching into session {id} (vm {})",
        record.vm_name
    ));
    let exit_code = super::invoke::dispatch(&record.vm_name, stdin_bytes, args.timeout)
        .with_context(|| format!("dispatching into session {id}"))?;

    // Bump the session's invoke counter / last-used timestamp so
    // observers (`mvmctl session info`) see the activity.
    if let Err(e) = session::update_session(&id, |r| {
        r.invoke_count = r.invoke_count.saturating_add(1);
        r.last_invoke_at = Some(rfc3339_now());
        Ok(())
    }) {
        tracing::warn!(err = %e, "failed to bump session invoke counter");
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn cmd_exec(args: ExecArgs) -> Result<()> {
    let (id, record) = require_running_session(&args.session_id)?;
    require_dev_mode(&id, &record, "exec")?;

    if args.argv.is_empty() {
        bail!("exec requires at least one argv element after `--`");
    }
    // Rebuild the shell command from argv. Shell-quote each element so
    // an embedded space or quote in user-provided args doesn't get
    // re-tokenized by bash.
    let cmd_line = args
        .argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    run_in_session(&id, &record, cmd_line, args.timeout)
}

fn cmd_run_code(args: RunCodeArgs) -> Result<()> {
    let (id, record) = require_running_session(&args.session_id)?;
    require_dev_mode(&id, &record, "run-code")?;

    // v1: dispatch as a shell command. v2 (deferred) will route
    // `run-code` through a dedicated wrapper-runtime verb so the code
    // executes in the wrapper's interpreter (Python, Node, etc.) with
    // access to its imported modules. For now, the user can
    // `bash -c 'python3 -c "..."'` themselves through `exec`.
    run_in_session(&id, &record, args.code, args.timeout)
}

fn require_dev_mode(
    id: &SessionId,
    record: &mvm_core::session::SessionRecord,
    verb: &str,
) -> Result<()> {
    use mvm_core::session::SessionMode;
    if record.mode == SessionMode::Prod {
        bail!(
            "session {id} is mode=prod; '{verb}' is dev-only. \
             Start the session with mode=dev to allow ad-hoc execution."
        );
    }
    Ok(())
}

/// Dispatch a shell command into an already-running session VM via
/// the existing `Exec` vsock verb. Streams stdout/stderr to mvmctl's
/// own streams; exits non-zero with the wrapper's exit code on failure.
///
/// Note: `Exec` is dev-only on the guest side (gated by the `dev-shell`
/// agent feature, ADR-002 §W4.3). This verb is itself gated by
/// `require_dev_mode` above, but if the session's substrate VM was
/// somehow built with a prod agent the underlying call will fail
/// with `Error { message: "exec not available" }` — surface that to
/// the user as-is.
fn run_in_session(
    id: &SessionId,
    record: &mvm_core::session::SessionRecord,
    command: String,
    timeout_secs: u64,
) -> Result<()> {
    use std::io::Write;

    let vm = crate::exec::SessionVm {
        vm_name: record.vm_name.clone(),
    };
    let output = crate::exec::dispatch_in_session(&vm, command, timeout_secs)
        .with_context(|| format!("dispatching command into session {id}"))?;

    let _ = std::io::stdout().write_all(output.stdout.as_bytes());
    let _ = std::io::stderr().write_all(output.stderr.as_bytes());

    if let Err(e) = session::update_session(id, |r| {
        r.invoke_count = r.invoke_count.saturating_add(1);
        r.last_invoke_at = Some(rfc3339_now());
        Ok(())
    }) {
        tracing::warn!(err = %e, "failed to bump session invoke counter");
    }

    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }
    Ok(())
}

/// Single-quote `s` so bash sees it as one literal token. Doubles up
/// embedded `'` as `'\''` (close-quote, escaped-quote, re-open).
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn rfc3339_now() -> String {
    use chrono::SecondsFormat;
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same env-var serialization as the on-disk store tests in mvm-core.
    /// Two tests in this module mutate `MVM_RUNTIME_DIR`; the lock keeps
    /// them from racing each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct RuntimeDirGuard {
        _temp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl Drop for RuntimeDirGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(prev) => std::env::set_var("MVM_RUNTIME_DIR", prev),
                    None => std::env::remove_var("MVM_RUNTIME_DIR"),
                }
            }
        }
    }

    fn isolated_runtime_dir() -> RuntimeDirGuard {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("MVM_RUNTIME_DIR").ok();
        unsafe {
            std::env::set_var("MVM_RUNTIME_DIR", temp.path());
        }
        RuntimeDirGuard {
            _temp: temp,
            _lock: lock,
            prev,
        }
    }

    #[test]
    fn info_errors_for_unknown_id() {
        let _guard = isolated_runtime_dir();
        let id = SessionId::new().to_string();
        let err = cmd_info(InfoArgs { session_id: id }).unwrap_err();
        assert!(
            err.to_string().contains("no session with id"),
            "expected missing-id error, got: {err}"
        );
    }

    #[test]
    fn set_timeout_zero_is_rejected() {
        let _guard = isolated_runtime_dir();
        let id = SessionId::new().to_string();
        let err = cmd_set_timeout(SetTimeoutArgs {
            session_id: id,
            seconds: 0,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("must be > 0"),
            "expected zero-seconds error, got: {err}"
        );
    }

    #[test]
    fn set_timeout_invalid_id_is_rejected() {
        let _guard = isolated_runtime_dir();
        let err = cmd_set_timeout(SetTimeoutArgs {
            session_id: "ABCDE".into(),
            seconds: 60,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("Invalid session id"),
            "expected invalid-id error, got: {err}"
        );
    }

    #[test]
    fn set_timeout_updates_existing_record() {
        let _guard = isolated_runtime_dir();
        let rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Prod);
        let id_str = rec.id.to_string();
        session::write_session(&rec).unwrap();
        cmd_set_timeout(SetTimeoutArgs {
            session_id: id_str.clone(),
            seconds: 999,
        })
        .unwrap();
        let id = SessionId::parse(&id_str).unwrap();
        let reread = session::read_session(&id).unwrap().unwrap();
        assert_eq!(reread.idle_timeout_secs, 999);
    }

    #[test]
    fn require_running_session_rejects_unknown() {
        let _guard = isolated_runtime_dir();
        let id = SessionId::new().to_string();
        let err = require_running_session(&id).unwrap_err();
        assert!(
            err.to_string().contains("no session with id"),
            "expected missing-id error, got: {err}"
        );
    }

    #[test]
    fn require_running_session_rejects_killed() {
        let _guard = isolated_runtime_dir();
        let mut rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Prod);
        rec.state = session::SessionState::Killed;
        let id = rec.id.to_string();
        session::write_session(&rec).unwrap();
        let err = require_running_session(&id).unwrap_err();
        assert!(
            err.to_string().contains("not running"),
            "expected not-running error, got: {err}"
        );
    }

    #[test]
    fn require_dev_mode_rejects_prod_session() {
        let rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Prod);
        let err = require_dev_mode(&rec.id, &rec, "exec").unwrap_err();
        assert!(
            err.to_string().contains("dev-only"),
            "expected dev-only error, got: {err}"
        );
    }

    #[test]
    fn require_dev_mode_accepts_dev_session() {
        let rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Dev);
        require_dev_mode(&rec.id, &rec, "exec").expect("dev session should pass");
    }

    #[test]
    fn exec_with_empty_argv_is_rejected() {
        let _guard = isolated_runtime_dir();
        let rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Dev);
        let id = rec.id.to_string();
        session::write_session(&rec).unwrap();
        let err = cmd_exec(ExecArgs {
            session_id: id,
            argv: vec![],
            timeout: 30,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("at least one argv element"),
            "expected empty-argv error, got: {err}"
        );
    }

    #[test]
    fn shell_quote_basic_token_wrapped_in_single_quotes() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_handles_embedded_single_quote() {
        // Bash escape sequence: close-quote, escaped-quote, re-open.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_preserves_spaces_and_special_chars() {
        assert_eq!(shell_quote("a b $c|d"), "'a b $c|d'");
    }

    #[test]
    fn attach_with_unknown_id_errors_before_dispatch() {
        // No session record on disk → require_running_session bails
        // before any attempt to talk to a vsock.
        let _guard = isolated_runtime_dir();
        let id = SessionId::new().to_string();
        let err = cmd_attach(AttachArgs {
            session_id: id,
            stdin: None,
            timeout: 1,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no session with id"),
            "expected missing-id error, got: {err}"
        );
    }

    #[test]
    fn run_code_on_prod_session_is_rejected() {
        let _guard = isolated_runtime_dir();
        let rec = session::SessionRecord::new_running("vm-1", "wl", session::SessionMode::Prod);
        let id = rec.id.to_string();
        session::write_session(&rec).unwrap();
        let err = cmd_run_code(RunCodeArgs {
            session_id: id,
            code: "print(1)".into(),
            timeout: 1,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("dev-only"),
            "expected dev-only error, got: {err}"
        );
    }
}
