use anyhow::Result;

use crate::shell;

/// Check if Homebrew is installed and accessible.
pub fn check_homebrew() -> Result<()> {
    which::which("brew").map_err(|_| {
        anyhow::anyhow!(
            "Homebrew is not installed.\n\
             Install it first:\n\n  \
             /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"\n\n\
             Then run 'mvm bootstrap' again."
        )
    })?;
    println!("[mvm] Homebrew found.");
    Ok(())
}

/// Install Lima via Homebrew if not already installed.
pub fn ensure_lima() -> Result<()> {
    if which::which("limactl").is_ok() {
        let output = shell::run_host("limactl", &["--version"])?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        println!("[mvm] Lima already installed: {}", version);
        return Ok(());
    }

    println!("[mvm] Installing Lima via Homebrew...");
    shell::run_host_visible("brew", &["install", "lima"])?;

    which::which("limactl").map_err(|_| {
        anyhow::anyhow!("Lima installation completed but 'limactl' not found in PATH.")
    })?;

    println!("[mvm] Lima installed successfully.");
    Ok(())
}
