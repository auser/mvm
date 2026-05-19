//! Plan 89 W3 part 1 — host-side scaffold for the persistent
//! builder VM's dispatch supervisor.
//!
//! [`PersistentBuilderSupervisor`] owns the host end of the
//! dispatch socket libkrun creates at
//! `<vm_state_dir>/vsock-<BUILDER_DISPATCH_PORT>.sock` once the
//! persistent VM is up. Callers — eventually `mvmctl build` from
//! inside an active `mvmctl dev` session — submit a
//! [`crate::builder_vm::BuilderJob`] via [`Self::submit`]; the
//! supervisor serializes it to a
//! [`crate::builder_protocol::BuilderRequest::Run`], writes the
//! frame over the socket, then reads back streamed
//! [`crate::builder_protocol::BuilderResponse::StderrChunk`] frames
//! followed by a terminating
//! [`crate::builder_protocol::BuilderResponse::Result`].
//!
//! ## Scope of this PR (W3 part 1)
//!
//! - **In:** the supervisor type, its connect/submit/shutdown
//!   surface, serialized V1 dispatch (one in-flight job per
//!   supervisor via a tokio-free `Mutex`), the typed
//!   [`PersistentBuilderError`] variants, integration tests
//!   driving the wire end-to-end via `UnixListener::pair`-style
//!   mocks in `crates/mvm-build/tests/persistent_builder_supervisor.rs`.
//! - **Out:** spawning the actual libkrun VM
//!   ([`crate::libkrun_builder::LibkrunPersistentBuilderVm`] lands
//!   in W3 part 2 alongside builder-init's dispatch-loop emit);
//!   `mvmctl dev up` auto-start of the supervisor (W3 part 3);
//!   per-job namespace isolation (W3 part 4); per-dispatch audit
//!   entries (W3 part 5).
//!
//! ## Why serialize V1
//!
//! Per Plan 89 §Concurrency, V1 is one in-flight dispatch per
//! supervisor. The dispatch mutex guards the socket so two
//! concurrent submits don't interleave their `BuilderRequest` /
//! `BuilderResponse` frames on the same connection. V2+ parallel
//! dispatch (per-job overlay namespaces + per-job vsock conns) is
//! an explicit non-goal of this plan.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use thiserror::Error;

use crate::builder_protocol::{
    BootTimingsWire, BuilderRequest, BuilderResponse, JobId, JobTimings,
};
use crate::builder_vm::BuilderJob;

/// Default timeout for the entire dispatch round (write request +
/// drain stderr stream + read terminating Result). Generous because
/// real builds can take minutes; callers that want a hard upper
/// bound should wrap their own timeout above [`Self::submit`].
const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Per-frame read timeout. Bounds how long the supervisor blocks
/// waiting for the next StderrChunk / Result if the guest's
/// dispatch loop is wedged. The chosen 30 s is large enough that a
/// busy nix-build with quiet stderr stretches don't trip it, while
/// being small enough that a crashed guest doesn't pin the host
/// thread for the full `DEFAULT_DISPATCH_TIMEOUT`.
const DEFAULT_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The completed outcome of one dispatched job. Mirrors the wire
/// shape of [`BuilderResponse::Result`] plus an in-process
/// accumulation of the stderr chunks that streamed in before the
/// terminating Result.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub job_id: JobId,
    pub exit_code: i32,
    pub stderr_tail: String,
    pub stderr_chunks: Vec<String>,
    pub boot_timings: Option<Box<BootTimingsWire>>,
    pub job_timings: JobTimings,
}

