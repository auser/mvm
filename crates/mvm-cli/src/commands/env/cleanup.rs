//! `mvmctl cleanup` — remove old build artifacts and run nix garbage collection.

use anyhow::Result;
use clap::Args as ClapArgs;

use crate::ui;

use mvm::shell;
use mvm_core::user_config::MvmConfig;

use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Number of newest build revisions to keep
    #[arg(long)]
    pub keep: Option<usize>,
    /// Remove all cached build revisions
    #[arg(long)]
    pub all: bool,
    /// Print each cached build path that gets removed
    #[arg(long)]
    pub verbose: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let keep_count = if args.all { 0 } else { args.keep.unwrap_or(5) };

    if !args.all && keep_count == 0 {
        anyhow::bail!("--keep must be greater than 0 (or use --all)");
    }

    // Show disk usage before cleanup. Reads the dev VM if it's up;
    // returns `None` if it isn't (and we keep going — disk reporting
    // is informational, not load-bearing).
    let disk_before = vm_disk_usage_pct();
    if let Some(pct) = disk_before {
        ui::info(&format!("Dev VM disk usage: {}%", pct));
    }

    // Step 1: Clear temp files inside the dev VM. Best-effort — the
    // call discards its Result so an unreachable VM degrades to a
    // silent skip (the host doesn't have its own /tmp blowup to
    // clean; the VM is the only place that fills up).
    ui::info("Clearing temporary files...");
    let _ = shell::run_in_vm("sudo rm -rf /tmp/* /var/tmp/* 2>/dev/null");

    // Step 2: Remove old dev-build subdirs under `~/.mvm/dev/builds/`.
    // Pure host filesystem work — no dev-VM dependency. Earlier
    // versions shelled through `ShellEnvironment`, but the VM only
    // bind-mounted the same host path; the indirection has been
    // dropped along with the rest of the Lima-era plumbing.
    let report = mvm_build::dev_build::cleanup_old_dev_builds(keep_count)?;

    if args.verbose {
        if report.removed_paths.is_empty() {
            ui::info("No cached build paths removed.");
        } else {
            ui::info("Removed cached build paths:");
            for path in &report.removed_paths {
                println!("  {}", path);
            }
        }
    }

    if args.all {
        ui::success(&format!(
            "Removed {} cached build(s).",
            report.removed_count
        ));
    } else {
        ui::success(&format!(
            "Removed {} cached build(s), kept newest {}.",
            report.removed_count, keep_count
        ));
    }

    // Step 3: Garbage-collect unreferenced Nix store paths inside
    // the dev VM. The Nix store lives in the VM, so this step is a
    // no-op when the dev VM isn't running — surface it as info
    // (not a warning) so cleanup doesn't look like it failed.
    if disk_before.is_none() {
        ui::info("Skipping nix-collect-garbage: dev VM is not running.");
    } else {
        ui::info("Running nix-collect-garbage...");
        match shell::run_in_vm_stdout("nix-collect-garbage -d 2>&1 | tail -3") {
            Ok(output) => {
                let trimmed = output.trim();
                if !trimmed.is_empty() {
                    println!("{trimmed}");
                }
            }
            Err(e) => {
                // If GC fails (disk too full for daemon), try clearing the Nix
                // user profile links and retrying once.
                ui::warn(&format!("nix-collect-garbage failed: {e}"));
                ui::info("Retrying after clearing Nix profile generations...");
                let _ = shell::run_in_vm("rm -rf ~/.local/state/nix/profiles/* 2>/dev/null");
                match shell::run_in_vm_stdout("nix-collect-garbage -d 2>&1 | tail -3") {
                    Ok(output) => {
                        let trimmed = output.trim();
                        if !trimmed.is_empty() {
                            println!("{trimmed}");
                        }
                    }
                    Err(e2) => ui::warn(&format!("nix-collect-garbage retry failed: {e2}")),
                }
            }
        }
    }

    // Show disk usage after cleanup.
    let disk_after = vm_disk_usage_pct();
    if let Some(pct) = disk_after {
        let freed_msg = match disk_before {
            Some(before) if before > pct => format!(" (freed {}%)", before - pct),
            _ => String::new(),
        };
        ui::success(&format!("Dev VM disk usage: {}%{}", pct, freed_msg));
    }

    // Plan 60 Phase 4 / Plan 37 §6 — every state-changing CLI verb
    // emits one audit record per attempt, even on no-op. `cleanup`
    // mutates the host's `~/.mvm/dev/builds/` directory (Step 2);
    // the count lands in the audit detail so an operator scanning
    // the log can correlate cache-eviction events with disk-usage
    // dips.
    let removed = report.removed_count;
    mvm_core::audit_emit!(SlotPrune, "source=cleanup removed={removed}");

    Ok(())
}

/// Read the dev VM root filesystem usage percentage.
fn vm_disk_usage_pct() -> Option<u8> {
    let output = shell::run_in_vm_stdout("df --output=pcent / 2>/dev/null | tail -1").ok()?;
    output.trim().trim_end_matches('%').trim().parse().ok()
}
