use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Incoming request from host to the builder agent (guest side, via vsock/serial).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BuilderRequest {
    /// Build a flake attribute with an optional timeout (seconds).
    Build {
        flake_ref: String,
        attr: String,
        timeout_secs: Option<u64>,
    },
    /// Health probe.
    Ping,
}

/// Vsock port used by the builder agent.
pub const BUILDER_AGENT_PORT: u32 = 21470;

/// Outgoing responses/log frames from the builder agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BuilderResponse {
    /// Build succeeded; artifact root placed in /build-out.
    Ok { out_path: String },
    /// Build failed.
    Err { message: String },
    /// Streaming log line (stdout/stderr).
    Log { line: String },
    /// Pong for health probes.
    Pong,
}

/// Path where SecurityPolicy is provisioned on the config drive.
pub const SECURITY_POLICY_PATH: &str = "/mnt/config/security-policy.json";

/// Shell metacharacters that must not appear in builder inputs.
const DANGEROUS_CHARS: &[char] = &[';', '|', '&', '$', '`', '(', ')', '{', '}', '<', '>'];

/// Validate a flake reference for safety.
///
/// Rejects references containing shell metacharacters or path traversal.
/// Allowed patterns: `.`, `/build-in`, `github:owner/repo`, `git+https://...`,
/// `path:...`, or similar Nix flake ref formats.
pub fn validate_flake_ref(flake_ref: &str) -> Result<(), String> {
    if flake_ref.is_empty() {
        return Err("flake_ref is empty".to_string());
    }

    // Reject shell metacharacters
    for ch in DANGEROUS_CHARS {
        if flake_ref.contains(*ch) {
            return Err(format!("flake_ref contains dangerous character: '{}'", ch));
        }
    }

    // Reject path traversal
    if flake_ref.contains("..") {
        return Err("flake_ref contains path traversal (..)".to_string());
    }

    Ok(())
}

/// Validate a build attribute for safety.
///
/// Must start with `packages.` to prevent arbitrary Nix evaluation.
/// Rejects shell metacharacters.
pub fn validate_build_attr(attr: &str) -> Result<(), String> {
    if !attr.starts_with("packages.") {
        return Err(format!(
            "build attr must start with 'packages.', got: '{}'",
            attr
        ));
    }

    for ch in DANGEROUS_CHARS {
        if attr.contains(*ch) {
            return Err(format!("build attr contains dangerous character: '{}'", ch));
        }
    }

    Ok(())
}

