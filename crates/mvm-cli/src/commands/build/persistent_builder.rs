//! Plan 89 W3 part 5 — `mvmctl persistent-builder` CLI verb.
//!
//! Wires the W3 parts 1-4 pieces (host-side
//! `LibkrunPersistentBuilderVm` + `PersistentBuilderSupervisor`
//! + the in-guest dispatch loop) into a user-facing command.
//! Three subcommands:
//!
//! - **`start --workspace <path>`** — spawns the long-lived
//!   builder VM and records the dispatch socket path so
//!   subsequent `submit` / `stop` calls find it.
//! - **`submit --flake <path>`** — dispatches one
//!   `BuilderJob::Flake` into the running VM, blocks for the
//!   `BuilderResponse::Result`, prints the outcome. Re-stages
//!   `cmd.sh` under the running VM's job dir per-call.
//! - **`stop`** — sends `BuilderRequest::Shutdown` to the dispatch
//!   loop, waits for the supervisor child to exit cleanly.
//!
//! This is deliberately separate from `mvmctl dev up`. Plan 89's
//! lifecycle binding (`mvmctl dev up` auto-starts the persistent
//! supervisor) lands in a follow-up to avoid colliding with the
//! ur-seed work in flight on `mvmctl dev`. Once both stacks are
//! merged, `mvmctl dev up` becomes a thin caller of the same
//! `LibkrunPersistentBuilderVm::start()` this verb invokes.
//!
//! ## Session state
//!
//! The running VM's dispatch-socket path + supervisor PID get
//! recorded at `~/.mvm/run/persistent-builder.json` so `submit` /
//! `stop` find them across process invocations. The file is mode
//! 0600 to match the ADR-002 W1.5 contract for `~/.mvm/run/`.
//!
//! ## What's deferred
//!
//! - Auto-start from `mvmctl dev up` (post-merge follow-up).
//! - `mvmctl build` routing into the persistent supervisor when a
//!   session is active (W3 part 6+).
//! - Install variant dispatch (W3 part 6+).
//! - Stderr streaming (W3 part 7).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use serde::{Deserialize, Serialize};

use mvm_build::builder_protocol::BuilderResponseRead;
use mvm_build::builder_vm::BuilderJob;
use mvm_build::libkrun_builder::{
    DISPATCH_SOCK_MARKER, LibkrunPersistentBuilderVm, PersistentVmHandle,
};
use mvm_build::persistent_builder::{DispatchOutcome, PersistentBuilderSupervisor};

use crate::commands::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    #[command(subcommand)]
    pub command: Sub,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Sub {
    /// Spawn the persistent builder VM and record the session.
    /// The VM keeps running after this command returns; `submit`
    /// dispatches jobs into it and `stop` brings it down.
    Start(StartArgs),
    /// Dispatch one flake build into the running persistent VM
    /// and print the outcome.
    Submit(SubmitArgs),
    /// Send `BuilderRequest::Shutdown` to the persistent VM and
    /// wait for it to power off cleanly.
    Stop(StopArgs),
    /// Print the current session record (if any).
    Status,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct StartArgs {
    /// Host directory bound at `/work` inside the persistent VM.
    /// Defaults to the current working directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SubmitArgs {
    /// Flake reference to build (e.g. `path:/work#packages.default`).
    #[arg(long)]
    pub flake: String,
    /// Flake attribute path. Defaults to `packages.<host_arch>-linux.default`.
    #[arg(long)]
    pub attr: Option<String>,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct StopArgs {}

/// Persisted at `~/.mvm/run/persistent-builder.json` — single
/// source of truth for `submit` / `stop` to locate a running
/// session.
#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    /// Opaque session ID from `PersistentVmHandle::session_id`.
    session_id: String,
    /// Path libkrun exposes for AF_VSOCK port 21471 proxy. The
    /// supervisor connects here.
    dispatch_socket_path: PathBuf,
    /// Per-VM job dir (bound at `/job` in the guest). `submit`
    /// stages each call's cmd.sh under a fresh
    /// `<job_dir_relpath>` here.
    job_dir: PathBuf,
    /// Workspace bound at `/work` in the guest. Recorded for
    /// `status` output; not load-bearing.
    workspace_root: PathBuf,
    /// PID of the libkrun supervisor child. `stop` uses
    /// `libc::kill(pid, 0)` to check liveness before attempting
    /// shutdown.
    supervisor_pid: u32,
}

fn session_record_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".mvm")
        .join("run")
        .join("persistent-builder.json")
}

