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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_homebrew_error_message() {
        // Verify the error message contains install instructions when brew is missing.
        // We can't control whether brew is installed, so test the message format
        // by checking what the function returns in the error case.
        if which::which("brew").is_err() {
            let err = check_homebrew().unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("Homebrew is not installed"));
            assert!(msg.contains("curl -fsSL"));
            assert!(msg.contains("mvm bootstrap"));
        } else {
            // brew is available — check_homebrew should succeed
            assert!(check_homebrew().is_ok());
        }
    }

    #[test]
    fn test_ensure_lima_when_limactl_present() {
        // If limactl is available, ensure_lima should succeed without installing
        if which::which("limactl").is_ok() {
            assert!(ensure_lima().is_ok());
        }
    }
}