/// Load `SecurityPolicy` from the config drive.
///
/// Returns `Ok(None)` if the file does not exist (policy not provisioned).
pub fn load_security_policy() -> Result<Option<mvm_core::security::SecurityPolicy>> {
    let path = Path::new(SECURITY_POLICY_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", SECURITY_POLICY_PATH))?;
    let policy: mvm_core::security::SecurityPolicy = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", SECURITY_POLICY_PATH))?;
    Ok(Some(policy))
}

fn log_frame(line: &str) -> BuilderResponse {
    BuilderResponse::Log {
        line: line.to_string(),
    }
}

/// Run the requested build using nix inside the guest and stage artifacts into /build-out.
pub fn handle_request(req: BuilderRequest) -> Result<BuilderResponse> {
    match req {
        BuilderRequest::Ping => Ok(BuilderResponse::Pong),
        BuilderRequest::Build {
            flake_ref,
            attr,
            timeout_secs,
        } => {
            let timeout = timeout_secs.unwrap_or(1800);

            let out_mount = Path::new("/build-out");
            if !out_mount.is_dir() {
                return Ok(BuilderResponse::Err {
                    message: "/build-out missing or not a directory".into(),
                });
            }
            // Best-effort mount of /dev/vdb -> /build-out if not already mounted.
            if Command::new("sh")
                .arg("-c")
                .arg("mountpoint -q /build-out || (mkdir -p /build-out && mount /dev/vdb /build-out)")
                .status()
                .is_err()
            {
                // continue; the copy will fail and report
            }
            if flake_ref == "/build-in" {
                let _ = Command::new("sh").arg("-c").arg(
                    "mountpoint -q /build-in || (mkdir -p /build-in && mount /dev/vdc /build-in)",
                ).status();
            }

            // nix build
            let build_cmd = format!(
                "timeout {} nix build {}#{} --no-link --print-out-paths",
                timeout, flake_ref, attr
            );
            let output = Command::new("sh")
                .arg("-c")
                .arg(&build_cmd)
                .output()
                .with_context(|| "failed to run nix build")?;

            // Emit stdout/stderr as log frames (best-effort)
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let _ = log_frame(line);
            }
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                let _ = log_frame(line);
            }

            if !output.status.success() {
                return Ok(BuilderResponse::Err {
                    message: format!("nix build failed (exit {}): {}", output.status, build_cmd),
                });
            }

            // Last store path from stdout
            let stdout = String::from_utf8_lossy(&output.stdout);
            let out_path = stdout
                .lines()
                .rev()
                .find(|l| l.starts_with("/nix/store/"))
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("nix build produced no store path"))?;

            // Copy artifacts to /build-out
            let copy_cmd = format!(
                "set -euo pipefail; \
                 cp {p}/kernel /build-out/vmlinux 2>/dev/null || cp {p}/vmlinux /build-out/vmlinux; \
                 cp {p}/rootfs /build-out/rootfs.ext4 2>/dev/null || cp {p}/rootfs.ext4 /build-out/rootfs.ext4; \
                 echo '{{\"note\":\"Base fc config placeholder\"}}' > /build-out/fc-base.json",
                p = out_path
            );
            let copy_out = Command::new("sh")
                .arg("-c")
                .arg(&copy_cmd)
                .output()
                .with_context(|| "failed to copy build artifacts")?;
            if !copy_out.status.success() {
                return Ok(BuilderResponse::Err {
                    message: format!("failed to copy artifacts: exit {}", copy_out.status),
                });
            }

            Ok(BuilderResponse::Ok { out_path })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip() {
        let req = BuilderRequest::Build {
            flake_ref: ".".into(),
            attr: "packages.aarch64-linux.tenant-worker".into(),
            timeout_secs: Some(123),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: BuilderRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    // -- validate_flake_ref tests --

    #[test]
    fn test_validate_flake_ref_safe_patterns() {
        assert!(validate_flake_ref(".").is_ok());
        assert!(validate_flake_ref("/build-in").is_ok());
        assert!(validate_flake_ref("github:auser/mvm").is_ok());
        assert!(validate_flake_ref("git+https://github.com/auser/mvm").is_ok());
        assert!(validate_flake_ref("path:/some/local/path").is_ok());
    }

    #[test]
    fn test_validate_flake_ref_empty() {
        assert!(validate_flake_ref("").is_err());
    }

    #[test]
    fn test_validate_flake_ref_rejects_shell_metacharacters() {
        assert!(validate_flake_ref(". ; rm -rf /").is_err());
        assert!(validate_flake_ref("$(evil)").is_err());
        assert!(validate_flake_ref("`evil`").is_err());
        assert!(validate_flake_ref("foo | bar").is_err());
        assert!(validate_flake_ref("foo & bar").is_err());
        assert!(validate_flake_ref("foo > /dev/sda").is_err());
        assert!(validate_flake_ref("foo < /etc/passwd").is_err());
    }

    #[test]
    fn test_validate_flake_ref_rejects_path_traversal() {
        assert!(validate_flake_ref("../../etc/passwd").is_err());
        assert!(validate_flake_ref("/build-in/../../../etc").is_err());
    }

    // -- validate_build_attr tests --

    #[test]
    fn test_validate_build_attr_valid() {
        assert!(validate_build_attr("packages.aarch64-linux.tenant-worker").is_ok());
        assert!(validate_build_attr("packages.x86_64-linux.default").is_ok());
        assert!(validate_build_attr("packages.aarch64-linux.minimal").is_ok());
    }

    #[test]
    fn test_validate_build_attr_rejects_non_packages() {
        assert!(validate_build_attr("devShells.x86_64-linux.default").is_err());
        assert!(validate_build_attr("nixosConfigurations.default").is_err());
        assert!(validate_build_attr("").is_err());
        assert!(validate_build_attr("arbitrary-attr").is_err());
    }

    #[test]
    fn test_validate_build_attr_rejects_metacharacters() {
        assert!(validate_build_attr("packages.x86_64-linux.default; rm -rf /").is_err());
        assert!(validate_build_attr("packages.$(evil)").is_err());
        assert!(validate_build_attr("packages.`whoami`").is_err());
    }

    // -- load_security_policy tests --

    #[test]
    fn test_load_security_policy_missing_file() {
        // Config file doesn't exist — should return Ok(None)
        let result = load_security_policy();
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
