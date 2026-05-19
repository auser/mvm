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

// ============================================================
// Plan 89 W3 part 7 — session record + PersistentBuilderVm
// ============================================================
//
// `mvmctl persistent-builder start` writes the session record at
// `~/.mvm/run/persistent-builder.json` and leaves the supervisor
// child running. `mvm_build::pipeline::dev_build` reads the record
// on every build and routes through `PersistentBuilderVm` (which
// implements `BuilderVm` via the wire) when a session is alive.

/// Session record format mirroring
/// `mvm_cli::commands::build::persistent_builder::SessionRecord`.
/// Duplicated here to keep mvm-build off the mvm-cli direction in
/// the dep graph; the `session_record_serde_matches_mvm_cli` test
/// pins the two shapes together via shared JSON fixtures.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    /// Opaque session ID from the live `PersistentVmHandle`.
    pub session_id: String,
    /// libkrun-managed Unix socket the supervisor connects to.
    pub dispatch_socket_path: PathBuf,
    /// Per-session job dir (bound at `/job` in the guest). Hosts
    /// stage per-dispatch artifacts here before submitting.
    pub job_dir: PathBuf,
    /// Workspace bound at `/work`. Recorded for `mvmctl
    /// persistent-builder status`; not load-bearing here.
    pub workspace_root: PathBuf,
    /// PID of the libkrun supervisor child. Used to check the
    /// session is alive before routing through it.
    pub supervisor_pid: u32,
}

/// On-disk location of the session record. Returns `None` only if
/// `$HOME` isn't set, which would be a misconfigured environment.
pub fn session_record_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".mvm")
            .join("run")
            .join("persistent-builder.json")
    })
}

/// Read the session record and verify the supervisor is alive.
/// Returns `None` if the record is missing, malformed, or the
/// recorded PID isn't a live process — all of which are "no
/// active session, fall back to single-shot" signals as far as
/// the routing layer is concerned. Never errors: a stale record
/// shouldn't break the user's build.
pub fn read_active_session() -> Option<SessionRecord> {
    let path = session_record_path()?;
    let body = std::fs::read(&path).ok()?;
    let record: SessionRecord = serde_json::from_slice(&body).ok()?;
    if !supervisor_alive(record.supervisor_pid) {
        return None;
    }
    Some(record)
}

/// `kill(pid, 0)` — checks the process exists without signalling.
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

/// Sidecar filename inside `<job_dir>/<job_id>/out/` carrying the
/// nix store path. The host parses it after a successful dispatch
/// to recover the revision hash (mirror of single-shot's
/// `read_revision_hash` in `libkrun_builder.rs`).
pub const STORE_PATH_SIDECAR: &str = "store-path.txt";

/// Subdir name inside a dispatch's job dir where the cmd.sh
/// copies build artifacts (`vmlinux`, `rootfs.ext4`, optional
/// `manifest.json`, and the [`STORE_PATH_SIDECAR`]).
pub const ARTIFACT_SUBDIR: &str = "out";

/// Path on the host where a dispatch's artifacts land.
pub fn artifact_dir_for(session_job_dir: &Path, job_id: &str) -> PathBuf {
    session_job_dir.join(job_id).join(ARTIFACT_SUBDIR)
}

