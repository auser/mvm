use anyhow::Result;

use crate::ui;
use mvm_core::platform::{self, Platform};
use mvm_runtime::config::{LEGACY_VM_NAME, VM_NAME};
use mvm_runtime::shell;

/// Check that a package manager is available for the current platform.
///
/// - macOS: requires Homebrew
/// - Linux: requires apt, dnf, or pacman
/// - Windows: requires WSL2 (delegates to [`bootstrap_wsl2`])
pub fn check_package_manager() -> Result<()> {
    if cfg!(target_os = "macos") {
        check_homebrew()
    } else if cfg!(target_os = "windows") {
        bootstrap_wsl2()
    } else {
        check_linux_package_manager()
    }
}

/// WSL2 bootstrap path (plan 53 §"Plan I.4"). On Windows, mvm runs
/// inside a WSL2 distro — the Windows-side `mvmctl.exe` is a launcher
/// that ensures WSL2 is configured and the Linux-side `mvmctl` is
/// installed inside the chosen distro.
///
/// The current implementation detects WSL2 readiness and surfaces an
/// install hint; full automation (running `wsl --install` + provisioning
/// the distro) happens in a follow-up once we've validated the user
/// flow on a real Windows host. See
/// `public/.../guides/windows-wsl2.md` for the manual walkthrough
/// users follow today.
#[cfg(target_os = "windows")]
pub fn bootstrap_wsl2() -> Result<()> {
    use std::process::Command;

    if which::which("wsl").is_err() {
        anyhow::bail!(
            "WSL is not installed on this Windows host.\n\
             Install it from an elevated PowerShell:\n  \
                 wsl --install\n\
             Then reboot and re-run `mvmctl bootstrap`. See\n  \
                 https://github.com/auser/mvm/blob/main/public/src/content/docs/install/windows.md"
        );
    }

    // `wsl --status` returns 0 if WSL2 is configured and at least one
    // distro is registered. We don't parse the output — exit code is
    // sufficient signal for the bootstrap path.
    let status = Command::new("wsl")
        .arg("--status")
        .status()
        .map_err(|e| anyhow::anyhow!("could not invoke wsl: {e}"))?;
    if !status.success() {
        anyhow::bail!(
            "WSL2 is not configured.\n\
             From an elevated PowerShell:\n  \
                 wsl --install\n  \
                 wsl --update\n\
             Then re-run `mvmctl bootstrap`."
        );
    }

    ui::info(
        "WSL2 detected. Install mvmctl inside your WSL2 distro and run\n  \
            wsl -d Ubuntu -- mvmctl bootstrap\n\
         to complete setup. See guides/windows-wsl2 for the full walkthrough.",
    );
    Ok(())
}

/// On non-Windows hosts, `bootstrap_wsl2` is a no-op so callers don't
/// have to cfg-gate at every call site.
#[cfg(not(target_os = "windows"))]
pub fn bootstrap_wsl2() -> Result<()> {
    Ok(())
}

