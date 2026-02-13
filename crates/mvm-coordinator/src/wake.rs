use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, watch};
use tracing::info;

use super::config::CoordinatorConfig;
use super::routing::ResolvedRoute;
use crate::client::CoordinatorClient;
use mvm_core::agent::{AgentRequest, AgentResponse};
use mvm_core::instance::InstanceStatus;

/// Per-tenant gateway state as seen by the coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayState {
    /// Gateway is running and ready to accept connections.
    Running {
        /// The gateway VM's guest IP + service port for TCP proxying.
        addr: SocketAddr,
    },
    /// A wake operation is in progress. Waiters subscribe to the channel.
    Waking,
    /// Gateway is warm (snapshot ready) or status unknown. Needs wake.
    Idle,
}

/// Entry tracking a tenant's gateway lifecycle.
struct TenantGateway {
    state: GatewayState,
    /// Broadcast channel for wake coalescing. All waiters receive the result
    /// when the wake completes.
    wake_notify: watch::Sender<Option<Result<SocketAddr, String>>>,
    wake_rx: watch::Receiver<Option<Result<SocketAddr, String>>>,
}

impl TenantGateway {
    fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            state: GatewayState::Idle,
            wake_notify: tx,
            wake_rx: rx,
        }
    }
}

/// Manages on-demand gateway wake/sleep lifecycle across tenants.
///
/// When a connection arrives for a tenant whose gateway isn't running, the
/// WakeManager sends a WakeInstance request to the agent and polls until the
/// gateway is ready. Concurrent requests for the same tenant coalesce into
/// a single wake operation.
pub struct WakeManager {
    tenants: Arc<Mutex<HashMap<String, TenantGateway>>>,
    /// Default port on gateway VMs where the service listens.
    gateway_service_port: u16,
}

impl WakeManager {
    pub fn new(_config: &CoordinatorConfig) -> Self {
        Self {
            tenants: Arc::new(Mutex::new(HashMap::new())),
            gateway_service_port: 8080,
        }
    }

    /// Ensure the gateway for this route is running. Returns the gateway's
    /// address for TCP proxying.
    ///
    /// If the gateway is already running, returns immediately.
    /// If it's idle, initiates a wake and waits.
    /// If a wake is already in progress, joins the existing waiter group.
    pub async fn ensure_running(
        &self,
        route: &ResolvedRoute,
        config: &CoordinatorConfig,
    ) -> Result<SocketAddr> {
        let tenant_id = &route.tenant_id;

        // Fast path: check if already running
        {
            let tenants = self.tenants.lock().await;
            if let Some(entry) = tenants.get(tenant_id)
                && let GatewayState::Running { addr } = &entry.state
            {
                return Ok(*addr);
            }
        }

        // Determine if we need to initiate wake or join existing one
        let rx = {
            let mut tenants = self.tenants.lock().await;
            let entry = tenants
                .entry(tenant_id.clone())
                .or_insert_with(TenantGateway::new);

            match &entry.state {
                GatewayState::Running { addr } => return Ok(*addr),
                GatewayState::Waking => {
                    // Another task is waking this gateway — just subscribe
                    entry.wake_rx.clone()
                }
                GatewayState::Idle => {
                    // We're the first — transition to Waking and initiate
                    entry.state = GatewayState::Waking;
                    // Reset the channel for this wake cycle
                    let (tx, rx) = watch::channel(None);
                    entry.wake_notify = tx;
                    entry.wake_rx = rx.clone();

                    let tenant_id = tenant_id.clone();
                    let route = route.clone();
                    let wake_timeout = config.coordinator.wake_timeout_secs;
                    let service_port = self.gateway_service_port;
                    let tenants_arc = Arc::clone(&self.tenants);

                    // Drop lock before spawning
                    let wake_rx = rx;
                    drop(tenants);

                    // Spawn wake task with Arc reference
                    tokio::spawn(async move {
                        let result = do_wake(&route, wake_timeout, service_port).await;

                        let mut tenants = tenants_arc.lock().await;
                        if let Some(entry) = tenants.get_mut(&tenant_id) {
                            match &result {
                                Ok(addr) => {
                                    entry.state = GatewayState::Running { addr: *addr };
                                    let _ = entry.wake_notify.send(Some(Ok(*addr)));
                                }
                                Err(e) => {
                                    entry.state = GatewayState::Idle;
                                    let _ = entry.wake_notify.send(Some(Err(e.to_string())));
                                }
                            }
                        }
                    });

                    return wait_for_wake(wake_rx, config.coordinator.wake_timeout_secs).await;
                }
            }
        };

        // We're a waiter on an existing wake operation
        wait_for_wake(rx, config.coordinator.wake_timeout_secs).await
    }

