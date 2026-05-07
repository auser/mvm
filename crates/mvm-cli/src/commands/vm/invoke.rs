//! `mvmctl invoke` — boot a microVM and call its baked entrypoint.
//!
//! ADR-007 / plan 41 W3.
//!
//! Distinct from `mvmctl exec` (dev-only, arbitrary shell). `invoke`
//! is the production-safe call surface — it dispatches the
//! `RunEntrypoint` vsock verb, which the guest agent serves only by
//! spawning the program named in `/etc/mvm/entrypoint`. There is no
//! shell, no argv override, and no env injection beyond what the
//! wrapper template defined at image build time.
//!
//! v1 behaviour:
//!   - boots a transient microVM from a registered template,
//!   - waits for the guest agent,
//!   - reads stdin from a file (`-` = mvmctl's own stdin, default empty),
//!   - sends `GuestRequest::RunEntrypoint`,
//!   - streams `EntrypointEvent::Stdout` / `Stderr` events back to
//!     mvmctl's own stdout / stderr as they arrive,
//!   - tears the VM down,
//!   - exits with the wrapper's exit code (or non-zero on error).
//!
//! `--fresh` and `--reset` are accepted but informational in v1 — the
//! current behaviour matches `--fresh` (no warm session reuse). When
//! the session-pool plan lands, the default flips to "reuse warm VM"
//! and `--fresh` becomes the opt-out for callers who need a clean
//! VM per call.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use mvm_core::user_config::MvmConfig;

