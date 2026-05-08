use anyhow::{Context, Result};

pub use mvm_core::idle_metrics::IdleMetrics;

use crate::shell;
use mvm_core::time;

/// Collect current idle metrics for a running instance.
///
/// Queries the Firecracker metrics endpoint and cgroup stats to compute
/// CPU usage, network activity, and idle duration.
pub fn collect_metrics(instance_dir: &str, socket_path: &str) -> Result<IdleMetrics> {
    // Read Firecracker metrics via API
    let metrics_json = shell::run_in_vm_stdout(&format!(
        r#"curl -s --unix-socket {socket} 'http://localhost/machine-config' 2>/dev/null || echo '{{}}'"#,
        socket = socket_path,
    ))?;

    // Read cgroup CPU stats if available
    let cpu_pct = read_cpu_usage(instance_dir).unwrap_or(0.0);

    // Read network bytes from FC metrics
    let net_bytes = read_net_bytes(socket_path).unwrap_or(0);

    // Compute idle_secs based on CPU and net activity
    let idle_secs = estimate_idle_secs(cpu_pct, net_bytes, &metrics_json);

    Ok(IdleMetrics {
        idle_secs,
        cpu_pct,
        net_bytes,
        last_updated: Some(time::utc_now()),
    })
}

/// Read CPU usage from Firecracker's metrics endpoint.
///
/// Returns CPU percentage (0-100) based on vCPU usage metrics.
fn read_cpu_usage(instance_dir: &str) -> Result<f32> {
    // Read from FC metrics FIFO if available
    let metrics_path = format!("{}/runtime/metrics.fifo", instance_dir);
    let output = shell::run_in_vm_stdout(&format!(
        r#"
        if [ -p {path} ]; then
            timeout 1 cat {path} 2>/dev/null | tail -1
        else
            echo '{{}}'
        fi
        "#,
        path = metrics_path,
    ))?;

    // Parse CPU utilization from FC metrics JSON
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&output)
        && let Some(vcpu) = val.get("vcpu")
        && let Some(exit_count) = vcpu.get("exit_io_out").and_then(|v| v.as_f64())
    {
        // Rough heuristic: high IO exit count → high CPU usage
        let pct = (exit_count / 1000.0).min(100.0) as f32;
        return Ok(pct);
    }

    Ok(0.0)
}

/// Read network bytes from Firecracker's metrics.
fn read_net_bytes(socket_path: &str) -> Result<u64> {
    let output = shell::run_in_vm_stdout(&format!(
        r#"curl -s --unix-socket {socket} 'http://localhost/metrics' 2>/dev/null || echo '{{}}'"#,
        socket = socket_path,
    ))?;

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&output)
        && let Some(net) = val.get("net")
    {
        let rx = net
            .get("rx_bytes_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let tx = net
            .get("tx_bytes_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        return Ok(rx + tx);
    }

    Ok(0)
}

/// Estimate idle seconds from activity metrics.
///
/// An instance is considered idle if CPU < 5% and no network activity.
fn estimate_idle_secs(cpu_pct: f32, net_bytes: u64, _raw_metrics: &str) -> u64 {
    if cpu_pct > 5.0 || net_bytes > 1024 {
        0 // Active — not idle
    } else if cpu_pct > 1.0 || net_bytes > 0 {
        60 // Low activity — slightly idle
    } else {
        300 // No activity — fully idle
    }
}

/// Update idle metrics for an instance by reading from the running VM.
///
/// Merges new metrics with the previous measurement to track idle duration
/// over time (idle_secs accumulates across polling intervals).
pub fn update_metrics(
    prev: &IdleMetrics,
    instance_dir: &str,
    socket_path: &str,
    poll_interval_secs: u64,
) -> Result<IdleMetrics> {
    let current =
        collect_metrics(instance_dir, socket_path).with_context(|| "Failed to collect metrics")?;

    // Accumulate idle time: if still idle, add the poll interval
    let idle_secs = if current.cpu_pct < 5.0 && current.net_bytes < 1024 {
        prev.idle_secs + poll_interval_secs
    } else {
        0 // Reset on activity
    };

    Ok(IdleMetrics {
        idle_secs,
        cpu_pct: current.cpu_pct,
        net_bytes: current.net_bytes,
        last_updated: current.last_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_metrics_default() {
        let m = IdleMetrics::default();
        assert_eq!(m.idle_secs, 0);
        assert_eq!(m.cpu_pct, 0.0);
        assert_eq!(m.net_bytes, 0);
        assert!(m.last_updated.is_none());
    }

    #[test]
    fn test_idle_metrics_roundtrip() {
        let m = IdleMetrics {
            idle_secs: 300,
            cpu_pct: 2.5,
            net_bytes: 4096,
            last_updated: Some("2025-01-01T00:00:00Z".to_string()),
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: IdleMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.idle_secs, 300);
        assert_eq!(parsed.cpu_pct, 2.5);
    }

    #[test]
    fn test_estimate_idle_secs_active() {
        assert_eq!(estimate_idle_secs(50.0, 10000, ""), 0);
    }

    #[test]
    fn test_estimate_idle_secs_low_activity() {
        assert_eq!(estimate_idle_secs(2.0, 100, ""), 60);
    }

    #[test]
    fn test_estimate_idle_secs_fully_idle() {
        assert_eq!(estimate_idle_secs(0.0, 0, ""), 300);
    }
}
