use mvm_core::config::{ARCH, fc_version};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

pub const VM_NAME: &str = "mvm";
pub const API_SOCKET: &str = "/tmp/firecracker.socket";
pub const TAP_DEV: &str = "tap0";
pub const TAP_IP: &str = "172.16.0.1";
pub const MASK_SHORT: &str = "/30";
pub const GUEST_IP: &str = "172.16.0.2";
pub const FC_MAC: &str = "06:00:AC:10:00:02";
/// Non-root user inside the Firecracker guest VM (dev mode).
pub const GUEST_USER: &str = "mvm";
/// Path inside the Lima VM (~ expands to the VM user's home)
pub const MICROVM_DIR: &str = "~/microvm";
pub const LOGFILE: &str = "~/microvm/firecracker.log";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MvmState {
    pub kernel: String,
    pub rootfs: String,
    pub ssh_key: String,
    #[serde(default)]
    pub fc_pid: Option<u32>,
}

/// Run mode info persisted at `~/microvm/.mvm-run-info` so `status` can
/// distinguish dev-mode VMs from flake-built VMs.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RunInfo {
    /// "dev" or "flake"
    pub mode: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub flake_ref: Option<String>,
    pub guest_user: String,
    pub cpus: u32,
    pub memory: u32,
}

/// Find the lima.yaml.tera template file.
/// Looks in: 1) resources/ next to the binary, 2) source tree, 3) sibling project
pub(crate) fn find_lima_template() -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();

    // Check next to binary
    let candidate = exe_dir.join("resources").join("lima.yaml.tera");
    if candidate.exists() {
        return Ok(candidate);
    }

    // Check in the source tree (development mode)
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join("resources").join("lima.yaml.tera");
    if candidate.exists() {
        return Ok(candidate);
    }

    // Check workspace root (crate is at crates/mvm-runtime/)
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let candidate = workspace_root.join("resources").join("lima.yaml.tera");
    if candidate.exists() {
        return Ok(candidate);
    }

    // Check sibling project (plain yaml fallback)
    let candidate = workspace_root
        .parent()
        .unwrap()
        .join("firecracker-lima-vm")
        .join("lima.yaml");
    if candidate.exists() {
        return Ok(candidate);
    }

    anyhow::bail!(
        "Cannot find lima.yaml.tera. Place it in resources/ or ensure ../firecracker-lima-vm/lima.yaml exists."
    )
}

/// Options for customizing Lima template rendering.
#[derive(Debug, Default)]
pub struct LimaRenderOptions {
    /// Path to a custom lima.yaml.tera template. If `None`, the bundled template is used.
    pub template_path: Option<PathBuf>,
    /// Extra Tera context variables injected alongside the built-in ones.
    /// These take precedence over built-in values if keys collide.
    pub extra_context: std::collections::HashMap<String, String>,
    /// Number of vCPUs for the Lima VM.
    pub cpus: Option<u32>,
    /// Memory in GiB for the Lima VM.
    pub memory_gib: Option<u32>,
}

/// Render the Lima YAML template with config values and return a temp file.
/// The caller must hold the returned NamedTempFile until limactl has read it.
pub fn render_lima_yaml() -> anyhow::Result<tempfile::NamedTempFile> {
    render_lima_yaml_with(&LimaRenderOptions::default())
}