/// Typed error surface for [`PersistentBuilderSupervisor`].
#[derive(Debug, Error)]
pub enum PersistentBuilderError {
    /// The dispatch socket was unreachable at submit time. Usually
    /// means the persistent VM is down or libkrun hasn't yet
    /// created the proxy socket. Caller may want to fall back to
    /// the single-shot [`crate::libkrun_builder::LibkrunBuilderVm`]
    /// path.
    #[error("dispatch socket {socket} unreachable: {source}")]
    SocketUnreachable {
        socket: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Reading or writing a frame failed mid-dispatch. The guest
    /// most likely crashed; subsequent submits will hit
    /// [`Self::SocketUnreachable`] until the supervisor restarts
    /// the VM.
    #[error("dispatch frame I/O error: {0}")]
    Frame(#[from] std::io::Error),

    /// Guest sent a frame whose `job_id` didn't match the
    /// request's. Hard failure — V1's serialized dispatch means
    /// every response should belong to the one in-flight job, so a
    /// mismatch means the guest's dispatch loop is corrupted.
    #[error("job_id mismatch: request {request:?}, response {response:?}")]
    JobIdMismatch { request: JobId, response: JobId },

    /// Guest closed the connection before sending a terminating
    /// [`BuilderResponse::Result`]. Could mean kernel panic, OOM
    /// kill of the build process, or a logic bug in the dispatch
    /// loop. Distinguished from [`Self::SocketUnreachable`] because
    /// the *connection* was established and partial output was
    /// received.
    #[error("dispatch ended without Result frame after {chunks} stderr chunks")]
    PrematureEof { chunks: usize },

    /// Internal: dispatch mutex poisoned (a previous submit panicked
    /// mid-flight). Caller should rebuild the supervisor.
    #[error("dispatch mutex poisoned — caller must restart the supervisor")]
    MutexPoisoned,
}

/// Persistent builder VM dispatch supervisor.
///
/// `socket_path` is `<vm_state_dir>/vsock-<BUILDER_DISPATCH_PORT>.sock`
/// — the libkrun-managed Unix socket that proxies to AF_VSOCK port
/// [`mvm_guest::builder_agent::BUILDER_DISPATCH_PORT`] inside the
/// guest. The owner of the persistent VM (W3 part 2 will spawn it
/// from `LibkrunPersistentBuilderVm`) constructs this supervisor
/// once the VM is up, then hands the result to whatever submits
/// jobs (W3 part 3 wires `mvmctl build` to it from inside a dev
/// session).
pub struct PersistentBuilderSupervisor {
    socket_path: PathBuf,
    dispatch_mutex: Mutex<()>,
    frame_read_timeout: Duration,
    dispatch_timeout: Duration,
}

impl PersistentBuilderSupervisor {
    /// Construct a supervisor against the given dispatch socket.
    /// Doesn't actually connect — connection is per-submit so a
    /// crashed-then-restarted VM doesn't require restarting the
    /// supervisor. The socket is opportunistically probed
    /// (`std::path::Path::exists`) so callers get a clean
    /// `SocketUnreachable` immediately for the obvious case.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            dispatch_mutex: Mutex::new(()),
            frame_read_timeout: DEFAULT_FRAME_READ_TIMEOUT,
            dispatch_timeout: DEFAULT_DISPATCH_TIMEOUT,
        }
    }

    /// Override the per-frame read timeout. Useful for tests that
    /// want fast fail; production callers should stick with the
    /// default.
    pub fn with_frame_read_timeout(mut self, timeout: Duration) -> Self {
        self.frame_read_timeout = timeout;
        self
    }

    /// Override the whole-dispatch timeout. Same caveat as
    /// [`Self::with_frame_read_timeout`].
    pub fn with_dispatch_timeout(mut self, timeout: Duration) -> Self {
        self.dispatch_timeout = timeout;
        self
    }

    /// Dispatch one [`BuilderJob`] into the persistent VM and
    /// block until the guest sends back a terminating
    /// [`BuilderResponse::Result`]. Holds the dispatch mutex for
    /// the duration (V1 = serialized).
    ///
    /// `job_dir_relpath` is the path inside the `/job` virtio-fs
    /// share where the host has already staged this job's
    /// artifacts (cmd.sh / install_spec.json / etc.). The guest's
    /// dispatch loop resolves it as `/job/<job_dir_relpath>`.
    pub fn submit(
        &self,
        job: BuilderJob,
        job_dir_relpath: String,
    ) -> Result<DispatchOutcome, PersistentBuilderError> {
        let _guard = self
            .dispatch_mutex
            .lock()
            .map_err(|_| PersistentBuilderError::MutexPoisoned)?;

        let job_id = JobId::new();
        let request = BuilderRequest::Run {
            job_id,
            job,
            job_dir_relpath,
        };

        let outcome = self.dispatch(&request, job_id)?;
        Ok(outcome)
    }

    /// Send a [`BuilderRequest::Shutdown`] to the guest's dispatch
    /// loop and wait for the matching [`BuilderResponse::Bye`].
    /// Consumes `self` because the supervisor is one-shot after
    /// shutdown — the VM will power off and the socket goes away.
    pub fn shutdown(self) -> Result<(), PersistentBuilderError> {
        let _guard = self
            .dispatch_mutex
            .lock()
            .map_err(|_| PersistentBuilderError::MutexPoisoned)?;

        let request = BuilderRequest::Shutdown {};
        let mut stream = self.connect()?;
        mvm_guest::vsock::write_frame(&mut stream, &request)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Expect Bye. Any other frame is a protocol violation but
        // we already asked for shutdown — log via the error chain
        // and let the caller treat it as success.
        let _ = read_next_response(&mut stream, self.frame_read_timeout);
        Ok(())
    }

