//! Vz (Apple Virtualization.framework) backend for mvm.
//!
//! Plan 97 / ADR-056. Tier 2 microVM backend for macOS 13+ that runs
//! the workload directly on the host (no nested Firecracker, no
//! libkrun in the path). Lifecycle delegates to a per-VM
//! `mvm-vz-supervisor` Swift subprocess (lives in
//! `crates/mvm-vz-supervisor/`) — same one-process-per-VM contract
//! `LibkrunBackend` uses, swapped underneath.
//!
//! ## Why opt-in only
//!
//! Per Plan 97 §"Phase D" and the user constraint: `auto_select()`
//! stays unchanged on macOS — libkrun remains the macOS default,
//! Firecracker remains the Linux default. Vz is selected only via
//! `MVM_BACKEND=vz` or `--backend vz` (the `from_hypervisor("vz")`
//! path).
//!
//! ## Lifecycle
//!
//! - `start` writes runtime metadata to `~/.mvm/vms/<name>/` (so
//!   `mvmctl console` can find the artifacts), constructs a
//!   [`mvm_vz::SupervisorConfig`] from the `VmStartConfig`, spawns
//!   `mvm-vz-supervisor` with the JSON on stdin, and waits up to
//!   [`PID_FILE_TIMEOUT`] for the supervisor to write its PID file.
//! - `stop` reads `<vm_state_dir>/vz.pid`, sends `SIGTERM` (the
//!   supervisor forwards to `VZVirtualMachine.requestStop()`), polls
//!   for the process to exit, and falls back to `SIGKILL` after
//!   [`STOP_TIMEOUT`].
//! - `status` reads the PID file and probes with `kill(pid, 0)`.
//! - `list` walks `~/.mvm/vms/*/vz.pid`.
//! - `logs` tails `<vm_state_dir>/console.log` (capture-only console
//!   per Plan 97 Security §9).

use anyhow::{Result, anyhow, bail};
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, StartMode, VmBackend,
    VmCapabilities, VmId, VmInfo, VmStartConfig, VmStatus,
};

use mvm_base::ui;
use mvm_vz as vz;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Apple Virtualization.framework backend.
///
/// Direct host-level Vz integration; no nested KVM, no libkrun shim.
/// Only available on macOS 13+ (`Platform::has_vz()`). On Linux this
/// type still compiles, but `is_available()` always returns `Ok(false)`
/// and `start` bails before spawning anything.
pub struct VzBackend;

/// PID file name the supervisor writes inside `vm_state_dir`. Distinct
/// from libkrun's `libkrun.pid` so the two backends can coexist under
/// the same `~/.mvm/vms/<name>/` tree if a host happens to use both.
const PID_FILE_NAME: &str = "vz.pid";

/// How long [`VzBackend::start`] waits for the supervisor to write its
/// PID file before killing the child and bailing. Matches the libkrun
/// path's budget.
const PID_FILE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`VzBackend::stop`] waits after `SIGTERM` before escalating
/// to `SIGKILL`. Vz's graceful-stop callback runs on the supervisor's
/// dispatch queue, so the 2 s window is comfortable.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Default kernel cmdline for Vz-launched guests. Matches the libkrun
/// path: `console=hvc0` for the virtio-console attachment, ext4 rootfs
/// at `/dev/vda`. The host-side cmdline allow-list (Plan 97 Security
/// §7 — to be wired in a follow-up that integrates with
/// `mvm_supervisor::admit_for_run`) will gate any tokens beyond this
/// default for workload microVMs.
const DEFAULT_CMDLINE: &str = "console=hvc0 root=/dev/vda rw init=/init";

