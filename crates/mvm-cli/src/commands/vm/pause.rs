//! `mvmctl pause <vm>` / `mvmctl resume <vm>` — instance snapshot
//! lifecycle. W1 / A4 of the e2b parity plan.
//!
//! `pause` quiesces the running VM, asks Firecracker to write
//! `vmstate.bin` + `mem.bin` to `~/.mvm/instances/<vm>/snapshot/`,
//! seals the W4 HMAC envelope (now epoch-bound — G5), and flips
//! `paused = true` in the persistent VM-name registry.
//!
//! `resume` verifies the envelope (refusing replayed older
//! snapshots), asks Firecracker to load the bytes back, resumes
//! vCPUs, and clears the `paused` flag.
//!
//! Both verbs hit the live Firecracker socket — calls against a
//! VM that's already gone fail cleanly at the socket-existence
//! check rather than mid-API.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::PathBuf;

use mvm_core::naming::validate_vm_name;
use mvm_core::user_config::MvmConfig;
use mvm_runtime::vm::instance_snapshot::{FirecrackerIO, pause_and_seal, verify_and_resume};

use super::Cli;
use super::shared::clap_vm_name;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct PauseArgs {
    /// Name of the VM to pause
    #[arg(value_parser = clap_vm_name)]
    pub name: String,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct ResumeArgs {
    /// Name of the VM to resume
    #[arg(value_parser = clap_vm_name)]
    pub name: String,
}

pub(in crate::commands) fn run_pause(_cli: &Cli, args: PauseArgs, _cfg: &MvmConfig) -> Result<()> {
    validate_vm_name(&args.name).with_context(|| format!("Invalid VM name: {:?}", args.name))?;
    let vm_dir = mvm_runtime::vm::microvm::resolve_running_vm_dir(&args.name)
        .with_context(|| format!("VM {:?} is not running", args.name))?;
    let socket = firecracker_socket(&vm_dir);
    let io = FirecrackerIO::new(socket);

    let sidecar =
        pause_and_seal(&args.name, &io).with_context(|| format!("pausing VM {:?}", args.name))?;

    let registry_path = mvm_runtime::vm::name_registry::registry_path();
    if let Ok(mut registry) = mvm_runtime::vm::name_registry::VmNameRegistry::load(&registry_path) {
        let _ = registry.set_paused(&args.name, true);
        let _ = registry.save(&registry_path);
    }
    println!(
        "{}: paused (epoch {}, vmstate {} B, mem {} B)",
        args.name, sidecar.epoch, sidecar.vmstate_len, sidecar.mem_len
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::WorkloadSleep,
        Some(&args.name),
        Some(&format!(
            "epoch={} vmstate={} mem={}",
            sidecar.epoch, sidecar.vmstate_len, sidecar.mem_len
        )),
    );
    Ok(())
}

pub(in crate::commands) fn run_resume(
    _cli: &Cli,
    args: ResumeArgs,
    _cfg: &MvmConfig,
) -> Result<()> {
    validate_vm_name(&args.name).with_context(|| format!("Invalid VM name: {:?}", args.name))?;
    // For resume the VM may not yet be running — the snapshot
    // restore path is what brings it back. We still need a
    // Firecracker socket the orchestrator can talk to. v1
    // requires the user to have already started a fresh VM
    // shell that's waiting for the snapshot load (Firecracker's
    // restore-into-empty-VMM workflow). The substrate is
    // ready; the launcher integration is a follow-up.
    let vm_dir =
        mvm_runtime::vm::microvm::resolve_running_vm_dir(&args.name).with_context(|| {
            format!(
                "VM {:?} has no running Firecracker shell; resume currently \
                 requires a fresh `mvmctl up --resume-from-snapshot` (follow-up). \
                 Substrate verifies, the launcher integration is pending.",
                args.name
            )
        })?;
    let socket = firecracker_socket(&vm_dir);
    let io = FirecrackerIO::new(socket);

    let sidecar = verify_and_resume(&args.name, &io)
        .with_context(|| format!("resuming VM {:?}", args.name))?;

    let registry_path = mvm_runtime::vm::name_registry::registry_path();
    if let Ok(mut registry) = mvm_runtime::vm::name_registry::VmNameRegistry::load(&registry_path) {
        let _ = registry.set_paused(&args.name, false);
        let _ = registry.save(&registry_path);
    }
    println!(
        "{}: resumed (epoch {}, vmstate {} B, mem {} B)",
        args.name, sidecar.epoch, sidecar.vmstate_len, sidecar.mem_len
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::WorkloadWake,
        Some(&args.name),
        Some(&format!(
            "epoch={} vmstate={} mem={}",
            sidecar.epoch, sidecar.vmstate_len, sidecar.mem_len
        )),
    );
    Ok(())
}

fn firecracker_socket(vm_dir: &str) -> PathBuf {
    PathBuf::from(format!("{vm_dir}/runtime/firecracker.socket"))
}

// `mvmctl snapshot ls / rm` lives next to pause/resume because
// they share `instance_snapshot` plumbing — keeping them in one
// file avoids a third tiny module.
#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct SnapshotArgs {
    #[command(subcommand)]
    pub command: SnapshotCmd,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub(in crate::commands) enum SnapshotCmd {
    /// List sealed instance snapshots under ~/.mvm/instances/*/snapshot/
    Ls {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove a sealed instance snapshot
    Rm {
        /// VM name whose snapshot to remove
        #[arg(value_parser = clap_vm_name)]
        name: String,
    },
}

pub(in crate::commands) fn run_snapshot(
    _cli: &Cli,
    args: SnapshotArgs,
    _cfg: &MvmConfig,
) -> Result<()> {
    match args.command {
        SnapshotCmd::Ls { json } => snap_ls(json),
        SnapshotCmd::Rm { name } => snap_rm(&name),
    }
}

fn snap_ls(json: bool) -> Result<()> {
    let entries = mvm_runtime::vm::instance_snapshot::list_instance_snapshots()?;
    if json {
        #[derive(serde::Serialize)]
        struct Row<'a> {
            vm_name: &'a str,
            vmstate_size_bytes: u64,
            mem_size_bytes: u64,
            epoch: Option<u64>,
            sealed: bool,
        }
        let rows: Vec<Row<'_>> = entries
            .iter()
            .map(|e| Row {
                vm_name: &e.vm_name,
                vmstate_size_bytes: e.vmstate_size_bytes,
                mem_size_bytes: e.mem_size_bytes,
                epoch: e.sidecar.as_ref().map(|s| s.epoch),
                sealed: e.sidecar.is_some(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no instance snapshots)");
        return Ok(());
    }
    println!(
        "{:<24} {:<7} {:<14} {:<14} STATUS",
        "VM", "EPOCH", "VMSTATE", "MEM"
    );
    for e in &entries {
        let (epoch, status) = match &e.sidecar {
            Some(s) => (s.epoch.to_string(), "sealed"),
            None => ("-".to_string(), "unsealed"),
        };
        println!(
            "{:<24} {:<7} {:<14} {:<14} {}",
            e.vm_name, epoch, e.vmstate_size_bytes, e.mem_size_bytes, status
        );
    }
    Ok(())
}

fn snap_rm(name: &str) -> Result<()> {
    validate_vm_name(name).with_context(|| format!("Invalid VM name: {:?}", name))?;
    let removed = mvm_runtime::vm::instance_snapshot::delete_instance_snapshot(name)?;
    if !removed {
        bail!("no snapshot found for VM {:?}", name);
    }
    let registry_path = mvm_runtime::vm::name_registry::registry_path();
    if let Ok(mut registry) = mvm_runtime::vm::name_registry::VmNameRegistry::load(&registry_path) {
        let _ = registry.set_paused(name, false);
        let _ = registry.save(&registry_path);
    }
    println!("{}: snapshot removed", name);
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::SnapshotDelete,
        Some(name),
        None,
    );
    Ok(())
}
