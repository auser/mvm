use anyhow::{Context, Result};
use std::process::{Command, Output, Stdio};

use crate::config::VM_NAME;

/// Run a command on the host, capturing output.
pub fn run_host(cmd: &str, args: &[&str]) -> Result<Output> {
    Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run: {} {}", cmd, args.join(" ")))
}

/// Run a command on the host, inheriting stdio (visible to user).
pub fn run_host_visible(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to run: {} {}", cmd, args.join(" ")))?;

    if !status.success() {
        anyhow::bail!(
            "Command failed (exit {}): {} {}",
            status.code().unwrap_or(-1),
            cmd,
            args.join(" ")
        );
    }
    Ok(())
}

/// Run a bash script inside the Lima VM, capturing output.
pub fn run_in_vm(script: &str) -> Result<Output> {
    Command::new("limactl")
        .args(["shell", VM_NAME, "bash", "-c", script])
        .output()
        .with_context(|| "Failed to run command in Lima VM")
}

/// Run a bash script inside the Lima VM, with output visible to user.
pub fn run_in_vm_visible(script: &str) -> Result<()> {
    let status = Command::new("limactl")
        .args(["shell", VM_NAME, "bash", "-c", script])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| "Failed to run command in Lima VM")?;

    if !status.success() {
        anyhow::bail!(
            "Command failed in Lima VM (exit {})",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Run a bash script inside the Lima VM, returning stdout as String.
pub fn run_in_vm_stdout(script: &str) -> Result<String> {
    let output = run_in_vm(script)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Replace the current process with an interactive command (for SSH/TTY).
/// Uses Unix exec() — the Rust process is fully replaced, no return on success.
/// Note: This is safe because all arguments are passed as an array, not via shell interpolation.
#[cfg(unix)]
pub fn replace_process(cmd: &str, args: &[&str]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let err = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .exec();

    // exec() only returns on error
    Err(err).with_context(|| format!("Failed to exec: {} {}", cmd, args.join(" ")))
}
