use anyhow::Result;
use serde::Serialize;

use super::state::InstanceNet;
use crate::vm::pool::config::InstanceResources;

/// Firecracker VM configuration for an instance.
#[derive(Debug, Serialize)]
pub struct FcConfig {
    #[serde(rename = "boot-source")]
    pub boot_source: BootSource,
    pub drives: Vec<Drive>,
    #[serde(rename = "network-interfaces")]
    pub network_interfaces: Vec<NetworkInterface>,
    #[serde(rename = "machine-config")]
    pub machine_config: MachineConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock: Option<VsockDevice>,
}

/// Firecracker vsock device configuration.
#[derive(Debug, Serialize)]
pub struct VsockDevice {
    pub vsock_id: String,
    pub guest_cid: u32,
    pub uds_path: String,
}

#[derive(Debug, Serialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    pub boot_args: String,
}

#[derive(Debug, Serialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Debug, Serialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub guest_mac: String,
    pub host_dev_name: String,
}

#[derive(Debug, Serialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
}

/// Generate a Firecracker JSON config for an instance.
///
/// Boot args include kernel console settings and static IP configuration
/// so the guest comes up with the correct network identity.
///
/// Drive model:
/// - rootfs: immutable root device (read-only)
/// - config: instance/pool metadata (read-only, refreshed on start/wake)
/// - data: persistent writable volume (optional, may be LUKS-encrypted)
/// - secrets: ephemeral sensitive material (read-only, tmpfs-backed)
#[allow(clippy::too_many_arguments)]
pub fn generate(
    resources: &InstanceResources,
    net: &InstanceNet,
    kernel_path: &str,
    rootfs_path: &str,
    config_disk_path: Option<&str>,
    data_disk_path: Option<&str>,
    secrets_disk_path: Option<&str>,
    vsock_uds_path: Option<&str>,
) -> Result<String> {
    let mut drives = vec![Drive {
        drive_id: "rootfs".to_string(),
        path_on_host: rootfs_path.to_string(),
        is_root_device: true,
        is_read_only: true,
    }];

    if let Some(config_path) = config_disk_path {
        drives.push(Drive {
            drive_id: "config".to_string(),
            path_on_host: config_path.to_string(),
            is_root_device: false,
            is_read_only: true,
        });
    }

    if let Some(data_path) = data_disk_path {
        drives.push(Drive {
            drive_id: "data".to_string(),
            path_on_host: data_path.to_string(),
            is_root_device: false,
            is_read_only: false,
        });
    }

    if let Some(secrets_path) = secrets_disk_path {
        drives.push(Drive {
            drive_id: "secrets".to_string(),
            path_on_host: secrets_path.to_string(),
            is_root_device: false,
            is_read_only: true,
        });
    }

    let vsock = vsock_uds_path.map(|path| VsockDevice {
        vsock_id: "vsock0".to_string(),
        guest_cid: crate::worker::vsock::GUEST_CID,
        uds_path: path.to_string(),
    });

    // Compute subnet mask from CIDR for boot args
    let mask = cidr_to_mask(net.cidr);

    let config = FcConfig {
        boot_source: BootSource {
            kernel_image_path: kernel_path.to_string(),
            boot_args: format!(
                "keep_bootcon console=ttyS0 reboot=k panic=1 pci=off \
                 ip={}::{}:{}::eth0:off",
                net.guest_ip, net.gateway_ip, mask,
            ),
        },
        drives,
        network_interfaces: vec![NetworkInterface {
            iface_id: "net1".to_string(),
            guest_mac: net.mac.clone(),
            host_dev_name: net.tap_dev.clone(),
        }],
        machine_config: MachineConfig {
            vcpu_count: resources.vcpus,
            mem_size_mib: resources.mem_mib,
        },
        vsock,
    };

    Ok(serde_json::to_string_pretty(&config)?)
}