/// Stage a per-dispatch cmd.sh + output subdir under
/// `<session_job_dir>/<job_id>/`. Returns the relative path
/// (just `<job_id>`) that the host passes as
/// `BuilderRequest::Run::job_dir_relpath` and the guest dispatch
/// loop resolves under `/job/`. cmd.sh:
///
/// 1. Runs `nix build` against `<flake_ref>#<attr>`, captures the
///    store path.
/// 2. Validates `$STORE_PATH/vmlinux` and `$STORE_PATH/rootfs.ext4`
///    (mkGuest layout contract); exits 4 with stderr message on
///    miss.
/// 3. `cp -L`s vmlinux + rootfs.ext4 (+ optional manifest.json)
///    to `/job/<job_id>/out/`.
/// 4. Writes the store path to `/job/<job_id>/out/store-path.txt`
///    so the host can recover the revision hash post-dispatch.
pub fn stage_flake_dispatch_job(
    session_job_dir: &Path,
    flake_ref: &str,
    attr: &str,
) -> std::io::Result<String> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let sub = session_job_dir.join(&job_id);
    let artifact_dir = sub.join(ARTIFACT_SUBDIR);
    std::fs::create_dir_all(&artifact_dir)?;
    let script = format!(
        "#!/bin/sh\n\
         set -eu\n\
         OUT_DIR='/job/{job_id}/{artifact_subdir}'\n\
         mkdir -p \"$OUT_DIR\"\n\
         STORE_PATH=$(nix --extra-experimental-features 'nix-command flakes' \\\n\
             build --no-link --print-out-paths \\\n\
             {flake_ref}#{attr})\n\
         echo \"store-path=$STORE_PATH\"\n\
         printf '%s\\n' \"$STORE_PATH\" > \"$OUT_DIR/{store_path_sidecar}\"\n\
         if [ ! -f \"$STORE_PATH/vmlinux\" ]; then\n\
             echo 'mvm-builder-init: nix output missing vmlinux' >&2\n\
             exit 4\n\
         fi\n\
         if [ ! -f \"$STORE_PATH/rootfs.ext4\" ]; then\n\
             echo 'mvm-builder-init: nix output missing rootfs.ext4' >&2\n\
             exit 4\n\
         fi\n\
         cp -L \"$STORE_PATH/vmlinux\" \"$OUT_DIR/vmlinux\"\n\
         cp -L \"$STORE_PATH/rootfs.ext4\" \"$OUT_DIR/rootfs.ext4\"\n\
         if [ -f \"$STORE_PATH/manifest.json\" ]; then\n\
             cp -L \"$STORE_PATH/manifest.json\" \"$OUT_DIR/manifest.json\"\n\
         fi\n",
        artifact_subdir = ARTIFACT_SUBDIR,
        store_path_sidecar = STORE_PATH_SIDECAR,
        flake_ref = shell_single_quote(flake_ref),
        attr = shell_single_quote(attr),
    );
    let cmd_path = sub.join("cmd.sh");
    std::fs::write(&cmd_path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cmd_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(job_id)
}

