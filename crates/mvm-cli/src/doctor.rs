use anyhow::Result;

use crate::ui;
use mvm_core::config::fc_version;
use mvm_runtime::shell;

#[derive(Debug)]
struct Check {
    name: &'static str,
    cmd: &'static str,
    ok: bool,
    info: String,
}

pub fn run() -> Result<()> {
    let mut checks = Vec::new();

    // Host tools
    checks.push(check_cmd("rustup", "rustup --version"));
    checks.push(check_cmd("cargo", "cargo --version"));
    checks.push(check_cmd("limactl", "limactl --version"));

    // Inside VM tools (if available)
    let in_vm = shell::inside_lima();
    if in_vm {
        checks.push(check_vm_cmd("nix", "nix --version"));
        checks.push(check_vm_cmd("firecracker", "firecracker --version"));
    } else {
        checks.push(check_cmd("firecracker", "firecracker --version"));
    }

    // Firecracker version target
    checks.push(Check {
        name: "fc target",
        cmd: "env",
        ok: true,
        info: fc_version(),
    });

    // Render
    ui::status_header();
    for c in &checks {
        let status = if c.ok { "OK" } else { "MISSING" };
        ui::status_line(&format!("{}:", c.name), &format!("{} ({})", status, c.info));
    }

    let missing: Vec<&Check> = checks.iter().filter(|c| !c.ok).collect();
    if !missing.is_empty() {
        ui::warn("Some dependencies are missing. Install them and re-run:");
        for m in missing {
            ui::info(&format!("  {} -> {}", m.name, m.cmd));
        }
        anyhow::bail!("doctor found missing dependencies");
    }

    ui::success("All required tools present.");
    Ok(())
}

fn check_cmd(name: &'static str, cmd: &'static str) -> Check {
    match shell::run_host("bash", &["-lc", cmd]) {
        Ok(out) if out.status.success() => Check {
            name,
            cmd,
            ok: true,
            info: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => Check {
            name,
            cmd,
            ok: false,
            info: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(e) => Check {
            name,
            cmd,
            ok: false,
            info: e.to_string(),
        },
    }
}

fn check_vm_cmd(name: &'static str, cmd: &'static str) -> Check {
    match shell::run_on_vm("mvm", cmd) {
        Ok(out) if out.status.success() => Check {
            name,
            cmd,
            ok: true,
            info: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => Check {
            name,
            cmd,
            ok: false,
            info: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(e) => Check {
            name,
            cmd,
            ok: false,
            info: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_struct_reports_ok() {
        let c = Check {
            name: "test-tool",
            cmd: "test-tool --version",
            ok: true,
            info: "1.0.0".to_string(),
        };
        assert!(c.ok);
        assert_eq!(c.name, "test-tool");
    }

    #[test]
    fn check_struct_reports_missing() {
        let c = Check {
            name: "missing-tool",
            cmd: "missing-tool --version",
            ok: false,
            info: "not found".to_string(),
        };
        assert!(!c.ok);
    }

    #[test]
    fn inside_lima_is_false_on_host() {
        // On the macOS host (CI or dev machine), LIMA_INSTANCE is not set and
        // Lima-specific files don't exist — so this should return false.
        // This validates that the detection logic doesn't false-positive.
        if std::env::var("LIMA_INSTANCE").is_err()
            && !std::path::Path::new("/etc/lima-boot.conf").exists()
        {
            assert!(!shell::inside_lima());
        }
    }

    #[test]
    fn check_cmd_rustup_on_host() {
        // rustup should be present on any dev machine / CI running cargo test
        let c = check_cmd("rustup", "rustup --version");
        assert!(c.ok, "rustup should be available: {}", c.info);
        assert!(
            c.info.contains("rustup"),
            "expected version string, got: {}",
            c.info
        );
    }

    #[test]
    fn check_cmd_cargo_on_host() {
        let c = check_cmd("cargo", "cargo --version");
        assert!(c.ok, "cargo should be available: {}", c.info);
        assert!(
            c.info.contains("cargo"),
            "expected version string, got: {}",
            c.info
        );
    }

    #[test]
    fn check_cmd_missing_tool() {
        let c = check_cmd(
            "nonexistent-mvm-tool-xyz",
            "nonexistent-mvm-tool-xyz --version",
        );
        assert!(!c.ok, "nonexistent tool should fail");
    }

    #[test]
    fn fc_target_version_is_nonempty() {
        let v = mvm_core::config::fc_version();
        assert!(!v.is_empty(), "FC version should be configured");
        assert!(
            v.starts_with('v'),
            "FC version should start with 'v': {}",
            v
        );
    }
}
