//! `mvmctl dev` — manage the Apple Container / microsandbox development
//! environment.
//!
//! Plan-60 / ADR-013 dropped Lima from mvm. On macOS 26+ Apple Silicon,
//! `mvmctl dev` boots an Apple Container with Nix + build tools via
//! PTY-over-vsock console. The non-Apple-Container path (Linux without
//! KVM, macOS Intel, macOS pre-26) is wired up by W8 — until that lands,
//! `mvmctl dev` on those hosts surfaces a clear error pointing at the
//! plan-60 follow-up rather than running broken Lima code.

use anyhow::Result;
use clap::{Args as ClapArgs, Subcommand};

use crate::ui;

use mvm_core::user_config::MvmConfig;

use super::super::vm::console;
use super::Cli;
use super::apple_container;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: Option<DevAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum DevAction {
    /// Bootstrap and start the dev environment.
    Up {
        /// Number of vCPUs for the dev VM (Apple Container).
        #[arg(long, default_value = "8")]
        cpus: u32,
        /// Memory (GiB) for the dev VM (Apple Container).
        #[arg(long, default_value = "16")]
        memory: u32,
        /// Project directory to cd into inside the VM.
        #[arg(long)]
        project: Option<String>,
        /// Bind a Prometheus metrics endpoint on this port (0 = disabled).
        #[arg(long, default_value = "0")]
        metrics_port: u16,
        /// Reload ~/.mvm/config.toml automatically when it changes.
        #[arg(long)]
        watch_config: bool,
        /// Open an interactive shell after starting.
        #[arg(long, short = 's')]
        shell: bool,
    },
    /// Stop the development VM.
    Down {
        /// Also delete the cached dev image (vmlinux + rootfs.ext4) so the
        /// next `dev up` rebuilds from local source.
        #[arg(long)]
        reset: bool,
    },
    /// Open a shell in the running dev VM.
    Shell {
        /// Project directory to cd into inside the VM.
        #[arg(long)]
        project: Option<String>,
    },
    /// Show dev environment status.
    Status,
    /// Rebuild the dev environment (down + clear cache + up).
    Rebuild {
        /// Number of vCPUs for the dev VM (Apple Container).
        #[arg(long, default_value = "8")]
        cpus: u32,
        /// Memory (GiB) for the dev VM (Apple Container).
        #[arg(long, default_value = "16")]
        memory: u32,
        /// Open an interactive shell after rebuilding.
        #[arg(long, short = 's')]
        shell: bool,
    },
    /// Import a dev image from local files (air-gapped install).
    ///
    /// Plan 36 §"Air-gapped install path": runs the same cosign sig +
    /// SHA-256 + version-pin + max-age + revocation verification
    /// pipeline as the network path, against local files. On success
    /// the verified artifacts are deposited into the version-namespaced
    /// cache (`~/.cache/mvm/dev/prebuilt/v{version}/`) and the next
    /// `mvmctl dev up` boots from them without re-downloading.
    ///
    /// The trusted path for operators in regulated/gov/air-gapped
    /// environments. Without this, the only option to run an
    /// unreachable-network host was `MVM_SKIP_HASH_VERIFY=1`, which
    /// disables the supply-chain check entirely.
    ImportImage {
        /// Path to the cosign-signed manifest JSON
        /// (`dev-image-{arch}.manifest.json`).
        #[arg(long, value_name = "FILE")]
        manifest: String,
        /// Path to the manifest's cosign bundle
        /// (`dev-image-{arch}.manifest.json.bundle`).
        #[arg(long, value_name = "FILE")]
        bundle: String,
        /// Path to the kernel binary (`dev-vmlinux-{arch}`).
        #[arg(long, value_name = "FILE")]
        vmlinux: String,
        /// Path to the rootfs (`dev-rootfs-{arch}.ext4`).
        #[arg(long, value_name = "FILE")]
        rootfs: String,
    },
}