/// Check if Homebrew is installed and accessible (macOS only).
pub fn check_homebrew() -> Result<()> {
    which::which("brew").map_err(|_| {
        anyhow::anyhow!(
            "Homebrew is not installed.\n\
             Install it first:\n\n  \
             /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"\n\n\
             Then run 'mvmctl bootstrap' again."
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
/// On native Linux with KVM, Lima is not required — this is a no-op.
/// On macOS or Linux without KVM: installs Lima via package manager.
pub fn ensure_lima() -> Result<()> {
    if platform::current() == Platform::LinuxNative {
        ui::info("Native Linux with KVM detected — Lima not required.");
        return Ok(());
    }

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

/// Install Lima on Linux via binary download from GitHub releases.
fn install_lima_linux() -> Result<()> {
    // Check for Homebrew first (works on Linux)
    if which::which("brew").is_ok() {
        ui::info("Installing Lima via Homebrew...");
        shell::run_host_visible("brew", &["install", "lima"])?;
        return Ok(());
    }

    // Check for Nix (cross-platform)
    if which::which("nix-env").is_ok() {
        ui::info("Installing Lima via Nix...");
        shell::run_host_visible("nix-env", &["-i", "lima"])?;
        return Ok(());
    }

    // Fallback: Download binary from GitHub releases
    ui::info("Installing Lima from GitHub releases...");
    let install_script = r#"
set -euo pipefail
LIMA_VERSION=$(curl -fsSL https://api.github.com/repos/lima-vm/lima/releases/latest | grep '"tag_name"' | sed -E 's/.*"v([^"]+)".*/\1/')
ARCH=$(uname -m)
case "$ARCH" in
    x86_64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
URL="https://github.com/lima-vm/lima/releases/download/v${LIMA_VERSION}/lima-${LIMA_VERSION}-Linux-${ARCH}.tar.gz"
echo "Downloading Lima ${LIMA_VERSION} for ${ARCH}..."
curl -fsSL "$URL" | sudo tar -xz -C /usr/local
sudo chmod +x /usr/local/bin/limactl
echo "Lima ${LIMA_VERSION} installed successfully"
"#;
    shell::run_host_visible("bash", &["-c", install_script])?;
    Ok(())
}

/// Check if the platform requires Lima.
pub fn is_lima_required() -> bool {
    platform::current().needs_lima()
}

/// Print an informational hint about libkrun availability (plan 53
/// §"Plan E"). libkrun is optional — when it's available, `mvmctl run`
/// can use it as a Tier 2 backend on macOS Intel and macOS <26 (where
/// Apple Container is unavailable) without going through Lima. This
/// function does *not* attempt to install libkrun automatically since
/// it lives in the host's package manager (Homebrew on macOS, distro
/// packages on Linux).
///
/// Idempotent and safe to call from any bootstrap path.
pub fn hint_libkrun_if_useful() {
    let plat = platform::current();
    // Skip on Linux+KVM (Firecracker is the right backend) and on
    // Windows (libkrun has no Windows port).
    if plat.has_kvm() || plat.is_windows() {
        return;
    }
    if mvm_libkrun::is_available() {
        ui::info(
            "Detected libkrun on this host; you can opt in with `mvmctl run --hypervisor libkrun`.",
        );
        return;
    }
    // Only suggest the install on platforms where libkrun would
    // materially improve the user experience — i.e. macOS without
    // Apple Container, where today the only path is Lima.
    if matches!(plat, Platform::MacOS) && !plat.has_apple_containers() {
        ui::info(&format!(
            "Tip: install libkrun for a no-Lima Tier 2 microVM path on this Mac.\n  {}",
            mvm_libkrun::install_hint()
        ));
    }
}

/// Detect a legacy `mvm` Lima VM left over from before W7.2 renamed the
/// builder VM to `mvm-builder`. Returns true if the legacy VM exists, in
/// which case `mvmctl` prints a one-line migration command for the user to
/// run themselves. We do **not** auto-rename: `limactl` lacks an in-place
/// rename, the legacy VM may still be running tenant work, and the host
/// mutation boundary in the Nix best-practices guide says destructive ops
/// stay user-visible.
///
/// No-op when Lima isn't required (native Linux + KVM) or `limactl` isn't
/// installed yet.
pub fn warn_if_legacy_lima_vm() -> Result<()> {
    if !is_lima_required() || which::which("limactl").is_err() {
        return Ok(());
    }

    let output = match shell::run_host("limactl", &["list", "--json"]) {
        Ok(out) if out.status.success() => out,
        _ => return Ok(()),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    // `limactl list --json` emits one JSON object per line. Cheap substring
    // check on the legacy name is enough — false positives just mean an
    // extra warning, no destructive action attached.
    let legacy_marker = format!("\"name\":\"{LEGACY_VM_NAME}\"");
    if !stdout.contains(&legacy_marker) {
        return Ok(());
    }

    ui::warn(&format!(
        "Detected a legacy Lima VM named '{LEGACY_VM_NAME}'. \
         The builder VM was renamed to '{VM_NAME}' (W7.2).\n\
         To migrate, run:\n\
         \n  \
         limactl stop '{LEGACY_VM_NAME}' && limactl rename '{LEGACY_VM_NAME}' '{VM_NAME}'\n\
         \n\
         This is *not* automated: `limactl rename` requires the VM to be \
         stopped, and an auto-rename could interrupt in-flight builds. \
         Until you migrate, `mvmctl dev up` will create a fresh \
         '{VM_NAME}' alongside your existing '{LEGACY_VM_NAME}' \
         (no data loss; just disk overhead)."
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_homebrew_error_message() {
        if which::which("brew").is_err() {
            let err = check_homebrew().unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("Homebrew is not installed"));
            assert!(msg.contains("curl -fsSL"));
            assert!(msg.contains("mvmctl bootstrap"));
        } else {
            assert!(check_homebrew().is_ok());
        }
    }

    #[test]
    fn test_ensure_lima_when_limactl_present() {
        if which::which("limactl").is_ok() {
            assert!(ensure_lima().is_ok());
        }
    }

    #[test]
    fn test_is_lima_required() {
        let _ = is_lima_required();
    }
}
