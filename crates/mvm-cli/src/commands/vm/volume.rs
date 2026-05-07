//! `mvmctl volume` — virtio-fs volume mount lifecycle. Plan 45 §D5
//! (Path C; renamed from the prior `share` subcommand without
//! behavioural change).
//!
//! Today this lands the **registry-side** plumbing: registering an
//! attached volume mount in `~/.mvm/instances/<vm>/volume_mounts.json`,
//! rejecting mount paths that hit the deny-list, and listing /
//! removing entries. The actual `virtiofsd`-on-host + Firecracker
//! virtio-device-attach is a follow-up — the substrate routes
//! through `mvm_security::policy::MountPathPolicy` and emits the
//! same `MountVolume` / `UnmountVolume` vsock verbs the agent
//! handler already serves.
//!
//! ## Per-host volume catalog (`volume create`)
//!
//! Plan 45 also calls for a per-host volume catalog (`mvmctl volume
//! create <name>` against `~/.mvm/volumes/registry.json`). That's a
//! separate primitive landing in a follow-up — this subcommand
//! currently mirrors the prior `share` shape.
//!
//! ## `--remote` mode (mvmd proxy)
//!
//! Per plan 45 §D5 (Path C), `--remote` routes operations through
//! mvmd's REST API rather than executing locally. v1 stub only —
//! the actual `mvmctl::mvmd_client` module ships in a follow-up
//! once the mvmd-side bucket reconciliation lands (mvmd Sprint 137
//! W2). Today `--remote` returns a clear "not yet implemented"
//! error.

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};

use mvm_core::naming::validate_vm_name;
use mvm_core::user_config::MvmConfig;
use mvm_runtime::vm::volume_registry::{VolumeMountEntry, VolumeMountRegistry};
use mvm_security::policy::validate_mount_path;

use super::Cli;
use super::shared::clap_vm_name;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub command: VolumeCmd,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum VolumeCmd {
    /// Mount a virtio-fs volume into a VM.
    ///
    /// Per plan 45 §D5 (Path C): operations against provider-backed
    /// (S3 / Hetzner / R2 / GCS / Azure) volumes route through mvmd
    /// via `--remote`. v1 mvm-side `mount` handles only local
    /// volumes (host directory exposed via virtio-fs).
    Mount {
        /// Name of the VM
        #[arg(value_parser = clap_vm_name)]
        name: String,
        /// Logical volume name (used as the virtio-fs tag).
        /// Must be lowercase alphanumeric + hyphens, ≤32 chars.
        #[arg(long)]
        volume: String,
        /// Absolute host directory exposed via virtio-fs.
        #[arg(long)]
        host: String,
        /// Mount point inside the VM (must be under /mnt, /data,
        /// or /work; never under /etc, /usr, /lib, /proc, /nix,
        /// etc.)
        #[arg(long)]
        guest: String,
        /// Mount the volume read-write (default: read-only).
        #[arg(long)]
        rw: bool,
        /// Route through mvmd REST instead of writing the local
        /// registry. Stub in v1 — see plan 45 §D5.
        #[arg(long)]
        remote: bool,
    },
    /// List registered volume mounts for a VM.
    Ls {
        #[arg(value_parser = clap_vm_name)]
        name: String,
        #[arg(long)]
        json: bool,
        /// Route through mvmd REST instead of reading the local
        /// registry. Stub in v1 — see plan 45 §D5.
        #[arg(long)]
        remote: bool,
    },
    /// Unmount a registered volume.
    Unmount {
        #[arg(value_parser = clap_vm_name)]
        name: String,
        /// Guest mount path to detach.
        guest_path: String,
        /// Route through mvmd REST instead of editing the local
        /// registry. Stub in v1 — see plan 45 §D5.
        #[arg(long)]
        remote: bool,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.command {
        VolumeCmd::Mount {
            name,
            volume,
            host,
            guest,
            rw,
            remote,
        } => {
            if remote {
                return remote_stub("volume mount");
            }
            mount(&name, &volume, &host, &guest, rw)
        }
        VolumeCmd::Ls { name, json, remote } => {
            if remote {
                return remote_stub("volume ls");
            }
            ls(&name, json)
        }
        VolumeCmd::Unmount {
            name,
            guest_path,
            remote,
        } => {
            if remote {
                return remote_stub("volume unmount");
            }
            unmount(&name, &guest_path)
        }
    }
}

fn remote_stub(op: &str) -> Result<()> {
    bail!(
        "{op} --remote not yet implemented; tracking in mvmd Sprint 137 W2 \
         (companion to mvm Plan 45 §D5). Use the local volume registry for now."
    )
}

fn validate_volume_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        bail!(
            "volume name length {} outside [1, 32] (used as virtio-fs tag)",
            name.len()
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("volume name {name:?} must be lowercase alphanumeric + hyphens");
    }
    if name.starts_with('-') {
        bail!("volume name {name:?} must not start with a hyphen");
    }
    Ok(())
}

