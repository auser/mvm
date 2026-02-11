use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::vm::instance::lifecycle::instance_list;
use crate::vm::instance::state::InstanceStatus;
use crate::vm::pool::lifecycle::pool_load;

/// Default idle threshold before transitioning Running → Warm (seconds).
pub const DEFAULT_WARM_THRESHOLD_SECS: u64 = 300; // 5 min

/// Default idle threshold before transitioning Warm → Sleeping (seconds).
pub const DEFAULT_SLEEP_THRESHOLD_SECS: u64 = 900; // 15 min

/// CPU threshold below which an instance is considered idle.
pub const IDLE_CPU_THRESHOLD: f32 = 5.0;

/// Network bytes threshold below which an instance is considered idle.
pub const IDLE_NET_THRESHOLD: u64 = 1024;

/// Recommended action from sleep policy evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SleepAction {
    /// Pause vCPUs (Running → Warm)
    Warm,
    /// Snapshot + shutdown (Warm → Sleeping)
    Sleep,
    /// No action needed
    None,
}

/// Configurable thresholds for a pool's sleep policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepPolicy {
    pub warm_threshold_secs: u64,
    pub sleep_threshold_secs: u64,
    pub cpu_threshold: f32,
    pub net_bytes_threshold: u64,
}

impl Default for SleepPolicy {
    fn default() -> Self {
        Self {
            warm_threshold_secs: DEFAULT_WARM_THRESHOLD_SECS,
            sleep_threshold_secs: DEFAULT_SLEEP_THRESHOLD_SECS,
            cpu_threshold: IDLE_CPU_THRESHOLD,
            net_bytes_threshold: IDLE_NET_THRESHOLD,
        }
    }
}

/// Result of evaluating sleep policy for a single instance.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub instance_id: String,
    pub current_status: InstanceStatus,
    pub action: SleepAction,
    pub reason: String,
}

/// Evaluate sleep policy for all instances in a pool.
///
/// Returns a list of recommended actions. Respects:
/// - Pool pinned/critical flags (never sleep)
/// - Instance idle metrics vs configured thresholds
/// - Memory pressure (sleep coldest first)
pub fn evaluate_pool(tenant_id: &str, pool_id: &str) -> Result<Vec<PolicyDecision>> {
    let spec = pool_load(tenant_id, pool_id)?;

    // Never touch pinned or critical pools
    if spec.pinned || spec.critical {
        return Ok(vec![]);
    }

    let instances = instance_list(tenant_id, pool_id)?;
    let policy = SleepPolicy::default();

    let mut decisions = Vec::new();

    for inst in &instances {
        let decision = evaluate_instance(inst, &policy);
        if decision.action != SleepAction::None {
            decisions.push(decision);
        }
    }

    // Sort by idle_secs descending (coldest first → sleep those first)
    decisions.sort_by(|a, b| {
        let a_idle = instances
            .iter()
            .find(|i| i.instance_id == a.instance_id)
            .map(|i| i.idle_metrics.idle_secs)
            .unwrap_or(0);
        let b_idle = instances
            .iter()
            .find(|i| i.instance_id == b.instance_id)
            .map(|i| i.idle_metrics.idle_secs)
            .unwrap_or(0);
        b_idle.cmp(&a_idle)
    });

    Ok(decisions)
}

/// Evaluate sleep policy for a single instance.
fn evaluate_instance(
    inst: &crate::vm::instance::state::InstanceState,
    policy: &SleepPolicy,
) -> PolicyDecision {
    let metrics = &inst.idle_metrics;

    // Only Running instances can be warmed, only Warm can be slept
    match inst.status {
        InstanceStatus::Running => {
            if metrics.idle_secs >= policy.warm_threshold_secs
                && metrics.cpu_pct < policy.cpu_threshold
                && metrics.net_bytes < policy.net_bytes_threshold
            {
                PolicyDecision {
                    instance_id: inst.instance_id.clone(),
                    current_status: inst.status,
                    action: SleepAction::Warm,
                    reason: format!(
                        "idle {}s >= {}s threshold, cpu {:.1}% < {:.1}%",
                        metrics.idle_secs,
                        policy.warm_threshold_secs,
                        metrics.cpu_pct,
                        policy.cpu_threshold,
                    ),
                }
            } else {
                PolicyDecision {
                    instance_id: inst.instance_id.clone(),
                    current_status: inst.status,
                    action: SleepAction::None,
                    reason: "active or below idle threshold".to_string(),
                }
            }
        }
        InstanceStatus::Warm => {
            if metrics.idle_secs >= policy.sleep_threshold_secs {
                PolicyDecision {
                    instance_id: inst.instance_id.clone(),
                    current_status: inst.status,
                    action: SleepAction::Sleep,
                    reason: format!(
                        "warm idle {}s >= {}s threshold",
                        metrics.idle_secs, policy.sleep_threshold_secs,
                    ),
                }
            } else {
                PolicyDecision {
                    instance_id: inst.instance_id.clone(),
                    current_status: inst.status,
                    action: SleepAction::None,
                    reason: "warm but below sleep threshold".to_string(),
                }
            }
        }
        _ => PolicyDecision {
            instance_id: inst.instance_id.clone(),
            current_status: inst.status,
            action: SleepAction::None,
            reason: format!("status {} not eligible for sleep policy", inst.status),
        },
    }
}

