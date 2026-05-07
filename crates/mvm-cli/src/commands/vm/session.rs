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

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.command {
        Cmd::Ls(a) => cmd_ls(a),
        Cmd::Info(a) => cmd_info(a),
        Cmd::Kill(a) => cmd_kill(a),
        Cmd::SetTimeout(a) => cmd_set_timeout(a),
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
}