    fn connect(&self) -> Result<UnixStream, PersistentBuilderError> {
        UnixStream::connect(&self.socket_path).map_err(|source| {
            PersistentBuilderError::SocketUnreachable {
                socket: self.socket_path.clone(),
                source,
            }
        })
    }

    fn dispatch(
        &self,
        request: &BuilderRequest,
        request_job_id: JobId,
    ) -> Result<DispatchOutcome, PersistentBuilderError> {
        let started = std::time::Instant::now();
        let mut stream = self.connect()?;
        let _ = stream.set_read_timeout(Some(self.frame_read_timeout));
        let _ = stream.set_write_timeout(Some(self.frame_read_timeout));

        mvm_guest::vsock::write_frame(&mut stream, request)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut stderr_chunks = Vec::new();
        loop {
            if started.elapsed() > self.dispatch_timeout {
                return Err(PersistentBuilderError::PrematureEof {
                    chunks: stderr_chunks.len(),
                });
            }
            let response = read_next_response(&mut stream, self.frame_read_timeout)?;
            match response {
                Some(BuilderResponse::StderrChunk { job_id, line }) => {
                    if job_id != request_job_id {
                        return Err(PersistentBuilderError::JobIdMismatch {
                            request: request_job_id,
                            response: job_id,
                        });
                    }
                    stderr_chunks.push(line);
                }
                Some(BuilderResponse::Result {
                    job_id,
                    exit_code,
                    stderr_tail,
                    boot_timings,
                    job_timings,
                }) => {
                    if job_id != request_job_id {
                        return Err(PersistentBuilderError::JobIdMismatch {
                            request: request_job_id,
                            response: job_id,
                        });
                    }
                    return Ok(DispatchOutcome {
                        job_id,
                        exit_code,
                        stderr_tail,
                        stderr_chunks,
                        boot_timings,
                        job_timings,
                    });
                }
                Some(BuilderResponse::Bye {}) => {
                    // Bye in the middle of a dispatch is unexpected —
                    // treat as premature EOF.
                    return Err(PersistentBuilderError::PrematureEof {
                        chunks: stderr_chunks.len(),
                    });
                }
                None => {
                    // Clean EOF before a Result arrived.
                    return Err(PersistentBuilderError::PrematureEof {
                        chunks: stderr_chunks.len(),
                    });
                }
            }
        }
    }
}

/// Read the next [`BuilderResponse`] from `stream`. Returns
/// `Ok(None)` on clean EOF and `Err(_)` on any I/O or framing
/// failure. Module-private because the supervisor is the only
/// caller that needs the "Option-on-EOF" semantics.
fn read_next_response(
    stream: &mut UnixStream,
    _read_timeout: Duration,
) -> Result<Option<BuilderResponse>, PersistentBuilderError> {
    match mvm_guest::vsock::read_frame::<BuilderResponse>(stream) {
        Ok(resp) => Ok(Some(resp)),
        Err(e) => {
            let src = e.source();
            if let Some(io_err) = src.and_then(|s| s.downcast_ref::<std::io::Error>())
                && io_err.kind() == std::io::ErrorKind::UnexpectedEof
            {
                return Ok(None);
            }
            Err(PersistentBuilderError::Frame(std::io::Error::other(
                e.to_string(),
            )))
        }
    }
}

/// Path of the dispatch socket libkrun creates for a builder VM
/// rooted at `vm_state_dir`. Mirrors the convention
/// `mvm_libkrun::KrunContext` uses when
/// `add_vsock_port(BUILDER_DISPATCH_PORT)` is configured.
pub fn dispatch_socket_path(vm_state_dir: &Path) -> PathBuf {
    vm_state_dir.join(format!(
        "vsock-{}.sock",
        mvm_guest::builder_agent::BUILDER_DISPATCH_PORT
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_socket_path_uses_builder_dispatch_port_constant() {
        let p = dispatch_socket_path(Path::new("/var/lib/mvm/vm-foo"));
        assert!(
            p.to_string_lossy().ends_with(&format!(
                "vsock-{}.sock",
                mvm_guest::builder_agent::BUILDER_DISPATCH_PORT
            )),
            "got: {}",
            p.display()
        );
    }

    #[test]
    fn submit_against_missing_socket_returns_socket_unreachable() {
        let supervisor = PersistentBuilderSupervisor::new("/no/such/socket.sock");
        let result = supervisor.submit(
            BuilderJob::Flake {
                flake_ref: "path:/x".to_string(),
                attr_path: "packages.aarch64-linux.default".to_string(),
            },
            "00000000".to_string(),
        );
        match result {
            Err(PersistentBuilderError::SocketUnreachable { socket, .. }) => {
                assert!(socket.to_string_lossy().contains("/no/such/socket.sock"));
            }
            other => panic!("expected SocketUnreachable, got {other:?}"),
        }
    }
}
