//! `mvmctl cache` subcommand handlers.

use anyhow::Result;
use clap::{Args as ClapArgs, Subcommand};

use crate::ui;
use mvm_core::user_config::MvmConfig;

use super::Cli;
use super::shared::human_bytes;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: CacheAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum CacheAction {
    /// Remove stale items from the cache directory
    Prune {
        /// Print what would be removed without actually removing anything
        #[arg(long)]
        dry_run: bool,
        /// Also sweep orphaned project builds — built artifacts whose
        /// source `mvm.toml` file is gone from disk. Equivalent to
        /// running `mvmctl manifest prune --orphans`; bundled here so
        /// "clean everything" is one command. ("Builds" is the user-
        /// facing noun for what `mvmctl build` produces; internally
        /// these are slot directories under `~/.mvm/templates/`.)
        #[arg(long)]
        orphan_builds: bool,
        /// Also reap orphaned per-VM helpers — `mvm-libkrun-supervisor`,
        /// `gvproxy`, and console-tail processes that were reparented
        /// to launchd when the parent `mvmctl` was killed mid-run, plus
        /// their `~/.cache/mvm/builder-vm/vms/<id>/` cache directories.
        /// Plan 95 §FU-1. Skips dirs whose PIDs are still children of a
        /// live `mvmctl` (those are in-flight `dev up` runs, not orphans).
        #[arg(long)]
        reap_orphans: bool,
    },
    /// Show cache directory path and disk usage
    Info,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let cache_dir = mvm_core::config::mvm_cache_dir();

    match args.action {
        CacheAction::Info => {
            println!("Cache directory: {cache_dir}");
            let path = std::path::Path::new(&cache_dir);
            if path.exists() {
                let size = dir_size(path);
                println!("Disk usage: {}", human_bytes(size));
            } else {
                println!("(not yet created)");
            }
            Ok(())
        }
        CacheAction::Prune {
            dry_run,
            orphan_builds,
            reap_orphans,
        } => {
            // Plan 95 §FU-1 — reap orphaned per-VM helpers. Done first
            // so subsequent steps see a clean process list and so the
            // sweeper can drop the per-VM cache dirs along with the
            // helpers that were holding their sockets/PIDs.
            if reap_orphans {
                match super::super::env::apple_container::reap_orphaned_vm_helpers(dry_run) {
                    Ok(o) => {
                        if o.killed == 0 && o.removed_dirs == 0 {
                            ui::info("No orphaned VM helpers.");
                        } else if dry_run {
                            ui::info(&format!(
                                "(dry-run) Would reap {} orphaned helper PID(s) and {} cache dir(s) ({}).",
                                o.killed,
                                o.removed_dirs,
                                human_bytes(o.freed_bytes)
                            ));
                        } else {
                            ui::success(&format!(
                                "Reaped {} orphaned helper PID(s) and {} cache dir(s), freed {}.",
                                o.killed,
                                o.removed_dirs,
                                human_bytes(o.freed_bytes)
                            ));
                        }
                    }
                    Err(e) => {
                        ui::warn(&format!("Orphan-helper reap failed: {e:#}"));
                    }
                }
            }

            // Optionally sweep orphaned builds first. Same logic as
            // `mvmctl manifest prune --orphans` — bundled here so the
            // user can do a single clean-everything pass without
            // remembering both verbs.
            if orphan_builds {
                if dry_run {
                    ui::info(
                        "(dry-run) Would scan for orphaned builds — see `mvmctl manifest prune --orphans --dry-run` for details.",
                    );
                } else {
                    match mvm::vm::template::lifecycle::template_prune_orphan_slots() {
                        Ok((count, _)) => {
                            mvm_core::audit_emit!(SlotPrune, "source=cache_prune count={count}");
                            if count > 0 {
                                ui::success(&format!("Pruned {count} orphaned build(s)."));
                            } else {
                                ui::info("No orphaned builds.");
                            }
                        }
                        Err(e) => {
                            ui::warn(&format!("Orphan-build prune failed: {e}"));
                        }
                    }
                }
            }

            let path = std::path::Path::new(&cache_dir);
            if !path.exists() {
                ui::info("Cache directory does not exist. Nothing to prune.");
                if !dry_run {
                    mvm_core::audit_emit!(CachePrune, "removed=0 freed_bytes=0 cache_dir=missing");
                }
                return Ok(());
            }

            // Prune: remove empty subdirectories and temp files
            let mut removed = 0u64;
            let mut freed = 0u64;

            // Plan 77 W2: sweep orphaned Stage 0 staging dirs first.
            // They live under `~/.cache/mvm/builder-vm/.<arch>.stage0-*`
            // (or the legacy `<arch>-staging` shape) and are left
            // behind by crashed `mvmctl dev up` invocations. The sweep
            // takes the Stage 0 advisory lock to avoid racing a live
            // bootstrap; if the lock is held it skips silently and we
            // proceed with the temp-file sweep.
            match super::super::env::apple_container::sweep_orphaned_stage0_staging_dirs(dry_run) {
                Ok(super::super::env::apple_container::Stage0SweepOutcome::Swept {
                    removed: r,
                    freed_bytes,
                }) => {
                    removed += r;
                    freed += freed_bytes;
                }
                Ok(super::super::env::apple_container::Stage0SweepOutcome::SkippedLockHeld) => {
                    ui::info(
                        "Stage 0 builder VM bootstrap appears to be running on this host; \
                         skipping orphan staging cleanup.",
                    );
                }
                Err(e) => {
                    ui::warn(&format!("Stage 0 staging sweep failed: {e:#}"));
                }
            }

            for entry in walkdir(path)? {
                let entry_path = entry.path();
                // Remove temp files (mvm-lima-*, .tmp)
                if let Some(name) = entry_path.file_name().and_then(|n| n.to_str())
                    && (name.starts_with("mvm-lima-") || name.ends_with(".tmp"))
                {
                    let size = entry_path.metadata().map(|m| m.len()).unwrap_or(0);
                    if dry_run {
                        println!(
                            "Would remove: {} ({})",
                            entry_path.display(),
                            human_bytes(size)
                        );
                    } else if entry_path.is_dir() {
                        let _ = std::fs::remove_dir_all(entry_path);
                    } else {
                        let _ = std::fs::remove_file(entry_path);
                    }
                    removed += 1;
                    freed += size;
                }
            }

            if removed == 0 {
                ui::info("Nothing to prune.");
            } else if dry_run {
                ui::info(&format!(
                    "Would remove {} items, freeing {}",
                    removed,
                    human_bytes(freed)
                ));
            } else {
                ui::success(&format!(
                    "Pruned {} items, freed {}",
                    removed,
                    human_bytes(freed)
                ));
            }
            // Plan 37 §6: every state-changing CLI verb emits one
            // audit record. We only mutate disk on the non-dry-run
            // path; dry-run reads only and stays out of the log.
            if !dry_run {
                mvm_core::audit_emit!(CachePrune, "removed={removed} freed_bytes={freed}");
            }
            Ok(())
        }
    }
}

/// Recursively calculate directory size in bytes.
fn dir_size(path: &std::path::Path) -> u64 {
    walkdir(path)
        .unwrap_or_default()
        .iter()
        .filter(|e| e.path().is_file())
        .map(|e| e.path().metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Simple recursive directory walker.
fn walkdir(path: &std::path::Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let epath = entry.path();
            let is_dir = epath.is_dir();
            entries.push(entry);
            if is_dir && let Ok(sub) = walkdir(&epath) {
                entries.extend(sub);
            }
        }
    }
    Ok(entries)
}
