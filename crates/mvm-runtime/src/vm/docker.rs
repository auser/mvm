//! Docker backend for mvm.
//!
//! Runs Nix-built microVM images as Docker containers. Uses the OCI
//! `image.tar.gz` produced by `mkGuest` (via `dockerTools.streamLayeredImage`),
//! loaded with `docker load`. Falls back to `docker import` for raw ext4.
//!
//! Guest communication uses a unix socket (volume-mounted) instead of vsock.
//! Port forwarding uses Docker's native `-p` flag.
//! Containers run detached by default — no launchd or foreground blocking needed.

use anyhow::Result;
use mvm_core::vm_backend::{
    GuestChannelInfo, VmBackend, VmCapabilities, VmId, VmInfo, VmNetworkInfo, VmStartConfig,
    VmStatus,
};

use crate::ui;

/// Label applied to all mvm-managed Docker containers.
const MVM_LABEL: &str = "mvm.managed=true";

/// Container name prefix.
fn container_name(vm_name: &str) -> String {
    format!("mvm-{vm_name}")
}

/// Image tag convention.
fn image_tag(vm_name: &str) -> String {
    format!("mvm-{vm_name}:latest")
}

/// Run a `docker` command and return the output.
fn docker_cmd(args: &[&str]) -> Result<std::process::Output> {
    let output = std::process::Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("docker command failed: {e}"))?;
    Ok(output)
}