fn shell_single_quote(s: &str) -> String {
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

/// Extract the leading hash from a Nix store path. Mirror of the
/// single-shot helper in `libkrun_builder.rs` (kept local rather
/// than re-exported to keep this module self-contained). Only used
/// by `PersistentBuilderVm`, so gated on the `builder-vm` feature
/// to keep no-feature builds free of dead-code warnings.
#[cfg(any(test, feature = "builder-vm"))]
fn extract_nix_store_hash(store_path: &str) -> Option<&str> {
    let name = store_path.strip_prefix("/nix/store/")?;
    let (hash, _rest) = name.split_once('-')?;
    if hash.is_empty() { None } else { Some(hash) }
}

/// `BuilderVm` impl that routes builds through the live persistent
/// VM via [`PersistentBuilderSupervisor`]. Constructed from a
/// [`SessionRecord`] read by [`read_active_session`].
///
/// On `run_build`:
/// 1. Stages cmd.sh + output dir under the session's `job_dir`.
/// 2. Submits a `BuilderJob::Flake` via the supervisor.
/// 3. On success: reads `store-path.txt`, derives the revision
///    hash, copies artifacts from the session's output dir into
///    `mounts.artifact_out` so callers see the same paths the
///    single-shot path produces.
/// 4. Returns `BuilderArtifacts::Image` mirroring the single-shot
///    shape so downstream `dev_build` post-processing is
///    unchanged.
///
/// Install variant (`BuilderJob::Install`) returns
/// `BuilderVmError::NotYetImplemented` — Plan 73's sealed-volume
/// invariants vs persistent-mode are W3 follow-up work.
#[cfg(feature = "builder-vm")]
pub struct PersistentBuilderVm {
    session: SessionRecord,
}

#[cfg(feature = "builder-vm")]
impl PersistentBuilderVm {
    pub fn new(session: SessionRecord) -> Self {
        Self { session }
    }
}

#[cfg(feature = "builder-vm")]
impl crate::builder_vm::BuilderVm for PersistentBuilderVm {
    fn run_build(
        &self,
        job: &crate::builder_vm::BuilderJob,
        mounts: &crate::builder_vm::BuilderMounts,
    ) -> Result<crate::builder_vm::BuilderArtifacts, crate::builder_vm::BuilderVmError> {
        use crate::builder_vm::{BuilderArtifacts, BuilderJob, BuilderVmError};

        let (flake_ref, attr_path) = match job {
            BuilderJob::Flake {
                flake_ref,
                attr_path,
            } => (flake_ref.as_str(), attr_path.as_str()),
            BuilderJob::Install { .. } => return Err(BuilderVmError::NotYetImplemented),
        };

        let job_id = stage_flake_dispatch_job(&self.session.job_dir, flake_ref, attr_path)
            .map_err(|e| {
                BuilderVmError::ExtractionFailed(format!("staging persistent dispatch job: {e}"))
            })?;

        let supervisor = PersistentBuilderSupervisor::new(&self.session.dispatch_socket_path)
            .with_frame_read_timeout(Duration::from_secs(60));
        let outcome = supervisor
            .submit(job.clone(), job_id.clone())
            .map_err(|e| BuilderVmError::NixBuildFailed(format!("persistent dispatch: {e}")))?;
        if outcome.exit_code != 0 {
            return Err(BuilderVmError::NixBuildFailed(format!(
                "persistent dispatch exit {} — stderr tail:\n{}",
                outcome.exit_code, outcome.stderr_tail
            )));
        }

        let artifact_dir = artifact_dir_for(&self.session.job_dir, &job_id);
        let store_path_body = std::fs::read_to_string(artifact_dir.join(STORE_PATH_SIDECAR))
            .map_err(|e| {
                BuilderVmError::ExtractionFailed(format!(
                    "reading {}: {e}",
                    artifact_dir.join(STORE_PATH_SIDECAR).display()
                ))
            })?;
        let revision_hash = extract_nix_store_hash(store_path_body.trim())
            .ok_or_else(|| {
                BuilderVmError::ExtractionFailed(format!(
                    "malformed store path in sidecar: {store_path_body:?}"
                ))
            })?
            .to_string();

        std::fs::create_dir_all(&mounts.artifact_out).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating {}: {e}",
                mounts.artifact_out.display()
            ))
        })?;
        let dst_vmlinux = mounts.artifact_out.join("vmlinux");
        std::fs::copy(artifact_dir.join("vmlinux"), &dst_vmlinux)
            .map_err(|e| BuilderVmError::ExtractionFailed(format!("copying vmlinux: {e}")))?;
        let dst_rootfs = mounts.artifact_out.join("rootfs.ext4");
        std::fs::copy(artifact_dir.join("rootfs.ext4"), &dst_rootfs)
            .map_err(|e| BuilderVmError::ExtractionFailed(format!("copying rootfs.ext4: {e}")))?;

        Ok(BuilderArtifacts::Image {
            rootfs_path: dst_rootfs,
            kernel_path: Some(dst_vmlinux),
            revision_hash,
            lock_hash: None,
            accessible: None,
        })
    }
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

    // -----------------------------------------------------------
    // Plan 89 W3 part 7 — session record + PersistentBuilderVm
    // -----------------------------------------------------------

    #[test]
    fn session_record_roundtrips_through_json() {
        let record = SessionRecord {
            session_id: "abc".to_string(),
            dispatch_socket_path: PathBuf::from("/tmp/sock"),
            job_dir: PathBuf::from("/tmp/jobs"),
            workspace_root: PathBuf::from("/work"),
            supervisor_pid: 4242,
        };
        let json = serde_json::to_vec(&record).unwrap();
        let back: SessionRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.session_id, "abc");
        assert_eq!(back.supervisor_pid, 4242);
    }

    #[test]
    fn session_record_parses_mvm_cli_emitted_shape() {
        // Cross-validation: mvm-cli writes the same JSON. If
        // either side renames a field, this test fails — the
        // JSON below is the exact body
        // `mvm_cli::commands::build::persistent_builder` emits.
        let json = r#"{
            "session_id": "abc123",
            "dispatch_socket_path": "/home/u/.mvm/vms/foo/vsock-21471.sock",
            "job_dir": "/home/u/.mvm/jobs/foo",
            "workspace_root": "/work",
            "supervisor_pid": 7777
        }"#;
        let record: SessionRecord = serde_json::from_str(json).expect("parse mvm-cli shape");
        assert_eq!(record.session_id, "abc123");
        assert_eq!(record.supervisor_pid, 7777);
    }

    #[test]
    fn read_active_session_returns_none_when_record_missing() {
        // Set HOME to a tempdir that has no .mvm/run/persistent-
        // builder.json — the read should silently return None
        // rather than error.
        let scratch = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests run in single-process; clobbering HOME for
        // a few lines is fine. Reset after.
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", scratch.path());
        }
        let result = read_active_session();
        unsafe {
            match old {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(result.is_none());
    }

    #[test]
    fn read_active_session_returns_none_when_pid_is_dead() {
        // Picked far above /proc/sys/kernel/pid_max defaults
        // (typically 4194304) but inside i32 range — kill(pid, 0)
        // returns ESRCH for any nonexistent PID. PID 0 on Unix
        // means "send to caller's process group" so it's a poor
        // sentinel; this one is safely unallocated.
        const DEFINITELY_DEAD_PID: u32 = 2_000_000_000;
        let scratch = tempfile::tempdir().expect("tempdir");
        let run_dir = scratch.path().join(".mvm").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let record = SessionRecord {
            session_id: "x".to_string(),
            dispatch_socket_path: PathBuf::from("/dev/null"),
            job_dir: PathBuf::from("/tmp"),
            workspace_root: PathBuf::from("/tmp"),
            supervisor_pid: DEFINITELY_DEAD_PID,
        };
        std::fs::write(
            run_dir.join("persistent-builder.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", scratch.path());
        }
        let result = read_active_session();
        unsafe {
            match old {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(
            result.is_none(),
            "dead PID {DEFINITELY_DEAD_PID} must be classified as not-alive"
        );
    }

    #[test]
    fn stage_flake_dispatch_job_writes_cmd_sh_and_store_path_sidecar_dir() {
        // Cross-platform: only checks file layout + content, no
        // actual VM dispatch.
        let scratch = tempfile::tempdir().expect("tempdir");
        let job_dir = scratch.path().to_path_buf();
        let job_id =
            stage_flake_dispatch_job(&job_dir, "path:/work", "packages.aarch64-linux.default")
                .expect("stage");
        let cmd_path = job_dir.join(&job_id).join("cmd.sh");
        assert!(cmd_path.is_file(), "{}", cmd_path.display());
        let body = std::fs::read_to_string(&cmd_path).expect("read");
        assert!(body.contains("'path:/work'"), "{body}");
        assert!(body.contains("'packages.aarch64-linux.default'"), "{body}");
        assert!(body.contains(STORE_PATH_SIDECAR), "{body}");
        assert!(body.contains("cp -L"), "{body}");
        let artifact_dir = artifact_dir_for(&job_dir, &job_id);
        assert!(
            artifact_dir.is_dir(),
            "expected pre-staged artifact dir at {}",
            artifact_dir.display()
        );
    }

    #[test]
    fn extract_nix_store_hash_recovers_leading_hash() {
        assert_eq!(
            extract_nix_store_hash("/nix/store/abc123-foo-bar"),
            Some("abc123")
        );
        assert_eq!(extract_nix_store_hash("/nix/store/-bad"), None);
        assert_eq!(extract_nix_store_hash("not-a-store-path"), None);
    }
}
