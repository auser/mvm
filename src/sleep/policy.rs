use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::infra::http;
use crate::vm::instance::lifecycle::instance_list;
use crate::vm::instance::state::{InstanceState, InstanceStatus};
use crate::vm::pool::config::RuntimePolicy;
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

// ============================================================================
// Minimum runtime eligibility check
// ============================================================================

/// Check whether an instance has satisfied its minimum runtime requirement
/// for the given transition.
///
/// Returns true if eligible (transition allowed), false if still within minimum runtime.
/// Enforced by the host agent using wall-clock timestamps — the guest is not involved.
pub fn is_eligible_for_transition(
    instance: &InstanceState,
    target: InstanceStatus,
    policy: &RuntimePolicy,
    now: &str,
) -> bool {
    match (instance.status, target) {
        // Running -> Warm or Running -> Stopped: check min_running_seconds
        (InstanceStatus::Running, InstanceStatus::Warm | InstanceStatus::Stopped) => {
            if policy.min_running_seconds == 0 {
                return true;
            }
            match &instance.entered_running_at {
                Some(entered) => elapsed_secs(entered, now) >= policy.min_running_seconds,
                None => true, // No timestamp = no constraint
            }
        }
        // Warm -> Sleeping: check min_warm_seconds
        (InstanceStatus::Warm, InstanceStatus::Sleeping) => {
            if policy.min_warm_seconds == 0 {
                return true;
            }
            match &instance.entered_warm_at {
                Some(entered) => elapsed_secs(entered, now) >= policy.min_warm_seconds,
                None => true,
            }
        }
        // All other transitions: no minimum runtime constraint
        _ => true,
    }
}

/// Compute elapsed seconds between two ISO timestamp strings.
fn elapsed_secs(from: &str, to: &str) -> u64 {
    let from_dt = from.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now());
    let to_dt = to.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now());
    to_dt.signed_duration_since(from_dt).num_seconds().max(0) as u64
}

