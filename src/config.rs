use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

pub const VM_NAME: &str = "mvm";
pub const FC_VERSION: &str = "v1.13.0";
pub const ARCH: &str = "aarch64";
pub const API_SOCKET: &str = "/tmp/firecracker.socket";
pub const TAP_DEV: &str = "tap0";
pub const TAP_IP: &str = "172.16.0.1";
pub const MASK_SHORT: &str = "/30";
pub const GUEST_IP: &str = "172.16.0.2";
pub const FC_MAC: &str = "06:00:AC:10:00:02";
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

/// Find the lima.yaml.tera template file.
/// Looks in: 1) resources/ next to the binary, 2) source tree, 3) sibling project
fn find_lima_template() -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .unwrap()
        .to_path_buf();

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

    // Check sibling project (plain yaml fallback)
    let candidate = manifest_dir
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

/// Render the Lima YAML template with config values and return a temp file.
/// The caller must hold the returned NamedTempFile until limactl has read it.
pub fn render_lima_yaml() -> anyhow::Result<tempfile::NamedTempFile> {
    let template_path = find_lima_template()?;
    let template_str = std::fs::read_to_string(&template_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", template_path.display(), e))?;

    let mut tera = tera::Tera::default();
    tera.add_raw_template("lima.yaml", &template_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse Lima template: {}", e))?;

    let mut ctx = tera::Context::new();
    ctx.insert("vm_name", VM_NAME);
    ctx.insert("fc_version", FC_VERSION);
    ctx.insert("arch", ARCH);
    ctx.insert("tap_ip", TAP_IP);
    ctx.insert("guest_ip", GUEST_IP);
    ctx.insert("microvm_dir", MICROVM_DIR);

    let rendered = tera.render("lima.yaml", &ctx)
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
