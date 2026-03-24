use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

use mvm_core::build_env::ShellEnvironment;

use crate::{shell, ui};

/// Shell environment implementation that delegates to the Lima VM.
///
/// Used by the CLI for dev-mode builds (`mvm build --flake`, `mvm run`,
/// `mvm template build`).
pub struct RuntimeBuildEnv;

impl ShellEnvironment for RuntimeBuildEnv {
    fn shell_exec(&self, script: &str) -> Result<()> {
        let out = shell::run_in_vm(script)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "Command failed (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn shell_exec_stdout(&self, script: &str) -> Result<String> {
        let out = shell::run_in_vm(script)?;
        if !out.status.success() {
            return Err(anyhow!(
                "Command failed (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn shell_exec_visible(&self, script: &str) -> Result<()> {
        shell::run_in_vm_visible(script)
    }

    fn log_info(&self, msg: &str) {
        ui::info(msg);
    }

    fn log_success(&self, msg: &str) {
        ui::success(msg);
    }

    fn log_warn(&self, msg: &str) {
        ui::warn(msg);
    }

    fn shell_exec_capture(&self, script: &str) -> Result<(String, String)> {
        let out = shell::run_in_vm_capture(script)?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            let output = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{}\n{}", stdout, stderr)
            };
            return Err(anyhow!(
                "Command failed (exit {}):\n{}",
                out.status.code().unwrap_or(-1),
                output
            ));
        }
        Ok((stdout, stderr))
    }
}

/// Shell environment that runs commands on the macOS host directly.
///
/// Used when Nix is available on the host and can build Linux targets
/// without Lima. This avoids starting a full Ubuntu VM just for
/// `nix build`. Callers should use `RuntimeBuildEnv` for operations
/// that need a Linux kernel (ext4 mount, Firecracker, TAP devices).
pub struct HostBuildEnv;

impl ShellEnvironment for HostBuildEnv {
    fn shell_exec(&self, script: &str) -> Result<()> {
        let out = Command::new("bash")
            .args(["-c", script])
            .output()
            .context("Failed to run command on host")?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "Command failed on host (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn shell_exec_stdout(&self, script: &str) -> Result<String> {
        let out = Command::new("bash")
            .args(["-c", script])
            .output()
            .context("Failed to run command on host")?;
        if !out.status.success() {
            return Err(anyhow!(
                "Command failed on host (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn shell_exec_visible(&self, script: &str) -> Result<()> {
        let status = Command::new("bash")
            .args(["-c", script])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run command on host")?;
        if !status.success() {
            anyhow::bail!(
                "Command failed on host (exit {})",
                status.code().unwrap_or(-1)
            );
        }
        Ok(())
    }

    fn log_info(&self, msg: &str) {
        ui::info(msg);
    }

    fn log_success(&self, msg: &str) {
        ui::success(msg);
    }

    fn log_warn(&self, msg: &str) {
        ui::warn(msg);
    }

    fn shell_exec_capture(&self, script: &str) -> Result<(String, String)> {
        let out = Command::new("bash")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to run command on host")?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            let output = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{}\n{}", stdout, stderr)
            };
            return Err(anyhow!(
                "Command failed on host (exit {}):\n{}",
                out.status.code().unwrap_or(-1),
                output
            ));
        }
        Ok((stdout, stderr))
    }
}

/// Choose the best build environment for the current platform.
///
/// - macOS with host Nix: `HostBuildEnv` (no Lima needed for builds)
/// - Everything else: `RuntimeBuildEnv` (routes through LinuxEnv)
pub fn default_build_env() -> Box<dyn ShellEnvironment> {
    let plat = mvm_core::platform::current();
    if plat == mvm_core::platform::Platform::MacOS && plat.has_host_nix() {
        Box::new(HostBuildEnv)
    } else {
        Box::new(RuntimeBuildEnv)
    }
}
