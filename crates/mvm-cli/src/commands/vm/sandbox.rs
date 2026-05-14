//! `mvmctl sandbox` — inspect and clean sandbox lifecycle state.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use std::collections::BTreeSet;

use mvm_backend::backend::AnyBackend;
use mvm_core::user_config::MvmConfig;
use mvm_core::vm_backend::VmStatus;

use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: SandboxAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum SandboxAction {
    /// Remove stale sandbox registry entries. Dry-run by default.
    Gc(GcArgs),
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct GcArgs {
    /// Report candidates without mutating registry state. This is the default.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Actually remove stale registry entries.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GcCandidate {
    name: String,
    reason: GcReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcReason {
    ExpiredStopped,
    Stopped,
}

impl GcReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExpiredStopped => "expired-stopped",
            Self::Stopped => "stopped",
        }
    }
}

pub(in crate::commands) fn run(cli: &Cli, args: Args, cfg: &MvmConfig) -> Result<()> {
    match args.action {
        SandboxAction::Gc(a) => run_gc(cli, a, cfg),
    }
}

fn run_gc(_cli: &Cli, args: GcArgs, _cfg: &MvmConfig) -> Result<()> {
    let registry_path = mvm::vm::name_registry::registry_path();
    let mut registry =
        mvm::vm::name_registry::VmNameRegistry::load(&registry_path).with_context(|| {
            format!(
                "Failed to load VM name registry at {}",
                registry_path.display()
            )
        })?;
    let running_names = collect_live_vm_names();
    let now = chrono::Utc::now();
    let candidates = gc_candidates(&registry, &running_names, now);
    let dry_run = !args.apply;

    if dry_run {
        if candidates.is_empty() {
            println!("sandbox gc: no stale sandbox registry entries (dry-run)");
            return Ok(());
        }
        println!(
            "sandbox gc: would remove {} stale registry entr{} (dry-run):",
            candidates.len(),
            if candidates.len() == 1 { "y" } else { "ies" }
        );
        for candidate in &candidates {
            println!("  {} ({})", candidate.name, candidate.reason.as_str());
        }
        println!("re-run with --apply to actually remove registry entries");
        return Ok(());
    }

    for candidate in &candidates {
        registry.deregister(&candidate.name);
    }
    registry.save(&registry_path).with_context(|| {
        format!(
            "Failed to save VM name registry at {}",
            registry_path.display()
        )
    })?;

    println!(
        "sandbox gc: removed {} stale registry entr{}",
        candidates.len(),
        if candidates.len() == 1 { "y" } else { "ies" }
    );
    for candidate in &candidates {
        println!("  {} ({})", candidate.name, candidate.reason.as_str());
    }

    let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
    let detail = if names.len() <= 8 {
        format!("removed={},names=[{}]", names.len(), names.join(","))
    } else {
        format!("removed={}", names.len())
    };
    mvm_core::audit_emit!(SandboxGc, "{detail}");
    Ok(())
}

fn collect_live_vm_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for hypervisor in ["apple-container", "docker", "firecracker"] {
        let backend = AnyBackend::from_hypervisor(hypervisor);
        let Ok(vms) = backend.list() else { continue };
        for vm in vms {
            if matches!(
                vm.status,
                VmStatus::Starting | VmStatus::Running | VmStatus::Paused
            ) {
                names.insert(vm.name);
            }
        }
    }
    names
}

fn gc_candidates(
    registry: &mvm::vm::name_registry::VmNameRegistry,
    live_names: &BTreeSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<GcCandidate> {
    let mut candidates: Vec<GcCandidate> = registry
        .vms
        .iter()
        .filter_map(|(name, reg)| {
            if live_names.contains(name) {
                return None;
            }
            let expired = reg
                .expires_at
                .as_deref()
                .and_then(mvm_core::util::time::parse_iso8601)
                .map(|expires_at| expires_at < now)
                .unwrap_or(false);
            let reason = if expired {
                GcReason::ExpiredStopped
            } else {
                GcReason::Stopped
            };
            Some(GcCandidate {
                name: name.clone(),
                reason,
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use mvm::vm::name_registry::{RegisterParams, VmNameRegistry};

    fn registry_with(name: &str, expires_at: Option<&str>) -> VmNameRegistry {
        let mut registry = VmNameRegistry::default();
        let mut params = RegisterParams::minimal(name, "/tmp/mvm-test-vm", "default");
        params.expires_at = expires_at.map(str::to_string);
        registry
            .register_with_metadata(params)
            .expect("register vm");
        registry
    }

    #[test]
    fn gc_candidates_skip_live_vms() {
        let registry = registry_with("live", None);
        let live_names = BTreeSet::from(["live".to_string()]);
        let candidates = gc_candidates(&registry, &live_names, chrono::Utc::now());
        assert!(candidates.is_empty());
    }

    #[test]
    fn gc_candidates_include_stopped_registry_entries() {
        let registry = registry_with("stopped", None);
        let candidates = gc_candidates(&registry, &BTreeSet::new(), chrono::Utc::now());
        assert_eq!(
            candidates,
            vec![GcCandidate {
                name: "stopped".to_string(),
                reason: GcReason::Stopped,
            }]
        );
    }

    #[test]
    fn gc_candidates_mark_expired_stopped_entries() {
        let registry = registry_with("expired", Some("2000-01-01T00:00:00Z"));
        let candidates = gc_candidates(&registry, &BTreeSet::new(), chrono::Utc::now());
        assert_eq!(
            candidates,
            vec![GcCandidate {
                name: "expired".to_string(),
                reason: GcReason::ExpiredStopped,
            }]
        );
    }
}
