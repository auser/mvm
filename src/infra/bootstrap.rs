use anyhow::Result;

use super::shell;
use super::ui;

/// Check that a package manager is available for the current platform.
///
/// - macOS: requires Homebrew
/// - Linux: requires apt, dnf, or pacman
pub fn check_package_manager() -> Result<()> {
    if cfg!(target_os = "macos") {
        check_homebrew()
    } else {
        check_linux_package_manager()
    }
}

/// Check if Homebrew is installed and accessible (macOS only).
pub fn check_homebrew() -> Result<()> {
    which::which("brew").map_err(|_| {
        anyhow::anyhow!(
            "Homebrew is not installed.\n\
             Install it first:\n\n  \
             /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"\n\n\
             Then run 'mvm bootstrap' again."
        )
    })?;
    ui::info("Homebrew found.");
    Ok(())
}

/// Check that a Linux package manager is available.
fn check_linux_package_manager() -> Result<()> {
    for cmd in &["apt-get", "dnf", "pacman"] {
        if which::which(cmd).is_ok() {
            ui::info(&format!("Package manager found: {}", cmd));
            return Ok(());
        }
    }
    anyhow::bail!(
        "No supported package manager found (apt-get, dnf, or pacman).\n\
         Install Lima manually: https://lima-vm.io/docs/installation/"
    )
}

/// Install Lima if not already installed.
///
/// - macOS: installs via Homebrew
/// - Linux: installs via the available package manager, or downloads the binary
pub fn ensure_lima() -> Result<()> {
    if which::which("limactl").is_ok() {
        let output = shell::run_host("limactl", &["--version"])?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        ui::info(&format!("Lima already installed: {}", version));
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        ui::info("Installing Lima via Homebrew...");
        shell::run_host_visible("brew", &["install", "lima"])?;
    } else {
        install_lima_linux()?;
    }

    which::which("limactl").map_err(|_| {
        anyhow::anyhow!("Lima installation completed but 'limactl' not found in PATH.")
    })?;

    ui::success("Lima installed successfully.");
    Ok(())
}

/// Install Lima on Linux via package manager or direct binary download.
fn install_lima_linux() -> Result<()> {
    if which::which("apt-get").is_ok() {
        ui::info("Installing Lima via apt...");
        // Lima is available in some Ubuntu/Debian repos, or via direct .deb
        shell::run_host_visible(
            "bash",
            &["-c", "curl -fsSL https://lima-vm.io/install.sh | sudo sh"],
        )?;
    } else if which::which("dnf").is_ok() {
        ui::info("Installing Lima via dnf...");
        shell::run_host_visible(
            "bash",
            &["-c", "curl -fsSL https://lima-vm.io/install.sh | sudo sh"],
        )?;
    } else if which::which("pacman").is_ok() {
        ui::info("Installing Lima via pacman...");
        shell::run_host_visible("sudo", &["pacman", "-S", "--noconfirm", "lima"])?;
    } else {
        anyhow::bail!(
            "No supported package manager found. Install Lima manually:\n\
             https://lima-vm.io/docs/installation/"
        );
    }
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
