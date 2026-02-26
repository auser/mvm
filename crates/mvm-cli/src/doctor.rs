use anyhow::Result;
use serde::Serialize;

use crate::ui;
use mvm_core::config::fc_version;
use mvm_core::platform::{self, Platform};
use mvm_runtime::shell;
use mvm_runtime::vm::lima;

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    category: &'static str,
    ok: bool,
    info: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    checks: Vec<Check>,
    all_ok: bool,
}

pub fn run(json: bool) -> Result<()> {
    let mut checks = Vec::new();

    // ── Tools ──────────────────────────────────────────────────────
    checks.push(check_cmd("rustup", "tools", "rustup --version"));
    checks.push(check_cmd("cargo", "tools", "cargo --version"));

    let in_vm = shell::inside_lima();
    if in_vm {
        // Inside Lima VM: limactl is not needed, nix and firecracker are local
        checks.push(check_cmd("nix", "tools", "nix --version"));
        checks.push(check_cmd("firecracker", "tools", "firecracker --version"));
    } else {
        // On host: limactl needed for macOS, firecracker checked via Lima
        if platform::current().needs_lima() {
            checks.push(check_cmd("limactl", "tools", "limactl --version"));
        }
        checks.push(check_vm_cmd(
            "firecracker",
            "tools",
            "firecracker --version",
        ));
    }

    checks.push(Check {
        name: "fc target",
        category: "tools",
        ok: true,
        info: fc_version(),
    });

    // ── Platform ──────────────────────────────────────────────────
    let plat = platform::current();
    checks.push(Check {
        name: "platform",
        category: "platform",
        ok: true,
        info: platform_description(plat),
    });

    checks.push(kvm_check(plat, in_vm));

    if plat.needs_lima() {
        checks.push(lima_status_check());
    }

    checks.push(disk_space_check(in_vm));

    // ── Render ────────────────────────────────────────────────────
    let all_ok = checks.iter().all(|c| c.ok);
    let report = DoctorReport { checks, all_ok };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !report.all_ok {
            anyhow::bail!("doctor found issues");
        }
        return Ok(());
    }

    render_text(&report);

    if !report.all_ok {
        let missing: Vec<&Check> = report.checks.iter().filter(|c| !c.ok).collect();
        ui::warn("\nIssues found:");
        for m in &missing {
            ui::info(&format!("  {} — {}", m.name, m.info));
        }
        anyhow::bail!("doctor found issues");
    }

    ui::success("\nAll checks passed.");
    Ok(())
}

fn render_text(report: &DoctorReport) {
    let mut current_category = "";
    for c in &report.checks {
        if c.category != current_category {
            current_category = c.category;
            let title = match current_category {
                "tools" => "Tools",
                "platform" => "Platform",
                _ => current_category,
            };
            println!("\n{}", title);
            println!("{}", "-".repeat(title.len()));
        }
        let status = if c.ok { "OK" } else { "MISSING" };
        ui::status_line(
            &format!("  {}:", c.name),
            &format!("{} ({})", status, c.info),
        );
    }
}

// ── Tool checks ───────────────────────────────────────────────────────────