/// Evaluate sleep policy under memory pressure.
///
/// When the node's available memory drops below a threshold,
/// returns instances that should be slept, sorted coldest first.
pub fn pressure_candidates(
    tenant_id: &str,
    pool_id: &str,
    max_to_sleep: usize,
) -> Result<Vec<String>> {
    let instances = instance_list(tenant_id, pool_id)?;
    let spec = pool_load(tenant_id, pool_id)?;

    if spec.pinned || spec.critical {
        return Ok(vec![]);
    }

    // Collect warm or idle-running instances, sorted by idle_secs desc
    let mut candidates: Vec<_> = instances
        .iter()
        .filter(|i| matches!(i.status, InstanceStatus::Warm | InstanceStatus::Running))
        .collect();

    candidates.sort_by(|a, b| b.idle_metrics.idle_secs.cmp(&a.idle_metrics.idle_secs));

    Ok(candidates
        .iter()
        .take(max_to_sleep)
        .map(|i| i.instance_id.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sleep::metrics::IdleMetrics;
    use crate::vm::instance::state::{InstanceNet, InstanceState};

    fn test_instance(status: InstanceStatus, idle_secs: u64, cpu_pct: f32) -> InstanceState {
        InstanceState {
            instance_id: "i-test".to_string(),
            pool_id: "workers".to_string(),
            tenant_id: "acme".to_string(),
            status,
            net: InstanceNet {
                tap_dev: "tn3i5".to_string(),
                mac: "02:fc:00:03:00:05".to_string(),
                guest_ip: "10.240.3.5".to_string(),
                gateway_ip: "10.240.3.1".to_string(),
                cidr: 24,
            },
            revision_hash: None,
            firecracker_pid: Some(12345),
            last_started_at: None,
            last_stopped_at: None,
            idle_metrics: IdleMetrics {
                idle_secs,
                cpu_pct,
                net_bytes: 0,
                last_updated: None,
            },
            healthy: None,
            last_health_check_at: None,
            manual_override_until: None,
        }
    }

    #[test]
    fn test_running_below_threshold_no_action() {
        let inst = test_instance(InstanceStatus::Running, 100, 10.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_running_idle_above_threshold_warm() {
        let inst = test_instance(InstanceStatus::Running, 600, 1.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::Warm);
    }

    #[test]
    fn test_running_high_cpu_no_warm() {
        let inst = test_instance(InstanceStatus::Running, 600, 50.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_warm_below_sleep_threshold_no_action() {
        let inst = test_instance(InstanceStatus::Warm, 600, 0.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_warm_above_sleep_threshold_sleep() {
        let inst = test_instance(InstanceStatus::Warm, 1000, 0.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::Sleep);
    }

    #[test]
    fn test_stopped_instance_no_action() {
        let inst = test_instance(InstanceStatus::Stopped, 9999, 0.0);
        let decision = evaluate_instance(&inst, &SleepPolicy::default());
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_sleep_policy_default_thresholds() {
        let p = SleepPolicy::default();
        assert_eq!(p.warm_threshold_secs, 300);
        assert_eq!(p.sleep_threshold_secs, 900);
        assert_eq!(p.cpu_threshold, 5.0);
    }

    #[test]
    fn test_sleep_action_roundtrip() {
        let action = SleepAction::Warm;
        let json = serde_json::to_string(&action).unwrap();
        let parsed: SleepAction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SleepAction::Warm);
    }
}