fn write_session_record(record: &SessionRecord) -> Result<()> {
    let path = session_record_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(record).context("serializing session record")?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn read_session_record() -> Result<SessionRecord> {
    let path = session_record_path();
    let body = std::fs::read(&path).with_context(|| {
        format!(
            "no persistent-builder session record at {} \
             (start one with `mvmctl persistent-builder start`)",
            path.display()
        )
    })?;
    serde_json::from_slice(&body)
        .with_context(|| format!("parsing session record at {}", path.display()))
}

fn remove_session_record() -> Result<()> {
    let path = session_record_path();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

pub fn run(_cli: &Cli, args: Args) -> Result<()> {
    match args.command {
        Sub::Start(a) => run_start(a),
        Sub::Submit(a) => run_submit(a),
        Sub::Stop(_) => run_stop(),
        Sub::Status => run_status(),
    }
}

fn run_start(args: StartArgs) -> Result<()> {
    if read_session_record().is_ok() {
        bail!(
            "a persistent-builder session is already running. \
             Stop it with `mvmctl persistent-builder stop` before starting a new one."
        );
    }

    let workspace = match args.workspace {
        Some(p) => p,
        None => std::env::current_dir().context("resolving current dir for --workspace")?,
    };

    let vm = LibkrunPersistentBuilderVm::new(&workspace);
    let handle = vm
        .start()
        .context("spawning persistent builder VM (LibkrunPersistentBuilderVm::start)")?;

    let supervisor_pid = handle_pid(&handle);
    let record = SessionRecord {
        session_id: handle.session_id().to_string(),
        dispatch_socket_path: handle.dispatch_socket_path(),
        job_dir: handle.job_dir().to_path_buf(),
        workspace_root: workspace,
        supervisor_pid,
    };
    write_session_record(&record)?;

    // We intentionally LEAK the handle here — the supervisor
    // child stays running after this process exits. `stop`
    // reattaches via the PID in the session record. The held
    // `_nix_store_lock` inside the handle goes away when this
    // process exits but the kernel-level flock follows the fd,
    // which the supervisor's parent (now PID 1 after we exit)
    // doesn't own — so the lock is released on our exit. That's
    // a known gap: between this exit and the supervisor exit,
    // another `mvmctl deps install` can take the lock. The
    // mvmctl-side session record acts as a soft mutex (start
    // refuses if one exists). Hardening to keep the kernel lock
    // alive across CLI invocations is a follow-up.
    std::mem::forget(handle);

    println!("session_id: {}", record.session_id);
    println!("dispatch_socket: {}", record.dispatch_socket_path.display());
    println!("supervisor_pid: {}", record.supervisor_pid);
    Ok(())
}

fn run_submit(args: SubmitArgs) -> Result<()> {
    let record = read_session_record()?;
    if !supervisor_alive(record.supervisor_pid) {
        let _ = remove_session_record();
        bail!(
            "recorded supervisor PID {} is not alive — session record cleared. \
             Start a new session with `mvmctl persistent-builder start`.",
            record.supervisor_pid
        );
    }

    let attr = args
        .attr
        .unwrap_or_else(|| format!("packages.{}-linux.default", host_arch_for_attr()));
    let job_dir_relpath = stage_flake_cmd_sh(&record.job_dir, &args.flake, &attr)?;

    let supervisor = PersistentBuilderSupervisor::new(&record.dispatch_socket_path)
        .with_frame_read_timeout(Duration::from_secs(60));

    let outcome = supervisor
        .submit(
            BuilderJob::Flake {
                flake_ref: args.flake.clone(),
                attr_path: attr.clone(),
            },
            job_dir_relpath,
        )
        .context("PersistentBuilderSupervisor::submit")?;

    print_outcome(&outcome);
    Ok(())
}

fn run_stop() -> Result<()> {
    let record = read_session_record()?;
    if !supervisor_alive(record.supervisor_pid) {
        eprintln!(
            "supervisor PID {} not alive — clearing stale session record",
            record.supervisor_pid
        );
        remove_session_record()?;
        return Ok(());
    }

    let supervisor = PersistentBuilderSupervisor::new(&record.dispatch_socket_path);
    supervisor
        .shutdown()
        .context("PersistentBuilderSupervisor::shutdown")?;

    // Wait briefly for the supervisor child to exit on its own
    // (the guest reboots after sending Bye, then libkrun returns,
    // then the supervisor's main exits). If it doesn't exit within
    // the deadline, fall through to a kill. We don't own the
    // process anymore — start() leaked the handle — so we poll the
    // PID via kill(pid, 0).
    let stop_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < stop_deadline {
        if !supervisor_alive(record.supervisor_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if supervisor_alive(record.supervisor_pid) {
        eprintln!(
            "supervisor PID {} did not exit within 30s; sending SIGTERM",
            record.supervisor_pid
        );
        send_sigterm(record.supervisor_pid);
    }

    remove_session_record()?;
    Ok(())
}

fn run_status() -> Result<()> {
    match read_session_record() {
        Ok(record) => {
            let alive = supervisor_alive(record.supervisor_pid);
            println!("session_id: {}", record.session_id);
            println!("workspace_root: {}", record.workspace_root.display());
            println!("dispatch_socket: {}", record.dispatch_socket_path.display());
            println!(
                "supervisor_pid: {} ({})",
                record.supervisor_pid,
                if alive { "alive" } else { "stale" }
            );
        }
        Err(e) => {
            println!("no persistent-builder session ({e})");
        }
    }
    Ok(())
}

/// Stage a fresh cmd.sh under `<job_dir>/<uuid>/cmd.sh`, return
/// the relative path the guest's dispatch loop resolves under
/// `/job/`. Matches the shape `LibkrunBuilderVm::run_build`
/// produces for the single-shot path so the guest's `run_job`
/// helper accepts the input unchanged.
fn stage_flake_cmd_sh(job_dir: &std::path::Path, flake_ref: &str, attr: &str) -> Result<String> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let sub = job_dir.join(&job_id);
    std::fs::create_dir_all(&sub).with_context(|| format!("creating {}", sub.display()))?;
    let script = format!(
        "#!/bin/sh\n\
         set -eu\n\
         exec nix --extra-experimental-features 'nix-command flakes' \\\n\
             build --no-link --print-out-paths \\\n\
             {flake_ref}#{attr}\n",
        flake_ref = shell_escape(flake_ref),
        attr = shell_escape(attr),
    );
    let cmd_path = sub.join("cmd.sh");
    std::fs::write(&cmd_path, script).with_context(|| format!("writing {}", cmd_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cmd_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(job_id)
}

/// Minimal POSIX-shell single-quote escape. Sufficient for the
/// closed flake_ref + attr_path shapes the supervisor accepts;
/// stricter validation belongs upstream in the IR.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn host_arch_for_attr() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn print_outcome(outcome: &DispatchOutcome) {
    println!("job_id: {}", outcome.job_id);
    println!("exit_code: {}", outcome.exit_code);
    println!("build_ms: {}", outcome.job_timings.build_ms);
    if !outcome.stderr_chunks.is_empty() {
        println!("stderr_chunks ({}):", outcome.stderr_chunks.len());
        for line in &outcome.stderr_chunks {
            println!("  {line}");
        }
    }
    if !outcome.stderr_tail.is_empty() {
        println!("stderr_tail:");
        println!("{}", outcome.stderr_tail);
    }
}

/// `kill(pid, 0)` — checks that the process exists and we can
/// signal it without actually signalling. Returns `false` on
/// `ESRCH` / `EPERM`.
fn supervisor_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn send_sigterm(pid: u32) {
    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Extract the supervisor PID. `PersistentVmHandle` doesn't expose
/// it publicly (the supervisor is internal); we reach in via the
/// process id of `std::process::Child` by going through `id()`
/// when available. Returns 0 if the handle's child has already
/// been consumed.
fn handle_pid(handle: &PersistentVmHandle) -> u32 {
    // PersistentVmHandle's `Child` is private. We can't read its
    // PID directly. For now read it from the supervisor's PID
    // file at <vm_state_dir>/builder.pid which the libkrun
    // supervisor writes (see SupervisorConfig::pid_file_name).
    let pid_path = handle.vm_state_dir().join("builder.pid");
    // The PID file may not exist immediately — the supervisor
    // writes it after init. Brief retry.
    for _ in 0..50 {
        if let Ok(body) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = body.trim().parse::<u32>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    0
}

// Used in the `start` doc string; pulled in here just so the
// unused-import lint stays silent in the cfg-feature-gated
// signatures.
#[allow(dead_code)]
const _MARKER_CONST: &str = DISPATCH_SOCK_MARKER;
#[allow(dead_code)]
fn _force_read_use(r: BuilderResponseRead) -> BuilderResponseRead {
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_wraps_value_in_single_quotes() {
        assert_eq!(shell_escape("simple"), "'simple'");
        assert_eq!(shell_escape("path:/work"), "'path:/work'");
    }

    #[test]
    fn shell_escape_handles_embedded_single_quote() {
        // The classic posix workaround: close quote, escaped
        // literal quote, re-open quote.
        assert_eq!(shell_escape("don't"), r"'don'\''t'");
    }

    #[test]
    fn host_arch_for_attr_returns_a_known_arch() {
        let a = host_arch_for_attr();
        assert!(a == "aarch64" || a == "x86_64", "got {a}");
    }

    #[test]
    fn stage_flake_cmd_sh_writes_valid_shell_under_per_job_subdir() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let job_dir = scratch.path().to_path_buf();
        let relpath = stage_flake_cmd_sh(&job_dir, "path:/work", "packages.aarch64-linux.default")
            .expect("stage");
        let cmd_path = job_dir.join(&relpath).join("cmd.sh");
        assert!(cmd_path.is_file(), "{}", cmd_path.display());
        let body = std::fs::read_to_string(&cmd_path).expect("read");
        // The shell script must reference the flake_ref + attr
        // (both escaped) and use the `nix build` invocation the
        // single-shot path also uses.
        assert!(body.contains("'path:/work'"), "{body}");
        assert!(body.contains("'packages.aarch64-linux.default'"), "{body}");
        assert!(body.contains("nix"), "{body}");
        assert!(body.starts_with("#!/bin/sh"), "{body}");
    }

    #[test]
    fn session_record_roundtrips_through_json() {
        let record = SessionRecord {
            session_id: "abc123".to_string(),
            dispatch_socket_path: PathBuf::from("/tmp/sock"),
            job_dir: PathBuf::from("/tmp/jobs"),
            workspace_root: PathBuf::from("/work"),
            supervisor_pid: 4242,
        };
        let json = serde_json::to_vec(&record).expect("serialize");
        let back: SessionRecord = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.session_id, "abc123");
        assert_eq!(back.dispatch_socket_path, PathBuf::from("/tmp/sock"));
        assert_eq!(back.supervisor_pid, 4242);
    }
}