/// Render the Lima YAML template with custom options.
pub fn render_lima_yaml_with(opts: &LimaRenderOptions) -> anyhow::Result<tempfile::NamedTempFile> {
    let template_path = match &opts.template_path {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("Custom Lima template not found: {}", p.display());
            }
            p.clone()
        }
        None => find_lima_template()?,
    };

    let template_str = std::fs::read_to_string(&template_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", template_path.display(), e))?;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("lima.yaml", &template_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse Lima template: {}", e))?;

    let mut ctx = tera::Context::new();
    ctx.insert("vm_name", VM_NAME);
    ctx.insert("fc_version", &fc_version());
    ctx.insert("arch", ARCH);
    ctx.insert("tap_ip", TAP_IP);
    ctx.insert("guest_ip", GUEST_IP);
    ctx.insert("microvm_dir", MICROVM_DIR);

    if let Some(cpus) = opts.cpus {
        ctx.insert("lima_cpus", &cpus);
    }
    if let Some(mem) = opts.memory_gib {
        ctx.insert("lima_memory", &mem);
    }

    for (key, value) in &opts.extra_context {
        ctx.insert(key, value);
    }

    let rendered = tera
        .render("lima.yaml", &ctx)
        .map_err(|e| anyhow::anyhow!("Failed to render Lima template: {}", e))?;

    let mut tmp = tempfile::Builder::new()
        .prefix("mvm-lima-")
        .suffix(".yaml")
        .tempfile()
        .map_err(|e| anyhow::anyhow!("Failed to create temp file: {}", e))?;

    tmp.write_all(rendered.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to write rendered yaml: {}", e))?;

    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn test_constants_non_empty() {
        assert!(!VM_NAME.is_empty());
        assert!(!mvm_core::config::fc_version().is_empty());
        assert!(!mvm_core::config::ARCH.is_empty());
        assert!(!API_SOCKET.is_empty());
        assert!(!TAP_DEV.is_empty());
        assert!(!TAP_IP.is_empty());
        assert!(!GUEST_IP.is_empty());
        assert!(!FC_MAC.is_empty());
    }

    #[test]
    fn test_fc_version_starts_with_v() {
        assert!(
            mvm_core::config::fc_version().starts_with('v'),
            "FC_VERSION should start with 'v'"
        );
    }

    #[test]
    fn test_ip_addresses_are_in_same_subnet() {
        // TAP_IP and GUEST_IP should share the 172.16.0.x prefix
        assert!(TAP_IP.starts_with("172.16.0."));
        assert!(GUEST_IP.starts_with("172.16.0."));
    }

    #[test]
    fn test_mvm_state_json_roundtrip() {
        let state = MvmState {
            kernel: "vmlinux-5.10.217".to_string(),
            rootfs: "ubuntu-24.04.ext4".to_string(),
            ssh_key: "ubuntu-24.04.id_rsa".to_string(),
            fc_pid: Some(12345),
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: MvmState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.kernel, "vmlinux-5.10.217");
        assert_eq!(parsed.rootfs, "ubuntu-24.04.ext4");
        assert_eq!(parsed.ssh_key, "ubuntu-24.04.id_rsa");
        assert_eq!(parsed.fc_pid, Some(12345));
    }

    #[test]
    fn test_mvm_state_json_without_pid() {
        let json = r#"{"kernel":"k","rootfs":"r","ssh_key":"s"}"#;
        let state: MvmState = serde_json::from_str(json).unwrap();
        assert_eq!(state.fc_pid, None);
    }

    #[test]
    fn test_mvm_state_default() {
        let state = MvmState::default();
        assert!(state.kernel.is_empty());
        assert!(state.rootfs.is_empty());
        assert!(state.ssh_key.is_empty());
        assert_eq!(state.fc_pid, None);
    }

    #[test]
    fn test_run_info_json_roundtrip() {
        let info = RunInfo {
            mode: "flake".to_string(),
            revision: Some("abc123".to_string()),
            flake_ref: Some("/home/user/project".to_string()),
            guest_user: "root".to_string(),
            cpus: 4,
            memory: 2048,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: RunInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, "flake");
        assert_eq!(parsed.revision.as_deref(), Some("abc123"));
        assert_eq!(parsed.flake_ref.as_deref(), Some("/home/user/project"));
        assert_eq!(parsed.guest_user, "root");
        assert_eq!(parsed.cpus, 4);
        assert_eq!(parsed.memory, 2048);
    }

    #[test]
    fn test_run_info_default() {
        let info = RunInfo::default();
        assert!(info.mode.is_empty());
        assert!(info.revision.is_none());
        assert!(info.flake_ref.is_none());
        assert!(info.guest_user.is_empty());
        assert_eq!(info.cpus, 0);
        assert_eq!(info.memory, 0);
    }

    #[test]
    fn test_run_info_minimal_json() {
        let json = r#"{"mode":"dev","guest_user":"mvm","cpus":2,"memory":1024}"#;
        let info: RunInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.mode, "dev");
        assert!(info.revision.is_none());
        assert!(info.flake_ref.is_none());
    }

    #[test]
    fn test_production_mode_disabled_by_default() {
        // Without env var set, should be false
        unsafe { std::env::remove_var("MVM_PRODUCTION") };
        assert!(!mvm_core::config::is_production_mode());
    }

    #[test]
    fn test_find_lima_template_succeeds() {
        // Should find resources/lima.yaml.tera in the source tree
        let path = find_lima_template().unwrap();
        assert!(path.exists());
        assert!(path.to_str().unwrap().contains("lima.yaml"));
    }

    #[test]
    fn test_render_lima_yaml_produces_valid_output() {
        let tmp = render_lima_yaml().unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        // Should contain Lima YAML structure
        assert!(content.contains("nestedVirtualization: true"));
        assert!(content.contains("writable: true"));

        // Lima's own template variable should be preserved (raw block unwrapped)
        assert!(content.contains("{{.User}}"));

        // Tera tags should NOT appear in output
        assert!(!content.contains("{% raw %}"));
        assert!(!content.contains("{% endraw %}"));
    }

    #[test]
    fn test_render_lima_yaml_temp_file_has_yaml_suffix() {
        let tmp = render_lima_yaml().unwrap();
        let path_str = tmp.path().to_str().unwrap();
        assert!(path_str.ends_with(".yaml"));
        assert!(path_str.contains("mvm-lima-"));
    }

    #[test]
    fn test_render_with_extra_context() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("vm_name".to_string(), "custom-vm".to_string());
        let opts = LimaRenderOptions {
            extra_context: extra,
            ..Default::default()
        };
        // Should succeed — extra context overrides vm_name but template
        // doesn't directly embed it in a visible way. Just verify no error.
        let tmp = render_lima_yaml_with(&opts).unwrap();
        assert!(tmp.path().exists());
    }

    #[test]
    fn test_render_with_custom_template() {
        let mut custom = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut custom, b"custom: {{ vm_name }}").unwrap();

        let opts = LimaRenderOptions {
            template_path: Some(custom.path().to_path_buf()),
            ..Default::default()
        };
        let tmp = render_lima_yaml_with(&opts).unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "custom: mvm");
    }

    #[test]
    fn test_render_with_missing_custom_template_fails() {
        let opts = LimaRenderOptions {
            template_path: Some(PathBuf::from("/nonexistent/template.tera")),
            ..Default::default()
        };
        assert!(render_lima_yaml_with(&opts).is_err());
    }

    #[test]
    fn test_render_lima_yaml_includes_nix_profile() {
        let tmp = render_lima_yaml().unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("mvm-nix.sh"),
            "Lima template should install Nix profile.d script"
        );
        assert!(
            content.contains("nix-daemon.sh"),
            "Lima template should source nix-daemon.sh"
        );
    }

    #[test]
    fn test_render_lima_yaml_includes_mvm_tools_profile() {
        let tmp = render_lima_yaml().unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("mvm-tools.sh"),
            "Lima template should install mvm-tools profile.d script"
        );
        assert!(
            content.contains("MVM_FC_VERSION"),
            "Lima template should export MVM_FC_VERSION"
        );
    }

    #[test]
    fn test_render_with_lima_resources() {
        let opts = LimaRenderOptions {
            cpus: Some(8),
            memory_gib: Some(16),
            ..Default::default()
        };
        let tmp = render_lima_yaml_with(&opts).unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("cpus: 8"),
            "Rendered YAML should contain cpus: 8, got:\n{}",
            content
        );
        assert!(
            content.contains(r#"memory: "16GiB""#),
            "Rendered YAML should contain memory: \"16GiB\", got:\n{}",
            content
        );
    }

    #[test]
    fn test_render_without_lima_resources_omits_fields() {
        let tmp = render_lima_yaml().unwrap();
        let mut content = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            !content.contains("cpus:"),
            "Default render should not contain cpus field"
        );
        assert!(
            !content.contains("memory:"),
            "Default render should not contain memory field"
        );
    }
}