use super::Cli;
use crate::ui;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Template (or pre-built manifest) to boot. Resolves the same way
    /// as `mvmctl exec --manifest <ARG>` (registered name, manifest
    /// path, or manifest directory). Required for v1; warm-session
    /// reuse and arbitrary VM-name targeting come with the
    /// session-pool plan.
    #[arg(value_name = "MANIFEST")]
    pub manifest: String,

    /// Path to stdin payload, or `-` for mvmctl's own stdin. Default:
    /// no stdin (the wrapper sees an empty pipe).
    #[arg(long, value_name = "PATH")]
    pub stdin: Option<String>,

    /// Wall-clock timeout for the call, in seconds. Default 30.
    #[arg(long, default_value = "30")]
    pub timeout: u64,

    /// vCPU count for the booted VM. Default 2.
    #[arg(long, default_value = "2")]
    pub cpus: u32,

    /// Memory for the booted VM (MiB). Default 512.
    #[arg(long, default_value = "512")]
    pub memory_mib: u32,

    /// Boot a fresh transient VM, run the call, tear down (the v1
    /// default — wired explicitly so future versions can flip the
    /// default to warm-session reuse without breaking scripts).
    #[arg(long, conflicts_with = "reset")]
    pub fresh: bool,

    /// Restore the session VM from its post-boot snapshot before the
    /// next call. Wired but no-op in v1; lands with the session-pool
    /// plan.
    #[arg(long)]
    pub reset: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    if args.reset {
        ui::warn(
            "--reset is wired but no-op in this build (session-pool plan); \
             treating as default behaviour",
        );
    }

    // v1: invoke targets a *template*. Resolve through the same shared
    // helper as `mvmctl exec --manifest`. Slot-hash and registered-name
    // both resolve to a string the lifecycle helpers consume.
    let template_id = match super::shared::resolve_manifest_arg(&args.manifest)? {
        super::shared::ManifestArgRef::Name(n) => n,
        super::shared::ManifestArgRef::Slot { slot_hash } => slot_hash,
    };

    let stdin_bytes = read_stdin_payload(args.stdin.as_deref())?;

    ui::info(&format!(
        "invoke: booting transient VM for template '{template_id}'"
    ));
    let vm = crate::exec::boot_session_vm(&template_id, "invoke", args.cpus, args.memory_mib)
        .context("Booting VM for invoke")?;

    // Phase 3: register a session record so `mvmctl session ls`
    // sees the in-flight call. v1 sessions are 1:1 with an
    // `mvmctl invoke` lifetime; warm-pool reuse (Phase 5) makes them
    // outlive the call. Errors registering are logged but don't
    // block the call.
    let session_id = register_invoke_session(&vm.vm_name, &template_id);

    if !crate::exec::wait_for_agent(&vm.vm_name, 30) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::exec::tear_down_session_vm(crate::exec::SessionVm {
                vm_name: vm.vm_name.clone(),
            })
        }));
        deregister_invoke_session(session_id.as_ref());
        anyhow::bail!("guest agent did not become reachable within 30s");
    }

    // Run the call. We always tear down the VM after, even if dispatch
    // fails — match `mvmctl exec` lifecycle so transient resources
    // (TAPs, sockets, snapshot dirs) don't leak.
    let dispatch_result = dispatch(&vm.vm_name, stdin_bytes, args.timeout);

    // Tear down. Errors here are warned but not propagated — the
    // call's exit code is what the caller cares about.
    crate::exec::tear_down_session_vm(crate::exec::SessionVm {
        vm_name: vm.vm_name.clone(),
    });
    deregister_invoke_session(session_id.as_ref());

    match dispatch_result {
        Ok(exit_code) => {
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Register a fresh session record for an `mvmctl invoke` call.
/// Returns the id on success, or `None` if registration failed (e.g.
/// no writable runtime dir). Logs warnings on failure but does not
/// abort the invoke — the call should still succeed if the session
/// machinery is unavailable.
fn register_invoke_session(
    vm_name: &str,
    workload_id: &str,
) -> Option<mvm_core::session::SessionId> {
    let record = mvm_core::session::SessionRecord::new_running(
        vm_name,
        workload_id,
        mvm_core::session::SessionMode::Prod,
    );
    let id = record.id.clone();
    match mvm_core::session::write_session(&record) {
        Ok(()) => Some(id),
        Err(e) => {
            tracing::warn!(err = %e, "failed to register invoke session");
            None
        }
    }
}

/// Remove the session record for an in-flight `mvmctl invoke`. If the
/// session was already killed externally (state = Killed / Reaped),
/// keep the record so an observer can see the lifecycle terminated.
fn deregister_invoke_session(id: Option<&mvm_core::session::SessionId>) {
    let Some(id) = id else { return };
    // Read current state — if external code marked it Killed, leave
    // the record in place; otherwise remove it.
    match mvm_core::session::read_session(id) {
        Ok(Some(rec)) if rec.state == mvm_core::session::SessionState::Running => {
            if let Err(e) = mvm_core::session::remove_session(id) {
                tracing::warn!(err = %e, "failed to remove invoke session record");
            }
        }
        Ok(_) => {
            // Either not present or in a non-Running state — leave as-is.
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to read invoke session record");
        }
    }
}

/// Read the stdin payload for the call.
///
/// - `None`: empty payload.
/// - `Some("-")`: read everything from mvmctl's own stdin.
/// - `Some(path)`: read the file at `path`.
fn read_stdin_payload(spec: Option<&str>) -> Result<Vec<u8>> {
    match spec {
        None => Ok(Vec::new()),
        Some("-") => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("Reading stdin from mvmctl's own stdin")?;
            Ok(buf)
        }
        Some(path) => std::fs::read(path).with_context(|| format!("Reading stdin from {path}")),
    }
}

/// Send the `RunEntrypoint` request and stream output back. Returns
/// the wrapper's exit code, or a non-zero placeholder on agent-side
/// errors. The placeholders reuse standard Unix conventions:
/// `124` for timeout (matching `timeout(1)`), `137` for SIGKILL
/// (8+9), `1` for everything else.
fn dispatch(vm_name: &str, stdin: Vec<u8>, timeout_secs: u64) -> Result<i32> {
    let transport = mvm_runtime::vsock_transport::for_vm(vm_name)
        .with_context(|| format!("Picking transport for guest agent on '{vm_name}'"))?;
    let mut stream = transport
        .connect(mvm_guest::vsock::GUEST_AGENT_PORT)
        .with_context(|| format!("Connecting to guest agent on '{vm_name}'"))?;

    let terminal = mvm_guest::vsock::send_run_entrypoint(
        &mut stream,
        stdin,
        timeout_secs,
        |event| match event {
            mvm_guest::vsock::EntrypointEvent::Stdout { chunk } => {
                let _ = std::io::stdout().write_all(chunk);
            }
            mvm_guest::vsock::EntrypointEvent::Stderr { chunk } => {
                let _ = std::io::stderr().write_all(chunk);
            }
            mvm_guest::vsock::EntrypointEvent::Control {
                header_json,
                payload,
            } => {
                // Phase 4a skeleton: surface fd-3 control records to
                // the operator with a clearly-labelled prefix the
                // user's stderr can't spoof (these come from mvmctl,
                // not the wrapper). A future slice (4d) adds an
                // SDK-facing `--envelope-fd <n>` flag that writes
                // raw frames out for structured consumption; until
                // then this human-readable form is the default.
                if payload.is_empty() {
                    let _ = writeln!(std::io::stderr(), "[mvmctl-control] {header_json}");
                } else {
                    let _ = writeln!(
                        std::io::stderr(),
                        "[mvmctl-control] {header_json} (+{} payload bytes)",
                        payload.len()
                    );
                }
            }
            // Terminal events (Exit / Error) are returned by
            // send_run_entrypoint; the handler is only invoked for
            // streaming chunks above.
            _ => {}
        },
    )
    .context("Streaming RunEntrypoint response")?;

    // Flush before potentially exiting.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    Ok(exit_code_for(&terminal))
}

fn exit_code_for(event: &mvm_guest::vsock::EntrypointEvent) -> i32 {
    use mvm_guest::vsock::{EntrypointEvent, RunEntrypointError};
    match event {
        EntrypointEvent::Exit { code } => *code,
        EntrypointEvent::Error { kind, message } => {
            let (code, label) = match kind {
                RunEntrypointError::Timeout => (124, "timeout"),
                RunEntrypointError::Busy => (1, "busy"),
                RunEntrypointError::PayloadCap => (1, "payload cap exceeded"),
                RunEntrypointError::WrapperCrashed => (137, "wrapper crashed"),
                RunEntrypointError::EntrypointInvalid => (1, "entrypoint invalid"),
                RunEntrypointError::InternalError => (1, "internal error"),
            };
            ui::warn(&format!("invoke: {label}: {message}"));
            code
        }
        // Non-terminal events shouldn't reach this function — the
        // streaming consumer only returns terminal events. Defensive:
        // treat as internal error.
        _ => {
            ui::warn("invoke: dispatcher returned non-terminal event");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_normal_exit_zero() {
        let evt = mvm_guest::vsock::EntrypointEvent::Exit { code: 0 };
        assert_eq!(exit_code_for(&evt), 0);
    }

    #[test]
    fn test_exit_code_normal_exit_preserves_nonzero() {
        let evt = mvm_guest::vsock::EntrypointEvent::Exit { code: 7 };
        assert_eq!(exit_code_for(&evt), 7);
    }

    #[test]
    fn test_exit_code_timeout_maps_to_124() {
        let evt = mvm_guest::vsock::EntrypointEvent::Error {
            kind: mvm_guest::vsock::RunEntrypointError::Timeout,
            message: "killed".into(),
        };
        assert_eq!(exit_code_for(&evt), 124);
    }

    #[test]
    fn test_exit_code_wrapper_crash_maps_to_137() {
        let evt = mvm_guest::vsock::EntrypointEvent::Error {
            kind: mvm_guest::vsock::RunEntrypointError::WrapperCrashed,
            message: "segfault".into(),
        };
        assert_eq!(exit_code_for(&evt), 137);
    }

    #[test]
    fn test_exit_code_busy_payload_invalid_internal_all_map_to_1() {
        use mvm_guest::vsock::RunEntrypointError as E;
        for kind in [
            E::Busy,
            E::PayloadCap,
            E::EntrypointInvalid,
            E::InternalError,
        ] {
            let evt = mvm_guest::vsock::EntrypointEvent::Error {
                kind,
                message: "x".into(),
            };
            assert_eq!(exit_code_for(&evt), 1, "expected 1 for {kind:?}");
        }
    }

    #[test]
    fn test_read_stdin_none_is_empty() {
        let bytes = read_stdin_payload(None).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn test_read_stdin_file_returns_contents() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello-stdin").unwrap();
        let bytes = read_stdin_payload(Some(tmp.path().to_str().unwrap())).unwrap();
        assert_eq!(bytes, b"hello-stdin");
    }

    #[test]
    fn test_read_stdin_missing_file_errors() {
        let err = read_stdin_payload(Some("/this/does/not/exist")).unwrap_err();
        assert!(err.to_string().contains("Reading stdin from"));
    }
}
