use anyhow::{Context, Result};

use crate::config::VM_NAME;
use crate::shell::{run_host, run_host_visible};

#[derive(Debug, PartialEq)]
pub enum LimaStatus {
    Running,
    Stopped,
    NotFound,
}

/// Get the current status of the Lima VM.
pub fn get_status() -> Result<LimaStatus> {
    let output = run_host("limactl", &["list", "--format", "{{.Status}}", VM_NAME])?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if !output.status.success() || stdout.is_empty() {
        return Ok(LimaStatus::NotFound);
    }

    match stdout.as_str() {
        "Running" => Ok(LimaStatus::Running),
        "Stopped" => Ok(LimaStatus::Stopped),
        _ => Ok(LimaStatus::NotFound),
    }
}

/// Create and start a new Lima VM from the given yaml config.
pub fn create(lima_yaml: &std::path::Path) -> Result<()> {
    let yaml_str = lima_yaml.to_str().context("Invalid lima.yaml path")?;
    run_host_visible("limactl", &["start", "--name", VM_NAME, yaml_str])
}

/// Start an existing stopped Lima VM.
pub fn start() -> Result<()> {
    run_host_visible("limactl", &["start", VM_NAME])
}

/// Ensure the Lima VM is running. Creates, starts, or does nothing as needed.
pub fn ensure_running(lima_yaml: &std::path::Path) -> Result<()> {
    match get_status()? {
        LimaStatus::Running => {
            println!("[mvm] Lima VM '{}' is running.", VM_NAME);
            Ok(())
        }
        LimaStatus::Stopped => {
            println!("[mvm] Starting Lima VM '{}'...", VM_NAME);
            start()
        }
        LimaStatus::NotFound => {
            println!("[mvm] Creating Lima VM '{}'...", VM_NAME);
            create(lima_yaml)
        }
    }
}

/// Require that the Lima VM is currently running.
pub fn require_running() -> Result<()> {
    match get_status()? {
        LimaStatus::Running => Ok(()),
        LimaStatus::Stopped => {
            anyhow::bail!(
                "Lima VM '{}' is stopped. Run 'mvm start' or 'mvm setup'.",
                VM_NAME
            )
        }
        LimaStatus::NotFound => {
            anyhow::bail!(
                "Lima VM '{}' does not exist. Run 'mvm setup' first.",
                VM_NAME
            )
        }
    }
}

/// Delete the Lima VM forcefully.
pub fn destroy() -> Result<()> {
    run_host_visible("limactl", &["delete", "--force", VM_NAME])
}
