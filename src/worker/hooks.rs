use anyhow::{Context, Result};

use crate::infra::shell;
use crate::vm::tenant::config::tenant_ssh_key_path;

/// Guest worker lifecycle signal paths (inside the guest).
pub const WORKER_READY: &str = "/run/mvm/worker-ready";
pub const WORKER_IDLE: &str = "/run/mvm/worker-idle";
pub const WORKER_BUSY: &str = "/run/mvm/worker-busy";

/// Sleep prep service name inside the guest.
const SLEEP_PREP_SERVICE: &str = "mvm-sleep-prep";

/// Default timeout for sleep prep ACK (seconds).
pub const DEFAULT_SLEEP_PREP_TIMEOUT: u64 = 10;

/// Signal a guest to prepare for sleep.
///
/// Triggers the `mvm-sleep-prep` systemd service inside the guest via SSH.
/// The service should: drop page cache, compact memory, park threads.
///
/// Returns Ok(true) if the guest ACKed (service completed),
/// Ok(false) if timed out or guest unreachable.
pub fn signal_sleep_prep(tenant_id: &str, guest_ip: &str, timeout_secs: u64) -> Result<bool> {
    let ssh_key = tenant_ssh_key_path(tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        r#"timeout {timeout} ssh -o StrictHostKeyChecking=no \
            -o ConnectTimeout=3 -o LogLevel=ERROR \
            -i {key} root@{ip} \
            'systemctl start {service} 2>/dev/null && echo ACK || echo NACK' \
            2>/dev/null || echo TIMEOUT"#,
        timeout = timeout_secs,
        key = ssh_key,
        ip = guest_ip,
        service = SLEEP_PREP_SERVICE,
    ))
    .with_context(|| format!("Failed to signal sleep prep to {}", guest_ip))?;

    Ok(output.trim() == "ACK")
}

/// Check if a guest worker has signaled readiness.
///
/// Looks for the WORKER_READY signal file inside the guest.
pub fn is_worker_ready(tenant_id: &str, guest_ip: &str) -> Result<bool> {
    let ssh_key = tenant_ssh_key_path(tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        r#"timeout 3 ssh -o StrictHostKeyChecking=no \
            -o ConnectTimeout=2 -o LogLevel=ERROR \
            -i {key} root@{ip} \
            'test -f {path} && echo yes || echo no' \
            2>/dev/null || echo no"#,
        key = ssh_key,
        ip = guest_ip,
        path = WORKER_READY,
    ))?;

    Ok(output.trim() == "yes")
}

/// Check the current worker signal state.
///
/// Returns the most recent signal: "ready", "idle", "busy", or "unknown".
pub fn worker_status(tenant_id: &str, guest_ip: &str) -> Result<String> {
    let ssh_key = tenant_ssh_key_path(tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        r#"timeout 3 ssh -o StrictHostKeyChecking=no \
            -o ConnectTimeout=2 -o LogLevel=ERROR \
            -i {key} root@{ip} \
            'if [ -f {busy} ]; then echo busy; \
             elif [ -f {idle} ]; then echo idle; \
             elif [ -f {ready} ]; then echo ready; \
             else echo unknown; fi' \
            2>/dev/null || echo unknown"#,
        key = ssh_key,
        ip = guest_ip,
        ready = WORKER_READY,
        idle = WORKER_IDLE,
        busy = WORKER_BUSY,
    ))?;

    Ok(output.trim().to_string())
}

/// Send a wakeup signal to a guest that was just restored from snapshot.
///
/// Triggers the `mvm-wake` service which should reinitialize connections,
/// refresh secrets, and signal worker-ready.
pub fn signal_wake(tenant_id: &str, guest_ip: &str) -> Result<bool> {
    let ssh_key = tenant_ssh_key_path(tenant_id);

    let output = shell::run_in_vm_stdout(&format!(
        r#"timeout 10 ssh -o StrictHostKeyChecking=no \
            -o ConnectTimeout=5 -o LogLevel=ERROR \
            -i {key} root@{ip} \
            'systemctl start mvm-wake 2>/dev/null && echo ACK || echo NACK' \
            2>/dev/null || echo TIMEOUT"#,
        key = ssh_key,
        ip = guest_ip,
    ))?;

    Ok(output.trim() == "ACK")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_paths() {
        assert_eq!(WORKER_READY, "/run/mvm/worker-ready");
        assert_eq!(WORKER_IDLE, "/run/mvm/worker-idle");
        assert_eq!(WORKER_BUSY, "/run/mvm/worker-busy");
    }

    #[test]
    fn test_sleep_prep_timeout() {
        assert_eq!(DEFAULT_SLEEP_PREP_TIMEOUT, 10);
    }

    #[test]
    fn test_sleep_prep_service_name() {
        assert_eq!(SLEEP_PREP_SERVICE, "mvm-sleep-prep");
    }
}
