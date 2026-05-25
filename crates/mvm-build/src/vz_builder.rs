//! Apple Virtualization.framework (Vz) backend for the builder VM,
//! parallel to [`libkrun_builder::LibkrunBuilderBackend`].
//!
//! Plan 97 Phase C, second `VmBackendForBuilder` impl. Owns the
//! Vz-specific spawn (`mvm-vz-supervisor` binary with
//! [`mvm_vz::SupervisorConfig`] JSON on stdin) and surfaces the
//! resulting [`BuilderVmExitInfo`] back to the seam.
//!
//! ## Scope
//!
//! This module is the **trait impl only**. The high-level
//! `VzBuilderVm` driver that wraps `BuilderVmRuntime` around this
//! backend (mirroring `LibkrunBuilderVm`) lands in a follow-up slice.
//! Today the impl exists so the next slice has a worked example of
//! the seam against a second hypervisor.
//!
//! ## Why no panic detector
//!
//! Plan 77 W6's panic detector exists for libkrun because
//! `krun_start_enter` blocks indefinitely on a panicked guest —
//! `Child::wait()` never returns, so a host-side console-log watcher
//! has to kill the supervisor. The Vz Swift supervisor uses
//! `VZVirtualMachine.start()` instead and exits cleanly when the guest
//! powers off or panics (Plan 97 Phase A's `main.swift` contract: 0
//! clean / 1 guest error / 2 config parse / 3 supervisor startup).
//! So the Vz path can rely on `Child::wait()` plus a wall-clock
//! timeout — no console-log polling needed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use crate::builder_vm::{
    BuilderVmDisk, BuilderVmError, BuilderVmExitInfo, BuilderVmMount, BuilderVmRunConfig,
    VmBackendForBuilder,
};
use crate::libkrun_builder::BuilderVmImage;

/// Standard kernel cmdline the Vz supervisor pairs with the builder
/// VM's rootfs. Matches `mvm_backend::vz::DEFAULT_CMDLINE`'s shape —
/// console on the virtio-hvc port (so [`Self::console_log_path`]
/// captures it), rootfs as the first virtio-blk device, builder VM
/// init at `/init`.
///
/// The cmdline from [`BuilderVmImage::Rootfs::cmdline`] takes
/// precedence when non-empty; this constant is the fallback so
/// callers that pass an empty cmdline get a sensible boot.
pub const DEFAULT_VZ_BUILDER_CMDLINE: &str = "console=hvc0 root=/dev/vda ro init=/init panic=1";

/// Vz parallel of [`libkrun_builder::LibkrunBuilderBackend`]. Holds
/// the resolved supervisor binary path + cached builder VM image so
/// a missing prerequisite surfaces at `new()`-time, not mid-build.
///
/// Only the [`BuilderVmImage::Rootfs`] variant is supported — Vz has
/// no analog of libkrun's `krun_set_root` directory-as-rootfs path.
/// [`Self::new_with_rootfs_image`] enforces the variant at
/// construction so `run_attached_with_mounts` can't be reached with
/// an incompatible image.
#[derive(Debug)]
pub struct VzBuilderBackend {
    supervisor_path: PathBuf,
    image: BuilderVmImage,
}

impl VzBuilderBackend {
    /// Construct over an already-resolved supervisor path + image.
    /// Refuses [`BuilderVmImage::RootDir`] — that variant is
    /// libkrun-only. Used by [`Self::new`] and by tests that want to
    /// inject a fake supervisor without touching the filesystem.
    pub fn new_with_rootfs_image(
        supervisor_path: PathBuf,
        image: BuilderVmImage,
    ) -> Result<Self, BuilderVmError> {
        match &image {
            BuilderVmImage::Rootfs { .. } => {}
            BuilderVmImage::RootDir { .. } => {
                return Err(BuilderVmError::ExtractionFailed(
                    "VzBuilderBackend requires a Rootfs image; RootDir is libkrun-specific \
                     (krun_set_root has no Vz analog)"
                        .to_string(),
                ));
            }
        }
        Ok(Self {
            supervisor_path,
            image,
        })
    }
}