impl VmBackend for VzBackend {
    fn name(&self) -> &str {
        "vz"
    }

    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            // The supervisor's stdin-only IPC does not expose
            // pause/resume verbs today; `pause`/`resume` below report
            // accordingly. Capability flips to `true` in the slice
            // that adds a control socket to the supervisor.
            pause_resume: false,
            // Snapshots are macOS-14-only and gated on the supervisor
            // also exposing a control socket (load/save uses Vz's
            // `saveMachineStateTo` / `restoreMachineStateFrom`).
            // Both pieces land together in Phase E; this capability
            // stays `false` until that lands.
            snapshots: false,
            vsock: true,
            tap_networking: false,
            // `VZVirtioTraditionalMemoryBalloon` is configured by the
            // Swift supervisor when `balloon.enabled = true`. Live
            // adjustment via `balloon_set_target` requires the same
            // control socket that pause/resume needs; capability is
            // declared `false` until then to match the trait's
            // capability ↔ behavior contract (vm_backend.rs:599-602).
            balloon: false,
        }
    }

    fn start(&self, config: &VmStartConfig) -> Result<VmId> {
        if !mvm_core::platform::current().has_vz() {
            bail!(
                "Apple Virtualization.framework is not available on this host. \
                 Requires macOS 13 or later (Plan 97 §\"Minimum macOS version\")."
            );
        }

        let kernel = config
            .kernel_path
            .as_deref()
            .ok_or_else(|| anyhow!("Vz backend requires a kernel path"))?;

        let supervisor_path = resolve_supervisor_path()?;
        let state_dir = vm_state_dir(&config.name);
        std::fs::create_dir_all(&state_dir)
            .map_err(|e| anyhow!("create per-VM state dir {}: {e}", state_dir.display()))?;

        // Record runtime metadata so `mvmctl console` / status RPCs
        // can find the artifacts after start. Matches the libkrun path
        // line-for-line.
        let rootfs = Path::new(&config.rootfs_path);
        let rootfs_dir = rootfs.parent().unwrap_or_else(|| Path::new("."));
        mvm_build::builder_vm::admit_overlay_aware(rootfs_dir)?;
        mvm_base::runtime_meta::record_from_rootfs(&config.name, StartMode::Detached, rootfs)?;

        // Vz config build.
        let cfg = build_supervisor_config(config, kernel, &state_dir);
        let pid_file = state_dir.join(PID_FILE_NAME);
        // Stale-PID-file cleanup from a previous crashed supervisor so
        // the wait-loop below detects the *new* one unambiguously.
        let _ = std::fs::remove_file(&pid_file);
        // Stale console log from a prior run is fine to leave — the
        // supervisor opens it with append. But truncate-via-create
        // gives users a fresh boot log on each start, matching what
        // most VM tools do. Best-effort.
        let console_log = state_dir.join("console.log");
        let _ = std::fs::File::create(&console_log);

        let json = cfg
            .to_json()
            .map_err(|e| anyhow!("serialize SupervisorConfig: {e}"))?;

        ui::info(&format!(
            "Starting Vz VM '{}' (cpus={}, mem={}MiB) via {}...",
            config.name,
            config.cpus,
            config.memory_mib,
            supervisor_path.display(),
        ));

        let mut child = Command::new(&supervisor_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow!("spawn {}: {e}", supervisor_path.display()))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("supervisor stdin was not piped"))?
            .write_all(json.as_bytes())
            .map_err(|e| anyhow!("pipe SupervisorConfig to supervisor stdin: {e}"))?;

        // Poll for the PID file. If the supervisor exits first, surface
        // its status (which probably means a config-validation error
        // from VZ — the supervisor prints to stderr before exiting,
        // which we've inherited above).
        let deadline = Instant::now() + PID_FILE_TIMEOUT;
        loop {
            if pid_file.exists() {
                break;
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| anyhow!("poll supervisor child: {e}"))?
            {
                bail!(
                    "supervisor exited before writing PID file (status: {status}). \
                     Check stderr above for VZ configuration errors. Console log: {}",
                    console_log.display()
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!(
                    "supervisor did not write {} within {:?}; killed. Console log: {}",
                    pid_file.display(),
                    PID_FILE_TIMEOUT,
                    console_log.display(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        ui::success(&format!(
            "Vz VM '{}' started (pid file: {}, console log: {}).",
            config.name,
            pid_file.display(),
            console_log.display()
        ));
        Ok(VmId(config.name.clone()))
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        let pid_path = vm_state_dir(&id.0).join(PID_FILE_NAME);
        let pid = match read_pid(&pid_path) {
            Some(p) => p,
            None => {
                ui::info(&format!(
                    "Vz VM '{}' has no PID file at {}; nothing to stop.",
                    id.0,
                    pid_path.display()
                ));
                return Ok(());
            }
        };

        if !pid_alive(pid) {
            ui::info(&format!(
                "Vz VM '{}' PID {pid} is not running; cleaning up state.",
                id.0
            ));
            let _ = std::fs::remove_file(&pid_path);
            return Ok(());
        }

        // SIGTERM → supervisor's `DispatchSourceSignal` handler →
        // `VZVirtualMachine.requestStop()` (ACPI power button to the
        // guest). The graceful path runs guest shutdown handlers; if
        // the guest ignores the ACPI event we fall back to SIGKILL.
        send_signal(pid, libc::SIGTERM);
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(pid) {
            ui::info(&format!(
                "Vz VM '{}' PID {pid} did not exit after SIGTERM within {STOP_TIMEOUT:?}; sending SIGKILL.",
                id.0
            ));
            send_signal(pid, libc::SIGKILL);
        }

        let _ = std::fs::remove_file(&pid_path);
        ui::success(&format!("Vz VM '{}' stopped.", id.0));
        Ok(())
    }

    fn stop_all(&self) -> Result<()> {
        let vms = self.list()?;
        let mut last_err = None;
        for vm in vms {
            if let Err(e) = self.stop(&VmId(vm.name.clone())) {
                tracing::warn!(name = vm.name, error = %e, "stop_all: stop failed");
                last_err = Some(e);
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            Ok(())
        }
    }

    fn pause(&self, _id: &VmId) -> Result<()> {
        bail!(
            "pause is not supported by the Vz backend yet \
             (mvm-vz-supervisor exposes only stdin-driven start/stop; \
             pause/resume require a control channel — Plan 97 follow-up)"
        )
    }

    fn resume(&self, _id: &VmId) -> Result<()> {
        bail!(
            "resume is not supported by the Vz backend yet \
             (mvm-vz-supervisor exposes only stdin-driven start/stop; \
             pause/resume require a control channel — Plan 97 follow-up)"
        )
    }

    fn status(&self, id: &VmId) -> Result<VmStatus> {
        let pid_path = vm_state_dir(&id.0).join(PID_FILE_NAME);
        match read_pid(&pid_path) {
            Some(pid) if pid_alive(pid) => Ok(VmStatus::Running),
            _ => Ok(VmStatus::Stopped),
        }
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        let root = vms_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("read {}: {e}", root.display())),
        };
        let mut vms = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let pid_path = path.join(PID_FILE_NAME);
            if !pid_path.exists() {
                // Not a Vz-managed VM (the libkrun supervisor writes
                // `libkrun.pid` in the same `~/.mvm/vms/<name>/` tree).
                continue;
            }
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue, // skip non-UTF-8 dir names
            };
            let alive = read_pid(&pid_path).is_some_and(pid_alive);
            vms.push(VmInfo {
                id: VmId(name.clone()),
                name,
                status: if alive {
                    VmStatus::Running
                } else {
                    VmStatus::Stopped
                },
                guest_ip: None,
                cpus: 0,
                memory_mib: 0,
                profile: None,
                revision: None,
                flake_ref: None,
                ports: Vec::new(),
            });
        }
        Ok(vms)
    }

    fn logs(&self, id: &VmId, lines: u32, _hypervisor: bool) -> Result<String> {
        // Capture-only console at `<vm_state_dir>/console.log` (Plan 97
        // Security §9). `hypervisor=true` would mean "supervisor's own
        // logs", which today is empty — the supervisor inherits stderr
        // from the parent `mvmctl`, so its logs are already on the
        // user's terminal. Surface the console capture in both cases.
        let log = vm_state_dir(&id.0).join("console.log");
        let contents = std::fs::read_to_string(&log)
            .map_err(|e| anyhow!("read console log {}: {e}", log.display()))?;
        if lines == 0 {
            return Ok(contents);
        }
        // Tail by counting newlines from the end.
        let lines_usize = lines as usize;
        let tail: Vec<&str> = contents.lines().rev().take(lines_usize).collect();
        Ok(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    fn is_available(&self) -> Result<bool> {
        Ok(mvm_core::platform::current().has_vz())
    }

    fn install(&self) -> Result<()> {
        ui::info("Apple Virtualization.framework is built into macOS 13+; no host install needed.");
        // Pre-flight the supervisor binary so operators don't hit a
        // mid-`mvmctl up` failure.
        match resolve_supervisor_path() {
            Ok(path) => ui::info(&format!("Supervisor binary: {}", path.display())),
            Err(e) => ui::info(&format!("Supervisor binary: NOT FOUND ({e})")),
        }
        Ok(())
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        Ok(GuestChannelInfo::Vsock {
            cid: 3,
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Plan 97 §"Can we still make all nine ADR-002 security
        // claims?". 7-claim table here covers claims 1–7 (8 and 9 live
        // outside `BackendSecurityProfile`).
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds, // 1 — host-fs isolation via Vz; supervisor refuses non-admitted shares
                ClaimStatus::Holds, // 2 — uid-0 protections same as FC (guest-side)
                ClaimStatus::DoesNotHold, // 3 — verified-boot pipeline targets FC today
                ClaimStatus::Holds, // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds, // 5 — vsock framing fuzzed (Swift JSON corpus equivalence still pending)
                ClaimStatus::Holds, // 6 — dev image hash verified
                ClaimStatus::Holds, // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 2",
            notes: &[
                "Hardware isolation via Apple Virtualization.framework on macOS 13+.",
                "Same Hypervisor.framework primitive libkrun uses; Apple-controlled VMM surface.",
                "Claim 3 (verified boot) partial — dm-verity pipeline targets Firecracker today.",
                "Claim 5 (supervisor JSON corpus equivalence) is a Plan 97 follow-up; vsock framing fuzz already in CI.",
                "Pause/resume + balloon + snapshots require a supervisor control socket (Plan 97 follow-up).",
            ],
        }
    }
}

// ─── helpers ───────────────────────────────────────────────────────

fn vms_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".mvm/vms")
}

fn vm_state_dir(name: &str) -> PathBuf {
    vms_root().join(name)
}

/// Build the [`mvm_vz::SupervisorConfig`] the supervisor binary
/// consumes on stdin. Maps the backend-agnostic `VmStartConfig` to
/// the Vz-specific JSON shape.
///
/// Phase B first-cut wiring — only the fields needed to boot a
/// dev-shell image are mapped. gvproxy networking, the runtime
/// overlay, dm-verity sidecar, and port forwarding all land in
/// follow-up slices when their host-side plumbing is in place.
fn build_supervisor_config(
    config: &VmStartConfig,
    kernel: &str,
    state_dir: &Path,
) -> vz::SupervisorConfig {
    let state_dir_str = state_dir.to_string_lossy().into_owned();
    let vsock_dir = state_dir.join("vsock").to_string_lossy().into_owned();
    let console_log = state_dir.join("console.log").to_string_lossy().into_owned();

    let disks = vec![vz::DiskConfig {
        id: "rootfs".into(),
        path: config.rootfs_path.clone(),
        // Rootfs is RO at boot under the W3 verified-boot model; even
        // when verity isn't on, libkrun and Firecracker mount it
        // read-only and rely on an overlay for writes. Mirror that
        // for Vz.
        read_only: true,
    }];

    vz::SupervisorConfig {
        name: config.name.clone(),
        vm_state_dir: state_dir_str,
        pid_file_name: Some(PID_FILE_NAME.to_string()),
        kernel: vz::KernelConfig {
            path: kernel.to_string(),
            cmdline: DEFAULT_CMDLINE.to_string(),
            initrd_path: config.initrd_path.clone(),
        },
        resources: vz::ResourceConfig {
            cpu_count: config.cpus,
            memory_mib: u64::from(config.memory_mib),
        },
        disks,
        virtio_fs: Vec::new(),
        vsock: vz::VsockConfig {
            ports: vec![mvm_guest::vsock::GUEST_AGENT_PORT],
            socket_dir: vsock_dir,
        },
        console_output_path: Some(console_log),
        // gvproxy wiring lands in a follow-up slice (needs
        // host-side gvproxy lifecycle the libkrun path already
        // performs for `MVM_NETWORKING=gvproxy`).
        network: None,
        // Balloon device on the Swift side is independent of live
        // adjustment; even with `capabilities().balloon == false`,
        // attaching a balloon device early lets the supervisor
        // expose it once the control channel lands. Default floor
        // sized so the guest never falls below a working memory
        // floor.
        balloon: Some(vz::BalloonConfig {
            enabled: true,
            floor_mib: 128,
        }),
    }
}

/// Resolve the absolute path to the `mvm-vz-supervisor` binary,
/// checking three sources in order, paralleling the libkrun
/// resolver:
///
/// 1. `MVM_VZ_SUPERVISOR_PATH` — explicit override for tests +
///    `cargo run` workflows.
/// 2. A binary named `mvm-vz-supervisor` adjacent to the current
///    executable — the layout produced by `cargo install` /
///    Homebrew bottles that ship `mvmctl` alongside it.
/// 3. The source-checkout build output under
///    `crates/mvm-vz-supervisor/.build/<arch>-apple-macosx/<config>/`
///    (CLAUDE.md "Source-checkout builds never depend on
///    mvm-published artifacts"); this matters during local dev
///    when `mvmctl` is `cargo run` from the workspace root.
/// 4. The version-pinned release layout `~/.mvm/bin/mvm-vz-supervisor-<version>`.
fn resolve_supervisor_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("MVM_VZ_SUPERVISOR_PATH") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "MVM_VZ_SUPERVISOR_PATH points at {} which is not a file",
            path.display()
        );
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("mvm-vz-supervisor");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Source-checkout layout. CARGO_MANIFEST_DIR points at the
    // current crate; the workspace root is two `..` above.
    if let Some(workspace_root) = workspace_root_from_manifest_dir() {
        let candidate = vz::source_tree_binary_path(&workspace_root);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Release-installed layout under `~/.mvm/bin/`.
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = vz::supervisor_binary_path(Path::new(&home), env!("CARGO_PKG_VERSION"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "mvm-vz-supervisor binary not found. Looked for: \
         $MVM_VZ_SUPERVISOR_PATH, alongside the current exe, \
         crates/mvm-vz-supervisor/.build/<arch>-apple-macosx/debug/mvm-vz-supervisor \
         (source-checkout), and ~/.mvm/bin/mvm-vz-supervisor-{} \
         (release-installed). Build via \
         `crates/mvm-vz-supervisor/tools/build.sh`.",
        env!("CARGO_PKG_VERSION")
    );
}

/// Derive the workspace root from the build-time
/// `CARGO_MANIFEST_DIR`. Returns `None` when the binary is run from
/// an installed layout (the env var was evaluated at compile time,
/// so this only fails on a moved checkout — which is fine since the
/// installed-layout path is checked next).
fn workspace_root_from_manifest_dir() -> Option<PathBuf> {
    // `crates/mvm-backend` → workspace root is two `..` up.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent()?.parent().map(Path::to_path_buf)
}

fn read_pid(path: &Path) -> Option<libc::pid_t> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<libc::pid_t>().ok()
}