fn check_cmd(name: &'static str, category: &'static str, cmd: &'static str) -> Check {
    match shell::run_host("bash", &["-lc", cmd]) {
        Ok(out) if out.status.success() => Check {
            name,
            category,
            ok: true,
            info: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => Check {
            name,
            category,
            ok: false,
            info: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(e) => Check {
            name,
            category,
            ok: false,
            info: e.to_string(),
        },
    }
}

fn check_vm_cmd(name: &'static str, category: &'static str, cmd: &'static str) -> Check {
    match shell::run_on_vm("mvm", cmd) {
        Ok(out) if out.status.success() => Check {
            name,
            category,
            ok: true,
            info: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => Check {
            name,
            category,
            ok: false,
            info: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(e) => Check {
            name,
            category,
            ok: false,
            info: e.to_string(),
        },
    }
}

// ── Platform checks ───────────────────────────────────────────────────────

fn platform_description(plat: Platform) -> String {
    match plat {
        Platform::MacOS => "macOS (Lima required)".to_string(),
        Platform::LinuxNative => "Linux with KVM".to_string(),
        Platform::LinuxNoKvm => "Linux without KVM (Lima required)".to_string(),
    }
}

fn kvm_check(plat: Platform, in_vm: bool) -> Check {
    // Inside Lima VM or native Linux: check /dev/kvm locally
    if in_vm || plat == Platform::LinuxNative || plat == Platform::LinuxNoKvm {
        // Use test -c (character device exists) rather than test -r (readable),
        // because KVM access may be via group membership which doesn't imply -r.
        return match shell::run_host("bash", &["-c", "test -c /dev/kvm && echo ok"]) {
            Ok(out) if out.status.success() => {
                let context = if in_vm {
                    "available (inside Lima VM)"
                } else {
                    "available"
                };
                Check {
                    name: "kvm",
                    category: "platform",
                    ok: true,
                    info: context.to_string(),
                }
            }
            _ => Check {
                name: "kvm",
                category: "platform",
                ok: false,
                info: if in_vm {
                    "/dev/kvm not accessible inside Lima VM".to_string()
                } else {
                    "not available. Enable virtualization in BIOS or check permissions on /dev/kvm."
                        .to_string()
                },
            },
        };
    }

    // macOS host: check /dev/kvm inside the Lima VM
    match shell::run_in_vm("test -c /dev/kvm && echo ok") {
        Ok(out) if out.status.success() => Check {
            name: "kvm",
            category: "platform",
            ok: true,
            info: "available (via Lima VM)".to_string(),
        },
        _ => Check {
            name: "kvm",
            category: "platform",
            ok: false,
            info: "Lima VM not running or /dev/kvm unavailable. Run 'mvm setup'.".to_string(),
        },
    }
}

fn lima_status_check() -> Check {
    match lima::get_status() {
        Ok(lima::LimaStatus::Running) => Check {
            name: "lima vm",
            category: "platform",
            ok: true,
            info: "running".to_string(),
        },
        Ok(lima::LimaStatus::Stopped) => Check {
            name: "lima vm",
            category: "platform",
            ok: false,
            info: "stopped. Run 'mvm dev' or 'limactl start mvm'.".to_string(),
        },
        Ok(lima::LimaStatus::NotFound) => Check {
            name: "lima vm",
            category: "platform",
            ok: false,
            info: "not found. Run 'mvm setup' or 'mvm bootstrap'.".to_string(),
        },
        Err(e) => Check {
            name: "lima vm",
            category: "platform",
            ok: false,
            info: format!("check failed: {}", e),
        },
    }
}

fn disk_space_check(in_vm: bool) -> Check {
    let result = if in_vm {
        parse_disk_space("df -BG /var/lib/mvm 2>/dev/null || df -BG / 2>/dev/null")
    } else if cfg!(target_os = "macos") {
        parse_disk_space("df -g ~ 2>/dev/null")
    } else {
        parse_disk_space("df -BG /var/lib/mvm 2>/dev/null || df -BG / 2>/dev/null")
    };

    match result {
        Some(gib) if gib >= 10 => Check {
            name: "disk space",
            category: "platform",
            ok: true,
            info: format!("{} GiB free", gib),
        },
        Some(gib) => Check {
            name: "disk space",
            category: "platform",
            ok: false,
            info: format!("only {} GiB free (10 GiB recommended)", gib),
        },
        None => Check {
            name: "disk space",
            category: "platform",
            ok: true,
            info: "unable to determine (skipped)".to_string(),
        },
    }
}

/// Parse free disk space in GiB from `df` output.
/// Expects the 4th column of the 2nd line to be the available space with a G suffix.
fn parse_disk_space(cmd: &str) -> Option<u64> {
    let output = shell::run_host("bash", &["-c", cmd]).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1)?;
    let avail = line.split_whitespace().nth(3)?;
    let num_str = avail.trim_end_matches('G').trim_end_matches('i');
    num_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_struct_reports_ok() {
        let c = Check {
            name: "test-tool",
            category: "tools",
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
            category: "tools",
            ok: false,
            info: "not found".to_string(),
        };
        assert!(!c.ok);
    }

    #[test]
    fn inside_lima_is_false_on_host() {
        if std::env::var("LIMA_INSTANCE").is_err()
            && !std::path::Path::new("/etc/lima-boot.conf").exists()
        {
            assert!(!shell::inside_lima());
        }
    }

    #[test]
    fn check_cmd_rustup_on_host() {
        let c = check_cmd("rustup", "tools", "rustup --version");
        assert!(c.ok, "rustup should be available: {}", c.info);
        assert!(
            c.info.contains("rustup"),
            "expected version string, got: {}",
            c.info
        );
    }

    #[test]
    fn check_cmd_cargo_on_host() {
        let c = check_cmd("cargo", "tools", "cargo --version");
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
            "tools",
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

    #[test]
    fn platform_description_covers_all_variants() {
        assert!(platform_description(Platform::MacOS).contains("macOS"));
        assert!(platform_description(Platform::LinuxNative).contains("KVM"));
        assert!(platform_description(Platform::LinuxNoKvm).contains("without KVM"));
    }

    #[test]
    fn parse_disk_space_typical_output() {
        let result = parse_disk_space(
            "printf 'Filesystem     1G-blocks  Used Available Use%% Mounted on\n/dev/sda1           100G   55G       45G  55%% /\n'",
        );
        assert_eq!(result, Some(45));
    }

    #[test]
    fn doctor_report_serializes_to_json() {
        let report = DoctorReport {
            checks: vec![Check {
                name: "test",
                category: "tools",
                ok: true,
                info: "v1.0".to_string(),
            }],
            all_ok: true,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"name\":\"test\""));
        assert!(json.contains("\"all_ok\":true"));
    }
}
