use anyhow::{Context, Result};
use std::process::{Command, Output, Stdio};

use super::config::VM_NAME;
use super::platform;

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

/// Run a bash script in the Linux execution environment, capturing output.
///
/// On native Linux with KVM: runs `bash -c` directly on the host.
/// On macOS or Linux without KVM: runs via `limactl shell` inside a Lima VM.
pub fn run_on_vm(vm_name: &str, script: &str) -> Result<Output> {
    #[cfg(test)]
    if let Some(output) = super::shell_mock::intercept(script) {
        return Ok(output);
    }

    if platform::current().needs_lima() {
        Command::new("limactl")
            .args(["shell", vm_name, "bash", "-c", script])
            .output()
            .with_context(|| format!("Failed to run command in Lima VM '{}'", vm_name))
    } else {
        Command::new("bash")
            .args(["-c", script])
            .output()
            .with_context(|| "Failed to run command on host")
    }
}

/// Run a bash script in the Linux execution environment, with output visible to user.
pub fn run_on_vm_visible(vm_name: &str, script: &str) -> Result<()> {
    let status = if platform::current().needs_lima() {
        Command::new("limactl")
            .args(["shell", vm_name, "bash", "-c", script])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("Failed to run command in Lima VM '{}'", vm_name))?
    } else {
        Command::new("bash")
            .args(["-c", script])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| "Failed to run command on host")?
    };

    if !status.success() {
        if platform::current().needs_lima() {
            anyhow::bail!(
                "Command failed in Lima VM '{}' (exit {})",
                vm_name,
                status.code().unwrap_or(-1)
            );
        } else {
            anyhow::bail!("Command failed (exit {})", status.code().unwrap_or(-1));
        }
    }
    Ok(())
}

/// Run a bash script in the Linux execution environment, returning stdout as String.
pub fn run_on_vm_stdout(vm_name: &str, script: &str) -> Result<String> {
    let output = run_on_vm(vm_name, script)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a bash script in the default execution environment, capturing output.
pub fn run_in_vm(script: &str) -> Result<Output> {
    run_on_vm(VM_NAME, script)
}

/// Run a bash script in the default execution environment, with output visible to user.
pub fn run_in_vm_visible(script: &str) -> Result<()> {
    run_on_vm_visible(VM_NAME, script)
}

/// Run a bash script in the default execution environment, returning stdout as String.
pub fn run_in_vm_stdout(script: &str) -> Result<String> {
    run_on_vm_stdout(VM_NAME, script)
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
