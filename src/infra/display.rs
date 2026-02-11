use serde::Serialize;
use tabled::Tabled;

/// Display row for `tenant list`.
#[derive(Debug, Serialize, Tabled)]
pub struct TenantRow {
    #[tabled(rename = "TENANT")]
    pub tenant_id: String,
    #[tabled(rename = "SUBNET")]
    pub subnet: String,
    #[tabled(rename = "BRIDGE")]
    pub bridge: String,
    #[tabled(rename = "MAX VCPUS")]
    pub max_vcpus: u32,
    #[tabled(rename = "MAX MEM")]
    pub max_mem_mib: u64,
}

/// Display row for `tenant info`.
#[derive(Debug, Serialize, Tabled)]
pub struct TenantInfo {
    #[tabled(rename = "TENANT")]
    pub tenant_id: String,
    #[tabled(rename = "SUBNET")]
    pub subnet: String,
    #[tabled(rename = "GATEWAY")]
    pub gateway: String,
    #[tabled(rename = "BRIDGE")]
    pub bridge: String,
    #[tabled(rename = "NET ID")]
    pub net_id: u16,
    #[tabled(rename = "MAX VCPUS")]
    pub max_vcpus: u32,
    #[tabled(rename = "MAX MEM")]
    pub max_mem_mib: u64,
    #[tabled(rename = "MAX RUNNING")]
    pub max_running: u32,
    #[tabled(rename = "MAX WARM")]
    pub max_warm: u32,
    #[tabled(rename = "CREATED")]
    pub created_at: String,
}

/// Display row for `pool list`.
#[derive(Debug, Serialize, Tabled)]
pub struct PoolRow {
    #[tabled(rename = "POOL")]
    pub pool_path: String,
    #[tabled(rename = "ROLE")]
    pub role: String,
    #[tabled(rename = "PROFILE")]
    pub profile: String,
    #[tabled(rename = "VCPUS")]
    pub vcpus: u8,
    #[tabled(rename = "MEM")]
    pub mem_mib: u32,
    #[tabled(rename = "RUNNING")]
    pub desired_running: u32,
    #[tabled(rename = "WARM")]
    pub desired_warm: u32,
    #[tabled(rename = "SLEEPING")]
    pub desired_sleeping: u32,
}

/// Display row for `pool info`.
#[derive(Debug, Serialize, Tabled)]
pub struct PoolInfo {
    #[tabled(rename = "POOL")]
    pub pool_path: String,
    #[tabled(rename = "ROLE")]
    pub role: String,
    #[tabled(rename = "FLAKE")]
    pub flake_ref: String,
    #[tabled(rename = "PROFILE")]
    pub profile: String,
    #[tabled(rename = "VCPUS")]
    pub vcpus: u8,
    #[tabled(rename = "MEM")]
    pub mem_mib: u32,
    #[tabled(rename = "DATA DISK")]
    pub data_disk_mib: u32,
    #[tabled(rename = "RUNNING")]
    pub desired_running: u32,
    #[tabled(rename = "WARM")]
    pub desired_warm: u32,
    #[tabled(rename = "SLEEPING")]
    pub desired_sleeping: u32,
    #[tabled(rename = "SECCOMP")]
    pub seccomp_policy: String,
}

/// Display row for `instance list`.
#[derive(Debug, Serialize, Tabled)]
pub struct InstanceRow {
    #[tabled(rename = "INSTANCE")]
    pub instance_path: String,
    #[tabled(rename = "STATUS")]
    pub status: String,
    #[tabled(rename = "IP")]
    pub guest_ip: String,
    #[tabled(rename = "TAP")]
    pub tap_dev: String,
    #[tabled(rename = "PID")]
    pub pid: String,
}

/// Display row for `instance stats`.
#[derive(Debug, Serialize, Tabled)]
pub struct InstanceInfo {
    #[tabled(rename = "INSTANCE")]
    pub instance_path: String,
    #[tabled(rename = "STATUS")]
    pub status: String,
    #[tabled(rename = "IP")]
    pub guest_ip: String,
    #[tabled(rename = "TAP")]
    pub tap_dev: String,
    #[tabled(rename = "MAC")]
    pub mac: String,
    #[tabled(rename = "PID")]
    pub pid: String,
    #[tabled(rename = "REVISION")]
    pub revision: String,
    #[tabled(rename = "STARTED")]
    pub last_started: String,
    #[tabled(rename = "STOPPED")]
    pub last_stopped: String,
}