/// Error shown on hosts where `mvmctl dev` can't run today (Linux
/// without Apple Container, macOS Intel, macOS pre-26). Points the
/// user at the plan-60 W8 follow-up so they have somewhere to look
/// rather than an opaque "not implemented" message.
fn bail_no_dev_backend() -> Result<()> {
    anyhow::bail!(
        "`mvmctl dev` requires Apple Container (macOS 26+ Apple Silicon).\n\
         Other hosts use the W8 direct-launch dev path, which has not\n\
         shipped yet — see specs/SPRINT.md §\"Up next\". For now, run\n\
         workloads with `mvmctl up <flake>` (which uses the production\n\
         microVM backends) or wait for the W8 follow-up."
    );
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, cfg: &MvmConfig) -> Result<()> {
    let action = args.action.unwrap_or(DevAction::Up {
        cpus: 8,
        memory: 16,
        project: None,
        metrics_port: 0,
        watch_config: false,
        shell: false,
    });

    match action {
        DevAction::Up {
            cpus,
            memory,
            project: _project,
            metrics_port: _metrics_port,
            watch_config: _watch_config,
            shell,
        } => {
            // CLI flag wins; otherwise fall back to per-user config defaults.
            let effective_cpus = if cpus == 8 { cfg.lima_cpus } else { cpus };
            let effective_mem = if memory == 16 {
                cfg.lima_mem_gib
            } else {
                memory
            };

            if mvm_core::platform::current().has_apple_containers() {
                apple_container::cmd_dev_apple_container(effective_cpus, effective_mem, shell)
            } else {
                bail_no_dev_backend()
            }
        }
        DevAction::Down { reset } => {
            let result = if mvm_core::platform::current().has_apple_containers() {
                apple_container::cmd_dev_apple_container_down()
            } else {
                // Nothing to stop on non-Apple-Container hosts (no Lima).
                // The gc-root cleanup below still runs.
                Ok(())
            };
            // Always drop the dev-image GC root on `down`. It exists to
            // pin the rootfs/kernel store paths *while the VM is using
            // them*; once the VM is stopped, holding the root just
            // blocks `nix-collect-garbage` from reclaiming superseded
            // images. The next `dev up` re-resolves the path via
            // `nix build --out-link`, which is a no-op against a fresh
            // closure (cache hit) and a re-realise against a changed
            // closure — either way, the symlink is recreated cleanly.
            let gc_root = format!("{}/dev/current", mvm_core::config::mvm_data_dir());
            if let Err(e) = std::fs::remove_file(&gc_root)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                ui::warn(&format!("Could not remove {gc_root}: {e}"));
            }

            if reset {
                // `--reset` additionally drops the host-backed Nix
                // store overlay disk. Useful for "make `dev up` start
                // from a truly empty store" — e.g. after a corrupting
                // crash, or to reproduce a first-boot scenario.
                // Without this flag, the build cache survives `down`,
                // which is the right default (rebuilds are cheap, the
                // closure isn't).
                let nix_disk = format!("{}/dev/nix-store.img", mvm_core::config::mvm_data_dir());
                match std::fs::remove_file(&nix_disk) {
                    Ok(()) => {
                        ui::info(
                            "Cleared host-backed Nix store; next `dev up` will mkfs a fresh one.",
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        ui::warn(&format!("Could not remove {nix_disk}: {e}"));
                    }
                }
            }
            result
        }
        DevAction::Shell { project: _project } => {
            if mvm_core::platform::current().has_apple_containers() {
                if !apple_container::is_apple_container_dev_running() {
                    anyhow::bail!("Dev VM is not running. Start it with: mvmctl dev up");
                }
                // Try connecting — the VM may be in another process
                match console::console_interactive("mvm-dev") {
                    Ok(()) => Ok(()),
                    Err(_) => anyhow::bail!(
                        "Dev VM is running but owned by another process.\n\
                         Use the terminal where you ran 'mvmctl dev up',\n\
                         or restart with: mvmctl dev down && mvmctl dev up --shell"
                    ),
                }
            } else {
                bail_no_dev_backend()
            }
        }
        DevAction::Status => {
            if mvm_core::platform::current().has_apple_containers() {
                apple_container::cmd_dev_apple_container_status()
            } else {
                ui::info(
                    "Dev environment: not configured on this host (Apple Container unavailable).\n\
                     The W8 direct-launch path will replace this — see specs/SPRINT.md.",
                );
                Ok(())
            }
        }
        DevAction::ImportImage {
            manifest,
            bundle,
            vmlinux,
            rootfs,
        } => apple_container::cmd_dev_import_image(&manifest, &bundle, &vmlinux, &rootfs),
        DevAction::Rebuild {
            cpus,
            memory,
            shell,
        } => {
            // Down
            if mvm_core::platform::current().has_apple_containers() {
                let _ = apple_container::cmd_dev_apple_container_down();
            }

            // Clear cached dev image
            let cache_dir = format!("{}/dev", mvm_core::config::mvm_cache_dir());
            let _ = std::fs::remove_dir_all(&cache_dir);

            // Up
            let effective_cpus = if cpus == 8 { cfg.lima_cpus } else { cpus };
            let effective_mem = if memory == 16 {
                cfg.lima_mem_gib
            } else {
                memory
            };
            if mvm_core::platform::current().has_apple_containers() {
                apple_container::cmd_dev_apple_container(effective_cpus, effective_mem, shell)
            } else {
                bail_no_dev_backend()
            }
        }
    }
}