impl VmBackendForBuilder for VzBuilderBackend {
    fn run_attached_with_mounts(
        &self,
        config: &BuilderVmRunConfig,
        mounts: &[BuilderVmMount],
        extra_disks: &[BuilderVmDisk],
        timeout: Duration,
    ) -> Result<BuilderVmExitInfo, BuilderVmError> {
        if !mvm_core::platform::current().has_vz() {
            return Err(BuilderVmError::ExtractionFailed(
                "Apple Virtualization.framework is not available on this host. \
                 Requires macOS 13 or later."
                    .to_string(),
            ));
        }

        std::fs::create_dir_all(&config.vm_state_dir).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating builder VM state dir {}: {e}",
                config.vm_state_dir.display()
            ))
        })?;

        let cfg = build_vz_supervisor_config(config, &self.image, mounts, extra_disks)?;
        let json = cfg.to_json().map_err(|e| {
            BuilderVmError::ExtractionFailed(format!("serialize SupervisorConfig: {e}"))
        })?;

        let mut child = Command::new(&self.supervisor_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                BuilderVmError::ExtractionFailed(format!(
                    "spawn mvm-vz-supervisor at {}: {e}",
                    self.supervisor_path.display()
                ))
            })?;
        child
            .stdin
            .take()
            .ok_or_else(|| {
                BuilderVmError::ExtractionFailed(
                    "mvm-vz-supervisor stdin was not piped after spawn".to_string(),
                )
            })?
            .write_all(json.as_bytes())
            .map_err(|e| {
                BuilderVmError::ExtractionFailed(format!(
                    "pipe SupervisorConfig JSON to mvm-vz-supervisor stdin: {e}"
                ))
            })?;

        match wait_with_timeout(child, timeout) {
            Ok(Some(code)) => Ok(BuilderVmExitInfo {
                exit_code: Some(code),
                panic_line: None,
            }),
            Ok(None) => Err(BuilderVmError::NixBuildFailed(format!(
                "builder VM exceeded {} seconds wall-clock; killed. Console log at {}.",
                timeout.as_secs(),
                self.console_log_path(&config.vm_state_dir).display(),
            ))),
            Err(e) => Err(BuilderVmError::ExtractionFailed(format!(
                "wait on mvm-vz-supervisor child: {e}"
            ))),
        }
    }

    fn console_log_path(&self, vm_state_dir: &Path) -> PathBuf {
        // Same path the libkrun backend uses + the Vz supervisor
        // already captures into via `console_output_path`. Both
        // backends keep the file at the same relative location so
        // operators don't have to know which backend produced a
        // given run.
        vm_state_dir.join("console.log")
    }
}

