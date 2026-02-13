use std::collections::HashMap;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::debug;

/// Tracks active connections and idle timers per tenant.
///
/// When the last connection for a tenant closes, an idle timer starts. If no
/// new connection arrives before the timeout, the coordinator signals the agent
/// to sleep the gateway back to warm.
pub struct IdleTracker {
    inner: Mutex<HashMap<String, TenantActivity>>,
}

struct TenantActivity {
    /// Number of currently open connections.
    active_connections: u64,
    /// When the last connection closed (None if connections are active).
    last_activity: Option<Instant>,
    /// Total connections served (for metrics).
    total_connections: u64,
}

impl TenantActivity {
    fn new() -> Self {
        Self {
            active_connections: 0,
            last_activity: None,
            total_connections: 0,
        }
    }
}

impl Default for IdleTracker {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl IdleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new connection opening for a tenant.
    pub async fn connection_opened(&self, tenant_id: &str) {
        let mut map = self.inner.lock().await;
        let entry = map
            .entry(tenant_id.to_string())
            .or_insert_with(TenantActivity::new);
        entry.active_connections += 1;
        entry.total_connections += 1;
        entry.last_activity = None; // connections active, no idle
        debug!(
            tenant = %tenant_id,
            active = entry.active_connections,
            "Connection opened"
        );
    }

    /// Record a connection closing for a tenant.
    pub async fn connection_closed(&self, tenant_id: &str) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(tenant_id) {
            entry.active_connections = entry.active_connections.saturating_sub(1);
            if entry.active_connections == 0 {
                entry.last_activity = Some(Instant::now());
                debug!(
                    tenant = %tenant_id,
                    "Last connection closed, idle timer started"
                );
            }
        }
    }

    /// Get the number of active connections for a tenant.
    pub async fn active_connections(&self, tenant_id: &str) -> u64 {
        let map = self.inner.lock().await;
        map.get(tenant_id)
            .map(|e| e.active_connections)
            .unwrap_or(0)
    }

    /// Get total connections served for a tenant (lifetime counter).
    pub async fn total_connections(&self, tenant_id: &str) -> u64 {
        let map = self.inner.lock().await;
        map.get(tenant_id).map(|e| e.total_connections).unwrap_or(0)
    }

    /// Find tenants that have been idle longer than their timeout.
    ///
    /// Returns tenant IDs that should have their gateways slept.
    pub async fn idle_tenants(&self, default_timeout_secs: u64) -> Vec<String> {
        let map = self.inner.lock().await;
        let now = Instant::now();
        let mut idle = Vec::new();

        for (tenant_id, activity) in map.iter() {
            if activity.active_connections == 0
                && let Some(last) = activity.last_activity
            {
                let elapsed = now.duration_since(last);
                if elapsed >= Duration::from_secs(default_timeout_secs) {
                    idle.push(tenant_id.clone());
                }
            }
        }
        idle
    }

    /// Reset idle state for a tenant (called after gateway is slept).
    pub async fn reset(&self, tenant_id: &str) {
        let mut map = self.inner.lock().await;
        map.remove(tenant_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connection_lifecycle() {
        let tracker = IdleTracker::new();

        assert_eq!(tracker.active_connections("alice").await, 0);

        tracker.connection_opened("alice").await;
        assert_eq!(tracker.active_connections("alice").await, 1);

        tracker.connection_opened("alice").await;
        assert_eq!(tracker.active_connections("alice").await, 2);

        tracker.connection_closed("alice").await;
        assert_eq!(tracker.active_connections("alice").await, 1);

        tracker.connection_closed("alice").await;
        assert_eq!(tracker.active_connections("alice").await, 0);
    }

    #[tokio::test]
    async fn test_total_connections() {
        let tracker = IdleTracker::new();

        tracker.connection_opened("alice").await;
        tracker.connection_closed("alice").await;
        tracker.connection_opened("alice").await;
        tracker.connection_closed("alice").await;

        assert_eq!(tracker.total_connections("alice").await, 2);
    }

    #[tokio::test]
    async fn test_idle_tenants_not_idle_yet() {
        let tracker = IdleTracker::new();
        tracker.connection_opened("alice").await;
        tracker.connection_closed("alice").await;

        // Just closed — not idle for 300s yet
        let idle = tracker.idle_tenants(300).await;
        assert!(idle.is_empty());
    }

    #[tokio::test]
    async fn test_idle_tenants_with_zero_timeout() {
        let tracker = IdleTracker::new();
        tracker.connection_opened("alice").await;
        tracker.connection_closed("alice").await;

        // Timeout of 0 means immediately idle
        let idle = tracker.idle_tenants(0).await;
        assert_eq!(idle, vec!["alice"]);
    }

    #[tokio::test]
    async fn test_active_connections_not_idle() {
        let tracker = IdleTracker::new();
        tracker.connection_opened("alice").await;

        // Has active connections — should not be idle
        let idle = tracker.idle_tenants(0).await;
        assert!(idle.is_empty());
    }

    #[tokio::test]
    async fn test_reset_clears_tenant() {
        let tracker = IdleTracker::new();
        tracker.connection_opened("alice").await;
        tracker.connection_closed("alice").await;
        tracker.reset("alice").await;

        assert_eq!(tracker.active_connections("alice").await, 0);
        assert_eq!(tracker.total_connections("alice").await, 0);
    }

    #[tokio::test]
    async fn test_close_without_open_saturates() {
        let tracker = IdleTracker::new();
        // Close without open shouldn't underflow
        tracker.connection_closed("alice").await;
        assert_eq!(tracker.active_connections("alice").await, 0);
    }

    #[tokio::test]
    async fn test_multiple_tenants_independent() {
        let tracker = IdleTracker::new();
        tracker.connection_opened("alice").await;
        tracker.connection_opened("bob").await;
        tracker.connection_closed("bob").await;

        assert_eq!(tracker.active_connections("alice").await, 1);
        assert_eq!(tracker.active_connections("bob").await, 0);

        let idle = tracker.idle_tenants(0).await;
        assert_eq!(idle, vec!["bob"]);
    }
}
