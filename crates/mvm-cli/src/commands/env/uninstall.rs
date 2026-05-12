//! `mvmctl uninstall` — remove state directories and the mvmctl binary.

use anyhow::Result;
use clap::Args as ClapArgs;

use crate::ui;

use mvm_backend::microvm;
use mvm_core::user_config::MvmConfig;

use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Skip confirmation prompt
    #[arg(long)]
    pub yes: bool,
    /// Also remove ~/.mvm/ and the mvmctl binary
    #[arg(long)]
    pub all: bool,
    /// Print actions without performing them
    #[arg(long)]
    pub dry_run: bool,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    // Build the action plan. Dry-run avoids any external process calls so
    // it stays fast.
    let mut actions: Vec<String> = vec![
        "Stop running microVMs (best-effort)".to_string(),
        "Remove /var/lib/mvm/ (VM state, volumes, run-info)".to_string(),
    ];
    if args.all {
        actions.push("Remove ~/.mvm/ (config, signing keys)".to_string());
        actions.push("Remove /usr/local/bin/mvmctl (binary)".to_string());
    }

    if args.dry_run {
        ui::info("Dry run — the following would be removed:");
        for a in &actions {
            println!("  • {a}");
        }
        return Ok(());
    }

    // Confirmation prompt — also avoids any external calls.
    if !args.yes {
        ui::info("The following will be removed:");
        for a in &actions {
            println!("  • {a}");
        }
        if !ui::confirm("Proceed with uninstall?") {
            ui::info("Cancelled.");
            return Ok(());
        }
    }

    // Stop running microVMs first (best-effort). Plan-60 / ADR-013
    // dropped Lima; there is no Lima VM to destroy.
    if let Err(e) = microvm::stop() {
        tracing::warn!("failed to stop microVMs before uninstall: {e}");
    }

    // Remove /var/lib/mvm/.
    let state_dir = std::path::Path::new("/var/lib/mvm");
    if state_dir.exists() {
        ui::info("Removing /var/lib/mvm/...");
        let status = std::process::Command::new("sudo")
            .args(["rm", "-rf", "/var/lib/mvm"])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!("sudo rm /var/lib/mvm exited with status {s}"),
            Err(e) => tracing::warn!("failed to remove /var/lib/mvm: {e}"),
        }
    }

    if args.all {
        // Remove ~/.mvm/.
        if let Ok(home) = std::env::var("HOME") {
            let config_dir = std::path::PathBuf::from(home).join(".mvm");
            if config_dir.exists() {
                ui::info("Removing ~/.mvm/...");
                if let Err(e) = std::fs::remove_dir_all(&config_dir) {
                    tracing::warn!("failed to remove ~/.mvm/: {e}");
                }
            }
        }

        // Remove /usr/local/bin/mvmctl.
        let bin = std::path::Path::new("/usr/local/bin/mvmctl");
        if bin.exists() {
            ui::info("Removing /usr/local/bin/mvmctl...");
            let status = std::process::Command::new("sudo")
                .args(["rm", "-f", "/usr/local/bin/mvmctl"])
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => tracing::warn!("sudo rm mvmctl exited with status {s}"),
                Err(e) => tracing::warn!("failed to remove /usr/local/bin/mvmctl: {e}"),
            }
        }
    }

    mvm_core::audit::event(mvm_core::audit::LocalAuditKind::Uninstall).emit();
    ui::success("Uninstall complete.");
    Ok(())
}