/// Build the Vz [`mvm_vz::SupervisorConfig`] from the trait's
/// hypervisor-agnostic inputs. Pulled out of
/// `run_attached_with_mounts` so unit tests can exercise the mapping
/// without spawning a supervisor.
fn build_vz_supervisor_config(
    config: &BuilderVmRunConfig,
    image: &BuilderVmImage,
    mounts: &[BuilderVmMount],
    extra_disks: &[BuilderVmDisk],
) -> Result<mvm_vz::SupervisorConfig, BuilderVmError> {
    let BuilderVmImage::Rootfs {
        kernel_path,
        rootfs_path,
        cmdline,
    } = image
    else {
        // Guarded at construction; double-check so this stays sound
        // if a future refactor exposes a builder that skips the
        // variant check.
        return Err(BuilderVmError::ExtractionFailed(
            "VzBuilderBackend reached run with non-Rootfs image".to_string(),
        ));
    };

    let kernel = path_to_string(kernel_path, "kernel_path")?;
    let rootfs = path_to_string(rootfs_path, "rootfs_path")?;
    let state_dir = path_to_string(&config.vm_state_dir, "vm_state_dir")?;
    let vsock_dir = config
        .vm_state_dir
        .join("vsock")
        .to_string_lossy()
        .into_owned();
    let console_log = config
        .vm_state_dir
        .join("console.log")
        .to_string_lossy()
        .into_owned();

    let effective_cmdline = if cmdline.is_empty() {
        DEFAULT_VZ_BUILDER_CMDLINE.to_string()
    } else {
        cmdline.clone()
    };

    let mut disks = vec![mvm_vz::DiskConfig {
        id: "rootfs".to_string(),
        path: rootfs,
        // Rootfs is RO at boot (matches the W3 verified-boot model
        // + every other backend's posture). Writes go to overlays
        // / extra disks.
        read_only: true,
    }];
    for d in extra_disks {
        disks.push(mvm_vz::DiskConfig {
            id: d.id.clone(),
            path: path_to_string(&d.host_path, "extra_disk")?,
            read_only: d.read_only,
        });
    }

    let mut virtio_fs = Vec::with_capacity(mounts.len());
    for m in mounts {
        virtio_fs.push(mvm_vz::VirtioFsShare {
            tag: m.tag.clone(),
            host_path: path_to_string(&m.host_path, "mount_host_path")?,
            read_only: m.read_only,
        });
    }

    let initrd_path = match &config.initrd_path {
        Some(p) => Some(path_to_string(p, "initrd_path")?),
        None => None,
    };

    Ok(mvm_vz::SupervisorConfig {
        name: config.name.clone(),
        vm_state_dir: state_dir,
        pid_file_name: Some("builder.pid".to_string()),
        kernel: mvm_vz::KernelConfig {
            path: kernel,
            cmdline: effective_cmdline,
            initrd_path,
        },
        resources: mvm_vz::ResourceConfig {
            // BuilderVmRunConfig.vcpus is u8 (caller-bound by host
            // cap checks); mvm_vz::ResourceConfig.cpu_count is u32.
            cpu_count: u32::from(config.vcpus),
            memory_mib: u64::from(config.memory_mib),
        },
        disks,
        virtio_fs,
        vsock: mvm_vz::VsockConfig {
            ports: config.vsock_ports.clone(),
            socket_dir: vsock_dir,
        },
        console_output_path: Some(console_log),
        // Network wiring (gvproxy) lands with the high-level
        // VzBuilderVm driver; bare run_attached_with_mounts callers
        // accept no-network builds (works for offline / cached
        // derivations only). Plan 88 §"Cross-platform backends"
        // is the follow-up.
        network: None,
        balloon: None,
        control_socket_path: None,
        startup_mode: mvm_vz::StartupMode::Boot,
    })
}