/// Convert CIDR prefix length to dotted-decimal subnet mask.
fn cidr_to_mask(cidr: u8) -> String {
    let mask: u32 = if cidr == 0 {
        0
    } else if cidr >= 32 {
        0xFFFF_FFFF
    } else {
        !((1u32 << (32 - cidr)) - 1)
    };
    format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xFF,
        (mask >> 16) & 0xFF,
        (mask >> 8) & 0xFF,
        mask & 0xFF,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_basic() {
        let resources = InstanceResources {
            vcpus: 2,
            mem_mib: 1024,
            data_disk_mib: 0,
        };
        let net = InstanceNet {
            tap_dev: "tn3i5".to_string(),
            mac: "02:fc:00:03:00:05".to_string(),
            guest_ip: "10.240.3.5".to_string(),
            gateway_ip: "10.240.3.1".to_string(),
            cidr: 24,
        };

        let json = generate(
            &resources,
            &net,
            "/path/vmlinux",
            "/path/rootfs.ext4",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["machine-config"]["vcpu_count"], 2);
        assert_eq!(parsed["machine-config"]["mem_size_mib"], 1024);
        assert_eq!(parsed["network-interfaces"][0]["host_dev_name"], "tn3i5");
        assert!(parsed.get("vsock").is_none());

        let boot_args = parsed["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot_args.contains("ip=10.240.3.5::10.240.3.1:255.255.255.0::eth0:off"));

        // rootfs should be read-only
        assert_eq!(parsed["drives"][0]["is_read_only"], true);
    }

    #[test]
    fn test_generate_with_all_drives() {
        let resources = InstanceResources {
            vcpus: 1,
            mem_mib: 512,
            data_disk_mib: 2048,
        };
        let net = InstanceNet {
            tap_dev: "tn3i5".to_string(),
            mac: "02:fc:00:03:00:05".to_string(),
            guest_ip: "10.240.3.5".to_string(),
            gateway_ip: "10.240.3.1".to_string(),
            cidr: 24,
        };

        let json = generate(
            &resources,
            &net,
            "/k",
            "/r",
            Some("/c"),
            Some("/d"),
            Some("/s"),
            None,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let drives = parsed["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 4);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[0]["is_read_only"], true);
        assert_eq!(drives[1]["drive_id"], "config");
        assert_eq!(drives[1]["is_read_only"], true);
        assert_eq!(drives[2]["drive_id"], "data");
        assert_eq!(drives[2]["is_read_only"], false);
        assert_eq!(drives[3]["drive_id"], "secrets");
        assert_eq!(drives[3]["is_read_only"], true);
    }

    #[test]
    fn test_generate_with_vsock() {
        let resources = InstanceResources {
            vcpus: 1,
            mem_mib: 512,
            data_disk_mib: 0,
        };
        let net = InstanceNet {
            tap_dev: "tn3i5".to_string(),
            mac: "02:fc:00:03:00:05".to_string(),
            guest_ip: "10.240.3.5".to_string(),
            gateway_ip: "10.240.3.1".to_string(),
            cidr: 24,
        };

        let json = generate(
            &resources,
            &net,
            "/k",
            "/r",
            None,
            None,
            None,
            Some("/tmp/v.sock"),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["vsock"]["vsock_id"], "vsock0");
        assert_eq!(parsed["vsock"]["guest_cid"], 3);
        assert_eq!(parsed["vsock"]["uds_path"], "/tmp/v.sock");
    }

    #[test]
    fn test_cidr_to_mask() {
        assert_eq!(cidr_to_mask(24), "255.255.255.0");
        assert_eq!(cidr_to_mask(16), "255.255.0.0");
        assert_eq!(cidr_to_mask(8), "255.0.0.0");
        assert_eq!(cidr_to_mask(32), "255.255.255.255");
        assert_eq!(cidr_to_mask(0), "0.0.0.0");
        assert_eq!(cidr_to_mask(25), "255.255.255.128");
    }
}