    /// Mark a tenant's gateway as idle (e.g., after sleep).
    pub async fn mark_idle(&self, tenant_id: &str) {
        let mut tenants = self.tenants.lock().await;
        if let Some(entry) = tenants.get_mut(tenant_id) {
            entry.state = GatewayState::Idle;
        }
    }

    /// Mark a tenant's gateway as running with a known address.
    pub async fn mark_running(&self, tenant_id: &str, addr: SocketAddr) {
        let mut tenants = self.tenants.lock().await;
        let entry = tenants
            .entry(tenant_id.to_string())
            .or_insert_with(TenantGateway::new);
        entry.state = GatewayState::Running { addr };
    }

    /// Get the current state of a tenant's gateway.
    pub async fn gateway_state(&self, tenant_id: &str) -> GatewayState {
        let tenants = self.tenants.lock().await;
        tenants
            .get(tenant_id)
            .map(|e| e.state.clone())
            .unwrap_or(GatewayState::Idle)
    }
}

/// Wait for a wake operation to complete (either ours or someone else's).
async fn wait_for_wake(
    mut rx: watch::Receiver<Option<Result<SocketAddr, String>>>,
    timeout_secs: u64,
) -> Result<SocketAddr> {
    let deadline = tokio::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(deadline, async {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow::anyhow!("Wake channel closed unexpectedly"))?;
            let val = rx.borrow().clone();
            if let Some(result) = val {
                return result.map_err(|e| anyhow::anyhow!("Wake failed: {}", e));
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => anyhow::bail!("Gateway wake timed out after {}s", timeout_secs),
    }
}

/// Execute the actual wake sequence: send WakeInstance to agent, poll until
/// the gateway instance is Running, return its address.
async fn do_wake(
    route: &ResolvedRoute,
    timeout_secs: u64,
    service_port: u16,
) -> Result<SocketAddr> {
    info!(
        tenant = %route.tenant_id,
        pool = %route.pool_id,
        node = %route.node,
        "Waking gateway"
    );

    let client =
        CoordinatorClient::new().with_context(|| "Failed to create QUIC client for wake")?;

    // First, find the gateway instance to wake by listing instances
    let response = client
        .send(
            route.node,
            &AgentRequest::InstanceList {
                tenant_id: route.tenant_id.clone(),
                pool_id: Some(route.pool_id.clone()),
            },
        )
        .await
        .with_context(|| "Failed to query instances for wake")?;

    let instances = match response {
        AgentResponse::InstanceList(list) => list,
        AgentResponse::Error { code, message } => {
            anyhow::bail!("Agent error ({}): {}", code, message);
        }
        _ => anyhow::bail!("Unexpected response from agent"),
    };

    // Find a warm or sleeping instance to wake
    let target = instances
        .iter()
        .find(|i| i.status == InstanceStatus::Warm || i.status == InstanceStatus::Sleeping)
        .or_else(|| {
            instances
                .iter()
                .find(|i| i.status == InstanceStatus::Stopped)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No wakeable instance found for {}/{}",
                route.tenant_id,
                route.pool_id
            )
        })?;

    let instance_id = &target.instance_id;
    let guest_ip = &target.net.guest_ip;

    info!(
        instance = %instance_id,
        guest_ip = %guest_ip,
        "Sending WakeInstance"
    );

    // Send wake request
    let wake_response = client
        .send(
            route.node,
            &AgentRequest::WakeInstance {
                tenant_id: route.tenant_id.clone(),
                pool_id: route.pool_id.clone(),
                instance_id: instance_id.clone(),
            },
        )
        .await
        .with_context(|| format!("Failed to wake instance {}", instance_id))?;

    match wake_response {
        AgentResponse::WakeResult { success } if success => {
            info!(instance = %instance_id, "Wake acknowledged");
        }
        AgentResponse::WakeResult { success: false } => {
            anyhow::bail!("Agent refused to wake instance {}", instance_id);
        }
        AgentResponse::Error { code, message } => {
            anyhow::bail!("Wake error ({}): {}", code, message);
        }
        _ => anyhow::bail!("Unexpected wake response"),
    }

    // Poll until the instance is Running
    let poll_interval = tokio::time::Duration::from_millis(200);
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Gateway instance {} did not become Running within {}s",
                instance_id,
                timeout_secs
            );
        }

        tokio::time::sleep(poll_interval).await;

        let status_response = client
            .send(
                route.node,
                &AgentRequest::InstanceList {
                    tenant_id: route.tenant_id.clone(),
                    pool_id: Some(route.pool_id.clone()),
                },
            )
            .await;

        if let Ok(AgentResponse::InstanceList(list)) = status_response
            && let Some(inst) = list.iter().find(|i| i.instance_id == *instance_id)
            && inst.status == InstanceStatus::Running
        {
            let addr: SocketAddr = format!("{}:{}", guest_ip, service_port)
                .parse()
                .with_context(|| {
                    format!("Invalid gateway address: {}:{}", guest_ip, service_port)
                })?;
            info!(
                instance = %instance_id,
                addr = %addr,
                "Gateway is Running"
            );
            return Ok(addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CoordinatorConfig {
        CoordinatorConfig::parse(
            r#"
[coordinator]
wake_timeout_secs = 5

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_gateway_state_default_is_idle() {
        let config = test_config();
        let wm = WakeManager::new(&config);
        assert_eq!(wm.gateway_state("alice").await, GatewayState::Idle);
    }

    #[tokio::test]
    async fn test_mark_running() {
        let config = test_config();
        let wm = WakeManager::new(&config);
        let addr: SocketAddr = "10.240.1.5:8080".parse().unwrap();
        wm.mark_running("alice", addr).await;
        assert_eq!(
            wm.gateway_state("alice").await,
            GatewayState::Running { addr }
        );
    }

    #[tokio::test]
    async fn test_mark_idle() {
        let config = test_config();
        let wm = WakeManager::new(&config);
        let addr: SocketAddr = "10.240.1.5:8080".parse().unwrap();
        wm.mark_running("alice", addr).await;
        wm.mark_idle("alice").await;
        assert_eq!(wm.gateway_state("alice").await, GatewayState::Idle);
    }

    #[tokio::test]
    async fn test_ensure_running_fast_path() {
        let config = test_config();
        let wm = WakeManager::new(&config);
        let addr: SocketAddr = "10.240.1.5:8080".parse().unwrap();
        wm.mark_running("alice", addr).await;

        let route = ResolvedRoute {
            tenant_id: "alice".to_string(),
            pool_id: "gateways".to_string(),
            node: "127.0.0.1:4433".parse().unwrap(),
            idle_timeout_secs: 300,
        };

        let result = wm.ensure_running(&route, &config).await.unwrap();
        assert_eq!(result, addr);
    }

    #[tokio::test]
    async fn test_wake_timeout() {
        // Keep sender alive so the timeout fires (not channel-closed error)
        let (_tx, timeout_rx) = watch::channel(None);
        let result = wait_for_wake(timeout_rx, 1).await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("timed out"));
    }

    #[tokio::test]
    async fn test_wake_notify_success() {
        let (tx, rx) = watch::channel(None);
        let addr: SocketAddr = "10.240.1.5:8080".parse().unwrap();

        // Simulate wake completing in background
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let _ = tx.send(Some(Ok(addr)));
        });

        let result = wait_for_wake(rx, 5).await.unwrap();
        assert_eq!(result, addr);
    }

    #[tokio::test]
    async fn test_wake_notify_failure() {
        let (tx, rx) = watch::channel(None);

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let _ = tx.send(Some(Err("agent unreachable".to_string())));
        });

        let result = wait_for_wake(rx, 5).await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("agent unreachable"));
    }
}