/// Block on `child` until it exits or `timeout` elapses. On timeout,
/// kill the supervisor and return `Ok(None)` so the caller can format
/// a wall-clock error pointing at the console log. Thread-based
/// because `std::process::Child` has no async wait on stable.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> std::io::Result<Option<i32>> {
    let id = child.id();
    let (tx, rx) = channel();
    // The wait happens in a separate thread so the main thread can
    // arm the wall-clock timer with a `recv_timeout`. The child
    // handle is moved into the thread so its `Drop` (which would
    // otherwise reap the process unconditionally) runs there only
    // on the success path.
    let join = thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send((child, status));
    });

    match rx.recv_timeout(timeout) {
        Ok((_child, status)) => {
            let _ = join.join();
            // The supervisor's exit code matches the guest's exit
            // per Plan 97 Phase A's main.swift contract. None for
            // signal-terminated supervisors (rare but possible
            // under SIGKILL).
            Ok(status?.code())
        }
        Err(RecvTimeoutError::Timeout) => {
            // Drop the receiver before killing so the spawned
            // thread's send doesn't block on a dead channel.
            drop(rx);
            // Kill by pid — the thread still owns the `Child`
            // handle, so we can't call `.kill()` directly. SIGKILL
            // because the supervisor is supposed to be a
            // well-behaved child; if it ignored SIGTERM we'd be
            // waiting on a wedged process anyway.
            //
            // Safety: `id` is the pid we got from `Child::id()`,
            // which is guaranteed valid until the OS reaps the
            // process. The spawned thread's `Child::wait()` is what
            // does the reaping, and it's still running, so this
            // kill races safely with that wait.
            #[cfg(unix)]
            unsafe {
                libc::kill(id as i32, libc::SIGKILL);
            }
            #[cfg(not(unix))]
            {
                // No portable cross-platform pid-kill; on non-unix
                // we wait for the watcher thread to surface the
                // child and kill via its handle. Drops the timeout
                // contract slightly, but Vz is macOS-only so this
                // path is unreachable in practice.
                let _ = id;
            }
            let _ = join.join();
            Ok(None)
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = join.join();
            Err(std::io::Error::other(
                "wait-with-timeout thread panicked before sending exit status",
            ))
        }
    }
}

