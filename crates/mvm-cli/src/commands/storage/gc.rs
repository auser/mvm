//! `mvmctl storage gc` — reclaim unreferenced thin volumes.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;
use std::sync::Arc;

use super::Cli;
use mvm_core::config::ensure_data_dir;
use mvm_core::policy::audit::{LocalAuditEvent, LocalAuditKind, LocalAuditLog};
use mvm_core::user_config::MvmConfig;
use mvm_runtime::storage::{
    Backend, DmsetupBackend, MockBackend, PoolConfig, ThinPool, ThinPoolImpl,
};

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Actually remove orphaned volumes. Without this flag, the
    /// command only reports what it would remove.
    #[arg(long)]
    pub apply: bool,
    /// Use the in-memory mock backend (for dev/macOS hosts).
    #[arg(long)]
    pub mock: bool,
    /// Comma-separated list of volume name prefixes to consider
    /// "live" (not removable). Anything not matching is candidate
    /// for removal. Default: empty (treat all volumes as candidates;
    /// dry-run only reports them).
    #[arg(long, value_delimiter = ',', default_values_t = Vec::<String>::new())]
    pub keep_prefix: Vec<String>,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let backend: Arc<dyn Backend> = if args.mock {
        Arc::new(MockBackend::new())
    } else {
        Arc::new(DmsetupBackend::new())
    };
    let pool = ThinPoolImpl::new(PoolConfig::default(), backend);

    let volumes = match pool.list_volumes() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mvmctl storage gc: pool unavailable: {e}");
            return Ok(());
        }
    };

    let mut candidates: Vec<String> = Vec::new();
    for name in volumes {
        if !args.keep_prefix.iter().any(|p| name.starts_with(p)) {
            candidates.push(name);
        }
    }

    if !args.apply {
        if candidates.is_empty() {
            println!("storage gc: no orphan volumes (dry-run)");
        } else {
            println!(
                "storage gc: would remove {} volume(s) (dry-run):",
                candidates.len()
            );
            for name in &candidates {
                println!("  {name}");
            }
            println!("re-run with --apply to actually remove");
        }
        return Ok(());
    }

    if candidates.is_empty() {
        println!("storage gc: no orphan volumes");
        return Ok(());
    }

    let removed = pool
        .gc(|name| !args.keep_prefix.iter().any(|p| name.starts_with(p)))
        .context("dmsetup gc failed")?;

    println!("storage gc: removed {} volume(s)", removed.len());
    for name in &removed {
        println!("  {name}");
    }

    let data_dir = PathBuf::from(ensure_data_dir().context("ensure ~/.mvm exists")?);
    let log_path = data_dir.join("audit.log");
    if let Ok(log) = LocalAuditLog::open(&log_path) {
        let detail = if removed.len() <= 8 {
            format!("removed=[{}]", removed.join(","))
        } else {
            format!("count={}", removed.len())
        };
        let _ = log.append(&LocalAuditEvent::now(
            LocalAuditKind::StorageGc,
            None,
            Some(detail),
        ));
    }

    Ok(())
}
