//! `mvmctl audit` subcommand handlers.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};

use crate::ui;

use mvm_core::user_config::MvmConfig;
use mvm_supervisor::{SignedEnvelope, verify_audit_chain};

use super::super::vm::audit_chain::{audit_path_for_tenant, default_audit_dir};
use super::super::vm::host_signer;
use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: AuditAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum AuditAction {
    /// Show the last N audit events (default: 20). Reads the legacy
    /// `~/.mvm/log/audit.jsonl` LocalAudit stream; pass `--chain` to
    /// follow the plan-64 chain at `~/.mvm/audit/<tenant>.jsonl`.
    Tail {
        /// Number of lines to show
        #[arg(long, short = 'n', default_value = "20")]
        lines: usize,
        /// Follow log output (poll every 500 ms until Ctrl-C)
        #[arg(long, short = 'f')]
        follow: bool,
        /// Read the plan-64 chain (`~/.mvm/audit/<tenant>.jsonl`) instead
        /// of the legacy LocalAudit log.
        #[arg(long)]
        chain: bool,
        /// Tenant whose chain to tail when `--chain` is set.
        /// Defaults to `"local"` (one-host = one-tenant per ADR-002).
        #[arg(long, default_value = "local")]
        tenant: String,
    },
    /// Verify the plan-64 audit chain. Returns nonzero exit on any
    /// signature or chain-link failure.
    Verify {
        /// Tenant whose chain to verify. Defaults to `"local"`.
        #[arg(long, default_value = "local")]
        tenant: String,
    },
    /// Show every audit chain entry bound to a specific plan_id.
    Show {
        /// The plan_id (UUIDv4) to filter by.
        plan_id: String,
        /// Tenant whose chain to search. Defaults to `"local"`.
        #[arg(long, default_value = "local")]
        tenant: String,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    match args.action {
        AuditAction::Tail {
            lines,
            follow,
            chain,
            tenant,
        } => {
            if chain {
                audit_tail_chain(&tenant, lines, follow)
            } else {
                audit_tail(lines, follow)
            }
        }
        AuditAction::Verify { tenant } => audit_verify(&tenant),
        AuditAction::Show { plan_id, tenant } => audit_show(&tenant, &plan_id),
    }
}

fn audit_tail_chain(tenant: &str, lines: usize, follow: bool) -> Result<()> {
    let dir = default_audit_dir()?;
    let path = audit_path_for_tenant(&dir, tenant);
    if !path.exists() {
        ui::info(&format!(
            "No plan-64 audit chain found for tenant '{tenant}'. \
             Events appear at {} after the next `mvmctl up`.",
            path.display()
        ));
        return Ok(());
    }
    print_last_n_chain_lines(&path, lines)?;
    if !follow {
        return Ok(());
    }
    let mut pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !path.exists() {
            continue;
        }
        let new_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if new_len > pos {
            use std::io::{BufRead, Seek, SeekFrom};
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(pos))?;
            let reader = std::io::BufReader::new(&file);
            for line in reader.lines() {
                let line = line?;
                print_chain_line(&line);
            }
            pos = new_len;
        }
    }
}

fn print_last_n_chain_lines(path: &std::path::Path, n: usize) -> Result<()> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        print_chain_line(line);
    }
    Ok(())
}

fn print_chain_line(line: &str) {
    match serde_json::from_str::<SignedEnvelope>(line) {
        Ok(env) => {
            // Render the inner AuditEntry as a single human-readable
            // line. Operators who want the full envelope still have
            // the raw file at `~/.mvm/audit/<tenant>.jsonl`.
            let labels = if env.entry.labels.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = env
                    .entry
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                format!("  [{}]", pairs.join(" "))
            };
            println!(
                "{ts}  {event}  plan={plan}  workload={workload}{labels}",
                ts = env.entry.timestamp,
                event = env.entry.event,
                plan = env.entry.plan_id.0,
                workload = env.entry.image_name,
            );
        }
        Err(_) => println!("{line}"),
    }
}

fn audit_verify(tenant: &str) -> Result<()> {
    let dir = default_audit_dir()?;
    let path = audit_path_for_tenant(&dir, tenant);
    if !path.exists() {
        ui::info(&format!(
            "No audit chain found for tenant '{tenant}' at {}. Nothing to verify.",
            path.display()
        ));
        return Ok(());
    }
    let signer =
        host_signer::load_or_init().context("loading host signer to verify audit chain")?;
    let vk = signer.verifying;
    match verify_audit_chain(&path, &vk) {
        Ok(count) => {
            ui::success(&format!(
                "audit chain '{}' verifies clean: {count} entries",
                path.display()
            ));
            Ok(())
        }
        Err(e) => {
            // Print a clear error AND propagate so the process exits
            // nonzero. `mvmctl audit verify` is meant for scripting.
            anyhow::bail!("audit chain verify failed: {e}");
        }
    }
}

fn audit_show(tenant: &str, plan_id: &str) -> Result<()> {
    let dir = default_audit_dir()?;
    let path = audit_path_for_tenant(&dir, tenant);
    if !path.exists() {
        ui::info(&format!(
            "No audit chain found for tenant '{tenant}' at {}.",
            path.display()
        ));
        return Ok(());
    }
    use std::io::BufRead;
    let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut matched = 0usize;
    for line in reader.lines() {
        let line = line?;
        if let Ok(env) = serde_json::from_str::<SignedEnvelope>(&line)
            && env.entry.plan_id.0 == plan_id
        {
            print_chain_line(&line);
            matched += 1;
        }
    }
    if matched == 0 {
        ui::info(&format!(
            "No audit entries found for plan_id '{plan_id}' in tenant '{tenant}'."
        ));
    }
    Ok(())
}

fn audit_tail(lines: usize, follow: bool) -> Result<()> {
    let log_path = mvm_core::audit::default_audit_log();
    let path = std::path::Path::new(&log_path);

    if !path.exists() {
        ui::info(&format!(
            "No audit log found. Events are recorded at {log_path}."
        ));
        return Ok(());
    }

    print_last_n_lines(path, lines)?;

    if !follow {
        return Ok(());
    }

    // Tail -f: track file position and poll for new content.
    let mut pos = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !path.exists() {
            continue;
        }
        let new_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if new_len > pos {
            let mut file = std::fs::File::open(path)?;
            use std::io::{BufRead, Seek, SeekFrom};
            file.seek(SeekFrom::Start(pos))?;
            let reader = std::io::BufReader::new(&file);
            for line in reader.lines() {
                let line = line?;
                print_audit_line(&line);
            }
            pos = new_len;
        }
    }
}

fn print_last_n_lines(path: &std::path::Path, n: usize) -> Result<()> {
    use std::io::BufRead;
    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        print_audit_line(line);
    }
    Ok(())
}

fn print_audit_line(line: &str) {
    match serde_json::from_str::<mvm_core::audit::LocalAuditEvent>(line) {
        Ok(event) => {
            let kind = serde_json::to_string(&event.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let vm = event
                .vm_name
                .as_deref()
                .map(|n| format!("  [{n}]"))
                .unwrap_or_default();
            let detail = event
                .detail
                .as_deref()
                .map(|d| format!("  {d}"))
                .unwrap_or_default();
            println!("{ts}  {kind}{vm}{detail}", ts = event.timestamp);
        }
        Err(_) => {
            // Non-local-audit line — print as-is (fleet AuditEntry, etc.)
            println!("{line}");
        }
    }
}