fn path_to_string(p: &Path, field: &str) -> Result<String, BuilderVmError> {
    p.to_str().map(str::to_string).ok_or_else(|| {
        BuilderVmError::ExtractionFailed(format!(
            "{field} path {} is not valid UTF-8 (Swift supervisor JSON requires UTF-8 strings)",
            p.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rootfs_image() -> BuilderVmImage {
        BuilderVmImage::Rootfs {
            kernel_path: PathBuf::from("/tmp/vmlinux"),
            rootfs_path: PathBuf::from("/tmp/rootfs.ext4"),
            cmdline: "console=hvc0 root=/dev/vda".to_string(),
        }
    }

    fn rundir_image() -> BuilderVmImage {
        BuilderVmImage::RootDir {
            root_dir: PathBuf::from("/tmp/root"),
            entry_path: "/init".to_string(),
        }
    }

    fn run_config() -> BuilderVmRunConfig {
        BuilderVmRunConfig {
            name: "builder-vz-test".to_string(),
            kernel_path: PathBuf::from("/tmp/unused-vmlinux"),
            kernel_cmdline: String::new(),
            initrd_path: None,
            vcpus: 4,
            memory_mib: 8192,
            vsock_ports: vec![5252, 5253],
            vm_state_dir: PathBuf::from("/tmp/mvm-test/builder-vz"),
        }
    }

    #[test]
    fn rejects_root_dir_image_at_construction() {
        let err = VzBuilderBackend::new_with_rootfs_image(
            PathBuf::from("/tmp/supervisor"),
            rundir_image(),
        )
        .expect_err("RootDir variant must be rejected");
        assert!(
            format!("{err}").contains("RootDir is libkrun-specific"),
            "got: {err}"
        );
    }

    #[test]
    fn console_log_lives_under_vm_state_dir() {
        let backend = VzBuilderBackend::new_with_rootfs_image(
            PathBuf::from("/tmp/supervisor"),
            rootfs_image(),
        )
        .expect("Rootfs accepted");
        let dir = PathBuf::from("/tmp/state/builder-vz-foo");
        let log = backend.console_log_path(&dir);
        assert_eq!(log, dir.join("console.log"));
        // Trait-object safety check — pins object-safety for the
        // future BuilderVmRuntime caller that holds `&dyn`.
        let _erased: &dyn VmBackendForBuilder = &backend;
    }

    #[test]
    fn build_supervisor_config_maps_run_config_fields() {
        let cfg = build_vz_supervisor_config(&run_config(), &rootfs_image(), &[], &[])
            .expect("build supervisor config");
        assert_eq!(cfg.name, "builder-vz-test");
        assert_eq!(cfg.resources.cpu_count, 4);
        assert_eq!(cfg.resources.memory_mib, 8192);
        assert_eq!(cfg.kernel.path, "/tmp/vmlinux");
        assert_eq!(cfg.kernel.cmdline, "console=hvc0 root=/dev/vda");
        assert_eq!(cfg.disks.len(), 1);
        assert_eq!(cfg.disks[0].id, "rootfs");
        assert!(cfg.disks[0].read_only, "rootfs always RO at boot");
        assert_eq!(cfg.vsock.ports, vec![5252, 5253]);
        assert!(cfg.console_output_path.is_some());
        // pid_file_name must be set so concurrent builder VMs don't
        // race on a shared PID file inside the state dir.
        assert_eq!(cfg.pid_file_name.as_deref(), Some("builder.pid"));
    }

    #[test]
    fn build_supervisor_config_falls_back_to_default_cmdline_when_image_cmdline_empty() {
        let image = BuilderVmImage::Rootfs {
            kernel_path: PathBuf::from("/tmp/vmlinux"),
            rootfs_path: PathBuf::from("/tmp/rootfs.ext4"),
            cmdline: String::new(),
        };
        let cfg =
            build_vz_supervisor_config(&run_config(), &image, &[], &[]).expect("build with empty");
        assert_eq!(cfg.kernel.cmdline, DEFAULT_VZ_BUILDER_CMDLINE);
        assert!(
            cfg.kernel.cmdline.contains("init=/init"),
            "default must wire builder VM init: {}",
            cfg.kernel.cmdline
        );
    }

    #[test]
    fn build_supervisor_config_threads_extra_disks_and_mounts() {
        let mounts = vec![
            BuilderVmMount {
                tag: "work".into(),
                host_path: PathBuf::from("/host/work"),
                read_only: true,
            },
            BuilderVmMount {
                tag: "out".into(),
                host_path: PathBuf::from("/host/out"),
                read_only: false,
            },
        ];
        let disks = vec![BuilderVmDisk {
            id: "nix-store".into(),
            host_path: PathBuf::from("/host/nix-store.img"),
            read_only: false,
        }];
        let cfg = build_vz_supervisor_config(&run_config(), &rootfs_image(), &mounts, &disks)
            .expect("build supervisor config");

        // Rootfs is disks[0]; nix-store rides as disks[1] with the
        // caller-supplied read_only flag.
        assert_eq!(cfg.disks.len(), 2);
        assert_eq!(cfg.disks[1].id, "nix-store");
        assert!(!cfg.disks[1].read_only);

        // virtio_fs preserves order + read_only — Plan 97 §"Host-path
        // mounts" reads-only-by-default for /work and /job; /out is
        // the only writable share.
        assert_eq!(cfg.virtio_fs.len(), 2);
        assert_eq!(cfg.virtio_fs[0].tag, "work");
        assert!(cfg.virtio_fs[0].read_only);
        assert_eq!(cfg.virtio_fs[1].tag, "out");
        assert!(!cfg.virtio_fs[1].read_only);
    }

    #[test]
    fn build_supervisor_config_rejects_non_utf8_paths() {
        // Construct a non-UTF-8 PathBuf via OsStr. On macOS APFS this
        // is APFS-illegal but PathBuf still accepts it; the Vz
        // supervisor JSON shape requires UTF-8, so we must fail
        // closed before spawning.
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let bad = PathBuf::from(OsStr::from_bytes(&[0xff, 0xfe, b'/', b'x']));
            let image = BuilderVmImage::Rootfs {
                kernel_path: bad,
                rootfs_path: PathBuf::from("/tmp/rootfs"),
                cmdline: "x".to_string(),
            };
            let err = build_vz_supervisor_config(&run_config(), &image, &[], &[])
                .expect_err("non-UTF-8 path must reject");
            assert!(format!("{err}").contains("not valid UTF-8"), "got: {err}");
        }
    }
}