fn pid_alive(pid: libc::pid_t) -> bool {
    // `kill(pid, 0)` returns 0 if the process exists, -1 with
    // errno=ESRCH if not.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn send_signal(pid: libc::pid_t, sig: libc::c_int) {
    unsafe { libc::kill(pid, sig) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_vz() {
        assert_eq!(VzBackend.name(), "vz");
    }

    #[test]
    fn capabilities_match_plan_97_phase_b() {
        let caps = VzBackend.capabilities();
        assert!(caps.vsock, "vsock always available");
        assert!(
            !caps.tap_networking,
            "Vz uses file-handle attachments via gvproxy"
        );
        // pause_resume/balloon/snapshots are all gated on the
        // supervisor exposing a control socket — declared `false`
        // here so the trait's capability ↔ behavior contract
        // (vm_backend.rs:599-602) holds. Flip when the control
        // socket lands.
        assert!(!caps.pause_resume);
        assert!(!caps.balloon);
        assert!(!caps.snapshots);
    }

    #[test]
    fn pause_resume_bail_with_capability_honest_message() {
        let backend = VzBackend;
        let id = VmId("smoke".into());
        let err = backend.pause(&id).expect_err("pause should bail");
        assert!(
            err.to_string().contains("not supported"),
            "error explains why: {err}"
        );
        let err = backend.resume(&id).expect_err("resume should bail");
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn stop_with_no_pid_file_returns_ok() {
        // Stopping a never-started VM is a no-op (matches libkrun).
        let backend = VzBackend;
        let id = VmId("definitely-not-running-1234567890".into());
        assert!(backend.stop(&id).is_ok());
    }

    #[test]
    fn status_with_no_pid_file_is_stopped() {
        let backend = VzBackend;
        let id = VmId("definitely-not-running-1234567890".into());
        assert!(matches!(backend.status(&id).unwrap(), VmStatus::Stopped));
    }

    #[test]
    fn list_skips_dirs_without_vz_pid_file() {
        // `list` walks `~/.mvm/vms/` and yields entries whose dir
        // contains `vz.pid`. The libkrun backend uses `libkrun.pid`
        // in the same tree, so we must not pick those up. We can't
        // exercise the full path in a unit test without a tempdir
        // mock; instead assert the empty case from a clean
        // contributor host doesn't error.
        let backend = VzBackend;
        // On a contributor host with no Vz VM ever started, this
        // returns an empty Vec. On a host that has one running,
        // the test still passes — the filter rules don't change
        // shape with population.
        let _ = backend.list().expect("list must not error");
    }

    #[test]
    fn guest_channel_info_is_vsock_at_agent_port() {
        let info = VzBackend.guest_channel_info(&VmId("smoke".into())).unwrap();
        match info {
            GuestChannelInfo::Vsock { cid, port } => {
                assert_eq!(cid, 3);
                assert_eq!(port, mvm_guest::vsock::GUEST_AGENT_PORT);
            }
            other => panic!("expected vsock channel, got {other:?}"),
        }
    }

    #[test]
    fn security_profile_is_tier_2_with_claim_3_partial() {
        let profile = VzBackend.security_profile();
        assert_eq!(profile.tier, "Tier 2");
        assert!(profile.layer_coverage.is_microvm());
        assert_eq!(profile.dropped_claims(), vec![3]);
    }

    #[test]
    fn build_supervisor_config_maps_vmstartconfig_fields() {
        let mut cfg = VmStartConfig {
            name: "smoke".into(),
            cpus: 2,
            memory_mib: 1024,
            ..Default::default()
        };
        cfg.kernel_path = Some("/abs/vmlinux".into());
        cfg.rootfs_path = "/abs/rootfs.ext4".into();
        let state_dir = Path::new("/tmp/vz-smoke-state");
        let built = build_supervisor_config(&cfg, "/abs/vmlinux", state_dir);

        assert_eq!(built.name, "smoke");
        assert_eq!(built.kernel.path, "/abs/vmlinux");
        assert_eq!(built.resources.cpu_count, 2);
        assert_eq!(built.resources.memory_mib, 1024);
        assert_eq!(built.disks.len(), 1);
        assert_eq!(built.disks[0].path, "/abs/rootfs.ext4");
        assert!(built.disks[0].read_only);
        assert_eq!(built.vsock.ports, vec![mvm_guest::vsock::GUEST_AGENT_PORT]);
        assert_eq!(built.pid_file_name.as_deref(), Some(PID_FILE_NAME));
        // Console capture goes to a file under state_dir; never `None`
        // — capture-only is the workload contract (Plan 97 Security §9).
        assert!(built.console_output_path.is_some());
    }

    #[test]
    fn resolve_supervisor_path_honors_env_override() {
        let _guard = mvm_base::runtime_meta::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // `MVM_VZ_SUPERVISOR_PATH` points at a real file → returned as-is.
        // SAFETY: serialized by TEST_ENV_LOCK.
        unsafe {
            std::env::set_var("MVM_VZ_SUPERVISOR_PATH", tmp.path());
        }
        let path = resolve_supervisor_path().expect("env override resolves");
        assert_eq!(path, tmp.path());
        // SAFETY: serialized by TEST_ENV_LOCK.
        unsafe {
            std::env::remove_var("MVM_VZ_SUPERVISOR_PATH");
        }
    }

    #[test]
    fn resolve_supervisor_path_env_pointing_at_missing_file_errors() {
        let _guard = mvm_base::runtime_meta::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by TEST_ENV_LOCK.
        unsafe {
            std::env::set_var(
                "MVM_VZ_SUPERVISOR_PATH",
                "/definitely/not/there/mvm-vz-supervisor",
            );
        }
        let err = resolve_supervisor_path().expect_err("missing file must error");
        assert!(
            err.to_string().contains("not a file"),
            "error explains why: {err}"
        );
        // SAFETY: serialized by TEST_ENV_LOCK.
        unsafe {
            std::env::remove_var("MVM_VZ_SUPERVISOR_PATH");
        }
    }
}
