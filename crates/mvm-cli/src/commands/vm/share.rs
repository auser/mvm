//! `mvmctl share` — virtio-fs share lifecycle. W1 / D of the
//! e2b parity plan.
//!
//! Today this lands the **registry-side** plumbing: registering an
//! attached share in `~/.mvm/instances/<vm>/shares.json`, rejecting
//! mount paths that hit the deny-list, and listing / removing
//! entries. The actual `virtiofsd`-on-host + Firecracker
//! virtio-device-attach is a follow-up — the substrate routes
//! through `mvm_security::policy::MountPathPolicy` and emits the
//! same `MountShare` / `UnmountShare` vsock verbs the agent
//! handler already serves.

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};

use mvm_core::naming::validate_vm_name;
use mvm_core::user_config::MvmConfig;
use mvm_runtime::vm::share_registry::{ShareEntry, ShareRegistry};
use mvm_security::policy::validate_mount_path;

use super::Cli;
use super::shared::clap_vm_name;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub command: ShareCmd,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum ShareCmd {
    /// Register a virtio-fs share against a VM
    Add {
        /// Name of the VM
        #[arg(value_parser = clap_vm_name)]
        name: String,
        /// Absolute host directory exposed via virtio-fs
        #[arg(long)]
        host: String,
        /// Mount point inside the VM (must be under /mnt, /data,
        /// or /work; never under /etc, /usr, /lib, /proc, etc.)
        #[arg(long)]
        guest: String,
        /// Mount the share read-write (default: read-only)
        #[arg(long)]
        rw: bool,
        /// virtio-fs tag (auto-generated when omitted)
        #[arg(long)]
        tag: Option<String>,
    },
    /// List registered shares for a VM
    Ls {
        #[arg(value_parser = clap_vm_name)]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered share
    Rm {
        #[arg(value_parser = clap_vm_name)]
        name: String,
        /// Guest mount path to detach
        guest_path: String,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.command {
        ShareCmd::Add {
            name,
            host,
            guest,
            rw,
            tag,
        } => add(&name, &host, &guest, rw, tag.as_deref()),
        ShareCmd::Ls { name, json } => ls(&name, json),
        ShareCmd::Rm { name, guest_path } => rm(&name, &guest_path),
    }
}

fn auto_tag(vm_name: &str, guest_path: &str) -> String {
    // Deterministic, kernel-safe (lowercase alphanumeric + `-`),
    // capped at 32 chars per the agent's `validate_tag` rule.
    let mut sanitized = String::with_capacity(32);
    sanitized.push_str("share-");
    for c in vm_name.chars().chain(['-']).chain(guest_path.chars()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            sanitized.push(c);
        } else {
            sanitized.push('-');
        }
        if sanitized.len() >= 32 {
            break;
        }
    }
    sanitized.truncate(32);
    sanitized
}

fn add(vm_name: &str, host: &str, guest: &str, rw: bool, tag: Option<&str>) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;

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

    let tag_owned = match tag {
        Some(t) => t.to_string(),
        None => auto_tag(vm_name, &canonical_guest),
    };

    let mut registry = ShareRegistry::load(vm_name)?;
    registry.add(ShareEntry {
        host_path: host.to_string(),
        guest_path: canonical_guest.clone(),
        tag: tag_owned.clone(),
        read_only: !rw,
        attached_at: mvm_core::util::time::utc_now(),
    })?;
    registry.save(vm_name)?;

    println!(
        "{vm_name}: registered share {host:?} → {canonical_guest} (tag={tag_owned}, ro={})",
        !rw
    );
    eprintln!(
        "note: virtiofsd-on-host + Firecracker virtio-device-attach are a follow-up; \
         the registry entry + agent MountShare verb are ready."
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::VmShareAdd,
        Some(vm_name),
        Some(&format!(
            "host={host} guest={canonical_guest} tag={tag_owned} ro={}",
            !rw
        )),
    );
    Ok(())
}

fn ls(vm_name: &str, json: bool) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;
    let registry = ShareRegistry::load(vm_name)?;
    if json {
        let rows: Vec<&ShareEntry> = registry.iter().map(|(_, v)| v).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if registry.is_empty() {
        println!("(no shares)");
        return Ok(());
    }
    println!(
        "{:<22} {:<22} {:<14} {:<4} HOST",
        "GUEST", "TAG", "ATTACHED", "RO"
    );
    for (_, e) in registry.iter() {
        println!(
            "{:<22} {:<22} {:<14} {:<4} {}",
            e.guest_path,
            e.tag,
            &e.attached_at[..e.attached_at.len().min(14)],
            if e.read_only { "yes" } else { "no" },
            e.host_path,
        );
    }
    Ok(())
}

fn rm(vm_name: &str, guest_path: &str) -> Result<()> {
    validate_vm_name(vm_name).with_context(|| format!("Invalid VM name: {:?}", vm_name))?;
    let mut registry = ShareRegistry::load(vm_name)?;
    let dropped = registry
        .remove(guest_path)
        .with_context(|| format!("VM {:?} has no share at {:?}", vm_name, guest_path))?;
    registry.save(vm_name)?;
    println!(
        "{vm_name}: removed share {} (tag={})",
        dropped.guest_path, dropped.tag
    );
    mvm_core::audit::emit(
        mvm_core::audit::LocalAuditKind::VmShareRemove,
        Some(vm_name),
        Some(&format!("guest={} tag={}", dropped.guest_path, dropped.tag)),
    );
    Ok(())
}