/// Run a `docker` command, check success, return stdout.
fn docker_stdout(args: &[&str]) -> Result<String> {
    let output = docker_cmd(args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker {} failed: {}", args.first().unwrap_or(&""), stderr);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Load the OCI image into Docker.
fn load_image(rootfs_dir: &str, vm_name: &str) -> Result<String> {
    let oci_image = std::path::Path::new(rootfs_dir)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("image.tar.gz");

    let tag = image_tag(vm_name);

    if oci_image.exists() {
        // Load OCI tarball (produced by Nix streamLayeredImage)
        ui::info(&format!(
            "Loading OCI image from {}...",
            oci_image.display()
        ));
        let output = docker_cmd(&["load", "-i", oci_image.to_str().unwrap_or("")])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("docker load failed: {stderr}");
        }
        // docker load outputs "Loaded image: <name>:<tag>"
        // Tag it with our convention
        let loaded = String::from_utf8_lossy(&output.stdout);
        if let Some(loaded_name) = loaded.trim().strip_prefix("Loaded image: ")
            && loaded_name != tag
        {
            let _ = docker_cmd(&["tag", loaded_name.trim(), &tag]);
        }
    } else {
        // Fallback: import raw ext4 (Linux only — needs to be mountable)
        ui::info("No OCI image found, importing rootfs.ext4...");
        #[cfg(target_os = "linux")]
        {
            let output = std::process::Command::new("docker")
                .args(["import", rootfs_dir, &tag])
                .output()
                .map_err(|e| anyhow::anyhow!("docker import: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("docker import failed: {stderr}");
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!(
                "No image.tar.gz found. Rebuild with a Nix flake to produce the OCI image.\n\
                 Raw ext4 import is only supported on Linux."
            );
        }
    }

    Ok(tag)
}

/// Docker backend implementation.
pub struct DockerBackend;

impl VmBackend for DockerBackend {
    fn name(&self) -> &str {
        "docker"
    }

    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            pause_resume: true,
            snapshots: false,
            vsock: false,
            tap_networking: false,
        }
    }

    fn start(&self, config: &VmStartConfig) -> Result<VmId> {
        let name = container_name(&config.name);

        // Load image
        let tag = load_image(&config.rootfs_path, &config.name)?;

        // Build docker run command
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--cpus".into(),
            config.cpus.to_string(),
            "--memory".into(),
            format!("{}m", config.memory_mib),
            "--label".into(),
            MVM_LABEL.into(),
        ];

        // Port mappings
        for port in &config.ports {
            args.push("-p".into());
            args.push(format!("{}:{}", port.host, port.guest));
        }

        // Volumes
        for vol in &config.volumes {
            args.push("-v".into());
            args.push(format!("{}:{}", vol.host, vol.guest));
        }

        // Metadata labels
        if !config.revision_hash.is_empty() {
            args.push("--label".into());
            args.push(format!("mvm.revision={}", config.revision_hash));
        }
        if !config.flake_ref.is_empty() {
            args.push("--label".into());
            args.push(format!("mvm.flake-ref={}", config.flake_ref));
        }
        if let Some(ref profile) = config.profile {
            args.push("--label".into());
            args.push(format!("mvm.profile={profile}"));
        }

        // Config files — write to temp dir and mount
        if !config.config_files.is_empty() {
            let config_dir = format!(
                "{}/.mvm/vms/{}/config",
                std::env::var("HOME").unwrap_or_default(),
                config.name
            );
            std::fs::create_dir_all(&config_dir)?;
            for f in &config.config_files {
                std::fs::write(format!("{}/{}", config_dir, f.name), &f.content)?;
            }
            args.push("-v".into());
            args.push(format!("{config_dir}:/mnt/config:ro"));
        }

        // Secret files
        if !config.secret_files.is_empty() {
            let secrets_dir = format!(
                "{}/.mvm/vms/{}/secrets",
                std::env::var("HOME").unwrap_or_default(),
                config.name
            );
            std::fs::create_dir_all(&secrets_dir)?;
            for f in &config.secret_files {
                let path = format!("{}/{}", secrets_dir, f.name);
                std::fs::write(&path, &f.content)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(f.mode))?;
                }
            }
            args.push("-v".into());
            args.push(format!("{secrets_dir}:/mnt/secrets:ro"));
        }

        // Image and command
        args.push(tag);
        args.push("/init".into());

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        docker_stdout(&args_refs)?;

        ui::success(&format!("Docker container '{}' started.", config.name));
        Ok(VmId(config.name.clone()))
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        let name = container_name(&id.0);
        let _ = docker_cmd(&["stop", &name]);
        let _ = docker_cmd(&["rm", "-f", &name]);
        Ok(())
    }

    fn stop_all(&self) -> Result<()> {
        let output = docker_stdout(&["ps", "-q", "--filter", &format!("label={MVM_LABEL}")]);
        if let Ok(ids) = output {
            for id in ids.lines() {
                if !id.is_empty() {
                    let _ = docker_cmd(&["stop", id]);
                    let _ = docker_cmd(&["rm", "-f", id]);
                }
            }
        }
        Ok(())
    }

    fn status(&self, id: &VmId) -> Result<VmStatus> {
        let name = container_name(&id.0);
        let status = docker_stdout(&["inspect", "--format", "{{.State.Status}}", &name])?;
        Ok(match status.as_str() {
            "running" => VmStatus::Running,
            "paused" => VmStatus::Paused,
            "created" | "restarting" => VmStatus::Starting,
            _ => VmStatus::Stopped,
        })
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        let output = docker_stdout(&[
            "ps",
            "-a",
            "--filter",
            &format!("label={MVM_LABEL}"),
            "--format",
            "{{.Names}}\t{{.Status}}\t{{.Label \"mvm.profile\"}}\t{{.Label \"mvm.revision\"}}\t{{.Label \"mvm.flake-ref\"}}",
        ]);

        let lines = match output {
            Ok(s) => s,
            Err(_) => return Ok(vec![]),
        };

        let mut vms = Vec::new();
        for line in lines.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let name = parts[0].strip_prefix("mvm-").unwrap_or(parts[0]);
            let status_str = parts[1];
            let status = if status_str.starts_with("Up") {
                VmStatus::Running
            } else if status_str.starts_with("Paused") {
                VmStatus::Paused
            } else {
                VmStatus::Stopped
            };

            vms.push(VmInfo {
                id: VmId(name.to_string()),
                name: name.to_string(),
                status,
                guest_ip: None,
                cpus: 0,
                memory_mib: 0,
                profile: parts
                    .get(2)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
                revision: parts
                    .get(3)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
                flake_ref: parts
                    .get(4)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
            });
        }
        Ok(vms)
    }

    fn logs(&self, id: &VmId, lines: u32, _hypervisor: bool) -> Result<String> {
        let name = container_name(&id.0);
        docker_stdout(&["logs", "--tail", &lines.to_string(), &name])
    }

    fn is_available(&self) -> Result<bool> {
        Ok(mvm_core::platform::current().has_docker())
    }

    fn install(&self) -> Result<()> {
        ui::info(
            "Docker is required for the Docker backend.\n\
             Install from: https://docs.docker.com/get-docker/\n\
             - macOS: Docker Desktop\n\
             - Linux: Docker Engine (apt/yum) or Docker Desktop\n\
             - Windows: Docker Desktop (requires WSL2)",
        );
        Ok(())
    }

    fn network_info(&self, id: &VmId) -> Result<VmNetworkInfo> {
        let name = container_name(&id.0);
        let ip = docker_stdout(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            &name,
        ])?;
        let gateway = docker_stdout(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.Gateway}}{{end}}",
            &name,
        ])
        .unwrap_or_default();

        Ok(VmNetworkInfo {
            guest_ip: ip,
            gateway_ip: gateway,
            subnet_cidr: "172.17.0.0/16".to_string(),
        })
    }

    fn guest_channel_info(&self, id: &VmId) -> Result<GuestChannelInfo> {
        let home = std::env::var("HOME").unwrap_or_default();
        Ok(GuestChannelInfo::UnixSocket {
            path: std::path::PathBuf::from(format!("{home}/.mvm/vms/{}/agent/agent.sock", id.0)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_backend_name() {
        let backend = DockerBackend;
        assert_eq!(backend.name(), "docker");
    }

    #[test]
    fn test_docker_capabilities() {
        let backend = DockerBackend;
        let caps = backend.capabilities();
        assert!(caps.pause_resume);
        assert!(!caps.snapshots);
        assert!(!caps.vsock);
        assert!(!caps.tap_networking);
    }

    #[test]
    fn test_container_name() {
        assert_eq!(container_name("hello"), "mvm-hello");
    }

    #[test]
    fn test_image_tag() {
        assert_eq!(image_tag("hello"), "mvm-hello:latest");
    }
}