fn mount(vm_name: &str, volume_name: &str, host: &str, guest: &str, rw: bool) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;
    validate_volume_name(volume_name)
        .with_context(|| format!("Invalid volume name: {:?}", volume_name))?;

    // Host path must be absolute and exist on disk; otherwise
    // virtiofsd would fail later with a confusing message.
    if !std::path::Path::new(host).is_absolute() {
        bail!("--host path must be absolute, got {:?}", host);
    }
    if !std::path::Path::new(host).is_dir() {
        bail!("--host path {:?} is not an existing directory", host);
    }

    // Validate the guest-side path against the mount policy
    // before we touch the registry — same check the agent runs.
    let canonical_guest = validate_mount_path(guest)
        .with_context(|| format!("guest path {:?} rejected by policy", guest))?;

    let mut registry = VolumeMountRegistry::load(vm_name)?;
    registry.add(VolumeMountEntry {
        volume_name: volume_name.to_string(),
        host_path: host.to_string(),
        guest_path: canonical_guest.clone(),
        read_only: !rw,
        attached_at: mvm_core::util::time::utc_now(),
    })?;
    registry.save(vm_name)?;

    println!(
        "{vm_name}: registered volume {volume_name:?} → {canonical_guest} (host={host}, ro={})",
        !rw
    );
    eprintln!(
        "note: virtiofsd-on-host + Firecracker virtio-device-attach are a follow-up; \
         the registry entry + agent MountVolume verb are ready."
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::VmVolumeAdd,
        Some(vm_name),
        Some(&format!(
            "volume={volume_name} host={host} guest={canonical_guest} ro={}",
            !rw
        )),
    );
    Ok(())
}

fn ls(vm_name: &str, json: bool) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;
    let registry = VolumeMountRegistry::load(vm_name)?;
    if json {
        let rows: Vec<&VolumeMountEntry> = registry.iter().map(|(_, v)| v).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if registry.is_empty() {
        println!("(no volume mounts)");
        return Ok(());
    }
    println!(
        "{:<22} {:<22} {:<14} {:<4} HOST",
        "GUEST", "VOLUME", "ATTACHED", "RO"
    );
    for (_, e) in registry.iter() {
        println!(
            "{:<22} {:<22} {:<14} {:<4} {}",
            e.guest_path,
            e.volume_name,
            &e.attached_at[..e.attached_at.len().min(14)],
            if e.read_only { "yes" } else { "no" },
            e.host_path,
        );
    }
    Ok(())
}

fn unmount(vm_name: &str, guest_path: &str) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;
    let mut registry = VolumeMountRegistry::load(vm_name)?;
    let dropped = registry
        .remove(guest_path)
        .with_context(|| format!("VM {:?} has no volume mount at {:?}", vm_name, guest_path))?;
    registry.save(vm_name)?;
    println!(
        "{vm_name}: unmounted volume {} from {} (host={})",
        dropped.volume_name, dropped.guest_path, dropped.host_path
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::VmVolumeRemove,
        Some(vm_name),
        Some(&format!(
            "volume={} guest={}",
            dropped.volume_name, dropped.guest_path
        )),
    );
    Ok(())
}