// ============================================================================
// Sleep policy evaluation
// ============================================================================

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
/// - Minimum runtime policy (defer transitions if not yet satisfied)
pub fn evaluate_pool(tenant_id: &str, pool_id: &str) -> Result<Vec<PolicyDecision>> {
    let spec = pool_load(tenant_id, pool_id)?;

    // Never touch pinned or critical pools
    if spec.pinned || spec.critical {
        return Ok(vec![]);
    }

    let instances = instance_list(tenant_id, pool_id)?;
    let policy = SleepPolicy::default();
    let runtime_policy = &spec.runtime_policy;
    let now = http::utc_now();

    let mut decisions = Vec::new();

    for inst in &instances {
        let decision = evaluate_instance(inst, &policy, runtime_policy, &now);
        decisions.push(decision);
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

/// Evaluate sleep policy for a single instance, respecting minimum runtime.
fn evaluate_instance(
    inst: &InstanceState,
    policy: &SleepPolicy,
    runtime_policy: &RuntimePolicy,
    now: &str,
) -> PolicyDecision {
    let metrics = &inst.idle_metrics;

    // Only Running instances can be warmed, only Warm can be slept
    match inst.status {
        InstanceStatus::Running => {
            if metrics.idle_secs >= policy.warm_threshold_secs
                && metrics.cpu_pct < policy.cpu_threshold
                && metrics.net_bytes < policy.net_bytes_threshold
            {
                // Check minimum runtime before allowing transition
                if !is_eligible_for_transition(inst, InstanceStatus::Warm, runtime_policy, now) {
                    return PolicyDecision {
                        instance_id: inst.instance_id.clone(),
                        current_status: inst.status,
                        action: SleepAction::None,
                        reason: format!(
                            "idle but min_running_seconds ({}) not yet satisfied",
                            runtime_policy.min_running_seconds,
                        ),
                    };
                }
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
                // Check minimum warm time before allowing sleep
                if !is_eligible_for_transition(inst, InstanceStatus::Sleeping, runtime_policy, now)
                {
                    return PolicyDecision {
                        instance_id: inst.instance_id.clone(),
                        current_status: inst.status,
                        action: SleepAction::None,
                        reason: format!(
                            "warm idle but min_warm_seconds ({}) not yet satisfied",
                            runtime_policy.min_warm_seconds,
                        ),
                    };
                }
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
/// Under pressure, minimum runtime is deprioritized but NOT exempt —
/// eligible instances are preferred, ineligible ones sorted after.
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

    let runtime_policy = &spec.runtime_policy;
    let now = http::utc_now();

    // Collect warm or idle-running instances
    let mut candidates: Vec<_> = instances
        .iter()
        .filter(|i| matches!(i.status, InstanceStatus::Warm | InstanceStatus::Running))
        .collect();

    // Sort: eligible instances first (by idle_secs desc), then ineligible
    candidates.sort_by(|a, b| {
        let a_eligible =
            is_eligible_for_transition(a, InstanceStatus::Sleeping, runtime_policy, &now);
        let b_eligible =
            is_eligible_for_transition(b, InstanceStatus::Sleeping, runtime_policy, &now);
        match (a_eligible, b_eligible) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.idle_metrics.idle_secs.cmp(&a.idle_metrics.idle_secs),
        }
    });

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
    use crate::vm::instance::state::InstanceNet;

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
            role: Default::default(),
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
            config_version: None,
            secrets_epoch: None,
            entered_running_at: None,
            entered_warm_at: None,
            last_busy_at: None,
        }
    }

    fn no_min_policy() -> RuntimePolicy {
        RuntimePolicy {
            min_running_seconds: 0,
            min_warm_seconds: 0,
            ..RuntimePolicy::default()
        }
    }

    #[test]
    fn test_running_below_threshold_no_action() {
        let inst = test_instance(InstanceStatus::Running, 100, 10.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_running_idle_above_threshold_warm() {
        let inst = test_instance(InstanceStatus::Running, 600, 1.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(decision.action, SleepAction::Warm);
    }

    #[test]
    fn test_running_high_cpu_no_warm() {
        let inst = test_instance(InstanceStatus::Running, 600, 50.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_warm_below_sleep_threshold_no_action() {
        let inst = test_instance(InstanceStatus::Warm, 600, 0.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(decision.action, SleepAction::None);
    }

    #[test]
    fn test_warm_above_sleep_threshold_sleep() {
        let inst = test_instance(InstanceStatus::Warm, 1000, 0.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(decision.action, SleepAction::Sleep);
    }

    #[test]
    fn test_stopped_instance_no_action() {
        let inst = test_instance(InstanceStatus::Stopped, 9999, 0.0);
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &no_min_policy(),
            "2025-01-01T00:00:00Z",
        );
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

    // ── Minimum runtime eligibility tests ────────────────────────────

    #[test]
    fn test_eligible_when_min_running_zero() {
        let inst = test_instance(InstanceStatus::Running, 600, 1.0);
        let policy = no_min_policy();
        assert!(is_eligible_for_transition(
            &inst,
            InstanceStatus::Warm,
            &policy,
            "2025-01-01T00:10:00Z"
        ));
    }

    #[test]
    fn test_not_eligible_within_min_running() {
        let mut inst = test_instance(InstanceStatus::Running, 600, 1.0);
        inst.entered_running_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_running_seconds: 120,
            ..RuntimePolicy::default()
        };
        // Only 30s elapsed, need 120s
        assert!(!is_eligible_for_transition(
            &inst,
            InstanceStatus::Warm,
            &policy,
            "2025-01-01T00:00:30Z"
        ));
    }

    #[test]
    fn test_eligible_past_min_running() {
        let mut inst = test_instance(InstanceStatus::Running, 600, 1.0);
        inst.entered_running_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_running_seconds: 120,
            ..RuntimePolicy::default()
        };
        // 300s elapsed, need 120s
        assert!(is_eligible_for_transition(
            &inst,
            InstanceStatus::Warm,
            &policy,
            "2025-01-01T00:05:00Z"
        ));
    }

    #[test]
    fn test_not_eligible_within_min_warm() {
        let mut inst = test_instance(InstanceStatus::Warm, 1000, 0.0);
        inst.entered_warm_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_warm_seconds: 60,
            ..RuntimePolicy::default()
        };
        // Only 10s elapsed, need 60s
        assert!(!is_eligible_for_transition(
            &inst,
            InstanceStatus::Sleeping,
            &policy,
            "2025-01-01T00:00:10Z"
        ));
    }

    #[test]
    fn test_eligible_past_min_warm() {
        let mut inst = test_instance(InstanceStatus::Warm, 1000, 0.0);
        inst.entered_warm_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_warm_seconds: 60,
            ..RuntimePolicy::default()
        };
        // 120s elapsed, need 60s
        assert!(is_eligible_for_transition(
            &inst,
            InstanceStatus::Sleeping,
            &policy,
            "2025-01-01T00:02:00Z"
        ));
    }

    #[test]
    fn test_eligible_no_timestamp() {
        let inst = test_instance(InstanceStatus::Running, 600, 1.0);
        // No entered_running_at set
        let policy = RuntimePolicy {
            min_running_seconds: 120,
            ..RuntimePolicy::default()
        };
        assert!(is_eligible_for_transition(
            &inst,
            InstanceStatus::Warm,
            &policy,
            "2025-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn test_eligible_other_transitions_always_allowed() {
        let inst = test_instance(InstanceStatus::Sleeping, 0, 0.0);
        let policy = RuntimePolicy {
            min_running_seconds: 9999,
            min_warm_seconds: 9999,
            ..RuntimePolicy::default()
        };
        // Sleeping -> Running always eligible regardless of policy
        assert!(is_eligible_for_transition(
            &inst,
            InstanceStatus::Running,
            &policy,
            "2025-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn test_elapsed_secs() {
        assert_eq!(
            elapsed_secs("2025-01-01T00:00:00Z", "2025-01-01T00:01:00Z"),
            60
        );
        assert_eq!(
            elapsed_secs("2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z"),
            0
        );
        // Backward time returns 0
        assert_eq!(
            elapsed_secs("2025-01-01T00:01:00Z", "2025-01-01T00:00:00Z"),
            0
        );
    }

    #[test]
    fn test_running_idle_but_min_runtime_defers_warm() {
        let mut inst = test_instance(InstanceStatus::Running, 600, 1.0);
        inst.entered_running_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_running_seconds: 900,
            ..RuntimePolicy::default()
        };
        // Instance is idle enough for warm but min_running_seconds not met
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &policy,
            "2025-01-01T00:05:00Z",
        );
        assert_eq!(decision.action, SleepAction::None);
        assert!(decision.reason.contains("min_running_seconds"));
    }

    #[test]
    fn test_running_idle_and_past_min_runtime_allows_warm() {
        let mut inst = test_instance(InstanceStatus::Running, 600, 1.0);
        inst.entered_running_at = Some("2025-01-01T00:00:00Z".to_string());
        let policy = RuntimePolicy {
            min_running_seconds: 60,
            ..RuntimePolicy::default()
        };
        // 300s elapsed, only need 60s
        let decision = evaluate_instance(
            &inst,
            &SleepPolicy::default(),
            &policy,
            "2025-01-01T00:05:00Z",
        );
        assert_eq!(decision.action, SleepAction::Warm);
    }

    #[test]
    fn test_runtime_policy_roundtrip() {
        let policy = RuntimePolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: RuntimePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.min_running_seconds, policy.min_running_seconds);
        assert_eq!(parsed.drain_timeout_seconds, policy.drain_timeout_seconds);
    }
}
