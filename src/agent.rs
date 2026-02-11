use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::infra::shell;
use crate::node;
use crate::security::certs;
use crate::sleep::policy;
use crate::vm::instance::lifecycle::{
    instance_create, instance_list, instance_sleep, instance_start, instance_stop, instance_wake,
    instance_warm,
};
use crate::vm::instance::state::{InstanceState, InstanceStatus};
use crate::vm::pool::lifecycle::{pool_create, pool_list, pool_load};
use crate::vm::tenant::config::TenantNet;
use crate::vm::tenant::lifecycle::{tenant_create, tenant_destroy, tenant_exists, tenant_list};

// ============================================================================
// Desired state schema (pushed by coordinator)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredState {
    pub schema_version: u32,
    pub node_id: String,
    pub tenants: Vec<DesiredTenant>,
    #[serde(default)]
    pub prune_unknown_tenants: bool,
    #[serde(default)]
    pub prune_unknown_pools: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredTenant {
    pub tenant_id: String,
    pub network: DesiredTenantNetwork,
    pub quotas: crate::vm::tenant::config::TenantQuota,
    #[serde(default)]
    pub secrets_hash: Option<String>,
    pub pools: Vec<DesiredPool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredTenantNetwork {
    pub tenant_net_id: u16,
    pub ipv4_subnet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredPool {
    pub pool_id: String,
    pub flake_ref: String,
    pub profile: String,
    pub instance_resources: crate::vm::pool::config::InstanceResources,
    pub desired_counts: crate::vm::pool::config::DesiredCounts,
    #[serde(default = "default_seccomp")]
    pub seccomp_policy: String,
    #[serde(default = "default_compression")]
    pub snapshot_compression: String,
}

fn default_seccomp() -> String {
    "baseline".to_string()
}

fn default_compression() -> String {
    "none".to_string()
}

// ============================================================================
// Reconcile report
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub tenants_created: Vec<String>,
    pub tenants_pruned: Vec<String>,
    pub pools_created: Vec<String>,
    pub instances_created: u32,
    pub instances_started: u32,
    pub instances_warmed: u32,
    pub instances_slept: u32,
    pub instances_stopped: u32,
    pub errors: Vec<String>,
}

// ============================================================================
// Typed message protocol (QUIC API)
// ============================================================================

/// Strongly typed request sent over QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentRequest {
    /// Push a new desired state for reconciliation.
    Reconcile(DesiredState),
    /// Query node capabilities and identity.
    NodeInfo,
    /// Query aggregate node statistics.
    NodeStats,
    /// List all tenants on this node.
    TenantList,
    /// List instances for a specific tenant (optionally filtered by pool).
    InstanceList {
        tenant_id: String,
        pool_id: Option<String>,
    },
    /// Urgently wake a sleeping instance.
    WakeInstance {
        tenant_id: String,
        pool_id: String,
        instance_id: String,
    },
}

/// Strongly typed response returned over QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResponse {
    /// Result of a reconcile pass.
    ReconcileResult(ReconcileReport),
    /// Node info.
    NodeInfo(node::NodeInfo),
    /// Aggregate node stats.
    NodeStats(node::NodeStats),
    /// List of tenant IDs.
    TenantList(Vec<String>),
    /// List of instance states.
    InstanceList(Vec<InstanceState>),
    /// Result of a wake operation.
    WakeResult { success: bool },
    /// Error response.
    Error { code: u16, message: String },
}

/// Default listen address for the QUIC API.
const DEFAULT_LISTEN: &str = "0.0.0.0:4433";

/// Maximum request frame size (1 MiB).
const MAX_FRAME_SIZE: usize = 1024 * 1024;

// ============================================================================
// Frame protocol: length-prefixed JSON over QUIC bi-directional streams
// ============================================================================

/// Read a length-prefixed JSON frame from a QUIC recv stream.
async fn read_frame(recv: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    // Read 4-byte big-endian length prefix
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .with_context(|| "Failed to read frame length")?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_FRAME_SIZE {
        anyhow::bail!("Frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE);
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read frame body")?;

    Ok(buf)
}

/// Write a length-prefixed JSON frame to a QUIC send stream.
async fn write_frame(send: &mut quinn::SendStream, data: &[u8]) -> Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    send.write_all(&len)
        .await
        .with_context(|| "Failed to write frame length")?;
    send.write_all(data)
        .await
        .with_context(|| "Failed to write frame body")?;
    send.finish().with_context(|| "Failed to finish stream")?;
    Ok(())
}

// ============================================================================
// Request handler
// ============================================================================

/// Handle a single typed request and produce a response.
fn handle_request(request: AgentRequest) -> AgentResponse {
    match request {
        AgentRequest::Reconcile(desired) => {
            let validation_errors = validate_desired_state(&desired);
            if !validation_errors.is_empty() {
                return AgentResponse::Error {
                    code: 400,
                    message: format!("Validation errors: {}", validation_errors.join("; ")),
                };
            }

            match reconcile_desired(&desired, desired.prune_unknown_tenants) {
                Ok(report) => AgentResponse::ReconcileResult(report),
                Err(e) => AgentResponse::Error {
                    code: 500,
                    message: format!("Reconcile failed: {}", e),
                },
            }
        }
        AgentRequest::NodeInfo => match node::collect_info() {
            Ok(info) => AgentResponse::NodeInfo(info),
            Err(e) => AgentResponse::Error {
                code: 500,
                message: format!("Failed to collect node info: {}", e),
            },
        },
        AgentRequest::NodeStats => match node::collect_stats() {
            Ok(stats) => AgentResponse::NodeStats(stats),
            Err(e) => AgentResponse::Error {
                code: 500,
                message: format!("Failed to collect node stats: {}", e),
            },
        },
        AgentRequest::TenantList => match tenant_list() {
            Ok(tenants) => AgentResponse::TenantList(tenants),
            Err(e) => AgentResponse::Error {
                code: 500,
                message: format!("Failed to list tenants: {}", e),
            },
        },
        AgentRequest::InstanceList { tenant_id, pool_id } => {
            let pools = match pool_id {
                Some(pid) => vec![pid],
                None => match pool_list(&tenant_id) {
                    Ok(p) => p,
                    Err(e) => {
                        return AgentResponse::Error {
                            code: 500,
                            message: format!("Failed to list pools: {}", e),
                        };
                    }
                },
            };

            let mut all = Vec::new();
            for pid in &pools {
                if let Ok(instances) = instance_list(&tenant_id, pid) {
                    all.extend(instances);
                }
            }
            AgentResponse::InstanceList(all)
        }
        AgentRequest::WakeInstance {
            tenant_id,
            pool_id,
            instance_id,
        } => match instance_wake(&tenant_id, &pool_id, &instance_id) {
            Ok(_) => AgentResponse::WakeResult { success: true },
            Err(e) => AgentResponse::Error {
                code: 500,
                message: format!("Wake failed: {}", e),
            },
        },
    }
}

// ============================================================================
// One-shot reconcile (CLI)
// ============================================================================

/// Run a single reconcile pass against a desired state file.
pub fn reconcile(desired_path: &str, prune: bool) -> Result<()> {
    let json = shell::run_in_vm_stdout(&format!("cat {}", desired_path))
        .with_context(|| format!("Failed to read desired state from {}", desired_path))?;
    let desired: DesiredState =
        serde_json::from_str(&json).with_context(|| "Failed to parse desired state JSON")?;

    let report = reconcile_desired(&desired, prune)?;

    if !report.tenants_created.is_empty() {
        println!("Created tenants: {}", report.tenants_created.join(", "));
    }
    if !report.tenants_pruned.is_empty() {
        println!("Pruned tenants: {}", report.tenants_pruned.join(", "));
    }
    if !report.pools_created.is_empty() {
        println!("Created pools: {}", report.pools_created.join(", "));
    }
    if report.instances_created > 0 {
        println!("Created {} instances", report.instances_created);
    }
    if report.instances_started > 0 {
        println!("Started {} instances", report.instances_started);
    }
    if report.instances_warmed > 0 {
        println!("Warmed {} instances", report.instances_warmed);
    }
    if report.instances_slept > 0 {
        println!("Slept {} instances", report.instances_slept);
    }
    if report.instances_stopped > 0 {
        println!("Stopped {} instances", report.instances_stopped);
    }
    if !report.errors.is_empty() {
        eprintln!("Reconcile errors:");
        for err in &report.errors {
            eprintln!("  - {}", err);
        }
    }

    Ok(())
}

// ============================================================================
// Reconcile engine
// ============================================================================

/// Core reconcile logic, returns a report of what changed.
fn reconcile_desired(desired: &DesiredState, prune: bool) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();

    // Phase 1: Ensure desired tenants exist
    for dt in &desired.tenants {
        if !tenant_exists(&dt.tenant_id)? {
            let subnet_parts: Vec<&str> = dt.network.ipv4_subnet.split('/').collect();
            let base_ip = subnet_parts.first().unwrap_or(&"10.240.0.0");
            let prefix = base_ip
                .rsplit_once('.')
                .map(|(p, _)| p)
                .unwrap_or("10.240.0");
            let gateway = format!("{}.1", prefix);

            let net = TenantNet::new(dt.network.tenant_net_id, &dt.network.ipv4_subnet, &gateway);

            if let Err(e) = tenant_create(&dt.tenant_id, net, dt.quotas.clone()) {
                report
                    .errors
                    .push(format!("Failed to create tenant {}: {}", dt.tenant_id, e));
                continue;
            }
            report.tenants_created.push(dt.tenant_id.clone());
        }

        // Phase 2: Ensure desired pools exist
        for dp in &dt.pools {
            let pool_exists = pool_load(&dt.tenant_id, &dp.pool_id).is_ok();
            if !pool_exists {
                if let Err(e) = pool_create(
                    &dt.tenant_id,
                    &dp.pool_id,
                    &dp.flake_ref,
                    &dp.profile,
                    dp.instance_resources.clone(),
                ) {
                    report.errors.push(format!(
                        "Failed to create pool {}/{}: {}",
                        dt.tenant_id, dp.pool_id, e
                    ));
                    continue;
                }
                report
                    .pools_created
                    .push(format!("{}/{}", dt.tenant_id, dp.pool_id));
            }

            // Phase 3: Scale instances to match desired counts
            if let Err(e) = reconcile_pool_instances(
                &dt.tenant_id,
                &dp.pool_id,
                &dp.desired_counts,
                &mut report,
            ) {
                report.errors.push(format!(
                    "Failed to reconcile {}/{}: {}",
                    dt.tenant_id, dp.pool_id, e
                ));
            }
        }

        // Phase 4: Prune unknown pools within this tenant
        if prune && desired.prune_unknown_pools {
            let desired_pool_ids: Vec<&str> = dt.pools.iter().map(|p| p.pool_id.as_str()).collect();
            if let Ok(existing_pools) = pool_list(&dt.tenant_id) {
                for pool_id in existing_pools {
                    if !desired_pool_ids.contains(&pool_id.as_str())
                        && let Err(e) =
                            crate::vm::pool::lifecycle::pool_destroy(&dt.tenant_id, &pool_id, true)
                    {
                        report.errors.push(format!(
                            "Failed to prune pool {}/{}: {}",
                            dt.tenant_id, pool_id, e
                        ));
                    }
                }
            }
        }
    }

    // Phase 5: Prune unknown tenants
    if prune && desired.prune_unknown_tenants {
        let desired_tenant_ids: Vec<&str> = desired
            .tenants
            .iter()
            .map(|t| t.tenant_id.as_str())
            .collect();
        if let Ok(existing_tenants) = tenant_list() {
            for tid in existing_tenants {
                if !desired_tenant_ids.contains(&tid.as_str()) {
                    if let Err(e) = tenant_destroy(&tid, true) {
                        report
                            .errors
                            .push(format!("Failed to prune tenant {}: {}", tid, e));
                    } else {
                        report.tenants_pruned.push(tid);
                    }
                }
            }
        }
    }

    // Phase 6: Run sleep policy evaluation for each pool
    for dt in &desired.tenants {
        for dp in &dt.pools {
            if let Ok(decisions) = policy::evaluate_pool(&dt.tenant_id, &dp.pool_id) {
                for decision in decisions {
                    let result = match decision.action {
                        policy::SleepAction::Warm => {
                            instance_warm(&dt.tenant_id, &dp.pool_id, &decision.instance_id)
                                .map(|_| report.instances_warmed += 1)
                        }
                        policy::SleepAction::Sleep => {
                            instance_sleep(&dt.tenant_id, &dp.pool_id, &decision.instance_id, false)
                                .map(|_| report.instances_slept += 1)
                        }
                        policy::SleepAction::None => Ok(()),
                    };
                    if let Err(e) = result {
                        report.errors.push(format!(
                            "Sleep policy action failed for {}: {}",
                            decision.instance_id, e
                        ));
                    }
                }
            }
        }
    }

    Ok(report)
}

/// Reconcile instances within a pool to match desired counts.
fn reconcile_pool_instances(
    tenant_id: &str,
    pool_id: &str,
    desired: &crate::vm::pool::config::DesiredCounts,
    report: &mut ReconcileReport,
) -> Result<()> {
    let instances = instance_list(tenant_id, pool_id)?;

    let mut running = Vec::new();
    let mut warm = Vec::new();
    let mut sleeping = Vec::new();
    let mut stopped = Vec::new();

    for inst in &instances {
        match inst.status {
            InstanceStatus::Running => running.push(inst.instance_id.clone()),
            InstanceStatus::Warm => warm.push(inst.instance_id.clone()),
            InstanceStatus::Sleeping => sleeping.push(inst.instance_id.clone()),
            InstanceStatus::Stopped => stopped.push(inst.instance_id.clone()),
            _ => {}
        }
    }

    // Scale up running instances
    let running_count = running.len() as u32;
    if running_count < desired.running {
        let needed = desired.running - running_count;

        // First, try to start stopped instances
        for id in stopped.iter().take(needed as usize) {
            match instance_start(tenant_id, pool_id, id) {
                Ok(_) => report.instances_started += 1,
                Err(e) => report.errors.push(format!("Failed to start {}: {}", id, e)),
            }
        }

        // If still need more, create new instances
        let started_from_stopped = needed.min(stopped.len() as u32);
        let still_needed = needed - started_from_stopped;
        for _ in 0..still_needed {
            match instance_create(tenant_id, pool_id) {
                Ok(id) => {
                    report.instances_created += 1;
                    match instance_start(tenant_id, pool_id, &id) {
                        Ok(_) => report.instances_started += 1,
                        Err(e) => report
                            .errors
                            .push(format!("Failed to start new {}: {}", id, e)),
                    }
                }
                Err(e) => report
                    .errors
                    .push(format!("Failed to create instance: {}", e)),
            }
        }
    }

    // Scale down running instances (stop excess)
    if running_count > desired.running {
        let excess = running_count - desired.running;
        for id in running.iter().rev().take(excess as usize) {
            match instance_stop(tenant_id, pool_id, id) {
                Ok(_) => report.instances_stopped += 1,
                Err(e) => report.errors.push(format!("Failed to stop {}: {}", id, e)),
            }
        }
    }

    Ok(())
}

/// Validate a desired state document.
pub fn validate_desired_state(desired: &DesiredState) -> Vec<String> {
    let mut errors = Vec::new();

    if desired.schema_version != 1 {
        errors.push(format!(
            "Unsupported schema version: {} (expected 1)",
            desired.schema_version
        ));
    }

    for tenant in &desired.tenants {
        if tenant.tenant_id.is_empty() {
            errors.push("Tenant ID cannot be empty".to_string());
        }
        for pool in &tenant.pools {
            if pool.pool_id.is_empty() {
                errors.push(format!(
                    "Pool ID cannot be empty in tenant {}",
                    tenant.tenant_id
                ));
            }
            if pool.instance_resources.vcpus == 0 {
                errors.push(format!(
                    "Pool {}/{} has 0 vCPUs",
                    tenant.tenant_id, pool.pool_id
                ));
            }
        }
    }

    errors
}

// ============================================================================
// Agent daemon (tokio + QUIC + periodic reconcile)
// ============================================================================

/// Start the agent daemon with QUIC API server and periodic reconcile.
///
/// Spawns a tokio runtime with:
/// - QUIC mTLS server accepting typed requests
/// - Periodic reconcile task (reads desired state from file)
/// - SIGTERM handler for graceful shutdown
pub fn serve(
    interval_secs: u64,
    desired_path: Option<&str>,
    listen_addr: Option<&str>,
) -> Result<()> {
    let addr: SocketAddr = listen_addr
        .unwrap_or(DEFAULT_LISTEN)
        .parse()
        .with_context(|| "Invalid listen address")?;

    let desired_file = desired_path.map(|s| s.to_string());

    // Build tokio runtime
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .with_context(|| "Failed to create tokio runtime")?;

    runtime.block_on(async move { run_daemon(addr, interval_secs, desired_file).await })
}

/// Main daemon loop: QUIC server + periodic reconcile + shutdown handler.
async fn run_daemon(
    addr: SocketAddr,
    interval_secs: u64,
    desired_file: Option<String>,
) -> Result<()> {
    // Load mTLS server config
    let server_config = certs::load_server_config()
        .with_context(|| "Failed to load TLS certificates. Run 'mvm agent certs init' first.")?;

    let endpoint = quinn::Endpoint::server(server_config, addr)
        .with_context(|| format!("Failed to bind QUIC endpoint on {}", addr))?;

    eprintln!("Agent listening on {}", addr);

    // Shutdown signal
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    // Periodic reconcile task
    let reconcile_handle = if let Some(path) = desired_file.clone() {
        let handle = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                eprintln!("Running periodic reconcile...");
                // Reconcile is synchronous (shell commands), run on blocking thread
                let path = path.clone();
                let result = tokio::task::spawn_blocking(move || reconcile(&path, true)).await;

                match result {
                    Ok(Ok(())) => eprintln!("Reconcile complete."),
                    Ok(Err(e)) => eprintln!("Reconcile error: {}", e),
                    Err(e) => eprintln!("Reconcile task panicked: {}", e),
                }
            }
        });
        Some(handle)
    } else {
        None
    };

    // Accept QUIC connections
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                match incoming {
                    Some(conn) => {
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(conn).await {
                                eprintln!("Connection error: {}", e);
                            }
                        });
                    }
                    None => break,
                }
            }
            _ = &mut shutdown => {
                eprintln!("Received shutdown signal, stopping...");
                break;
            }
        }
    }

    // Graceful shutdown
    endpoint.close(quinn::VarInt::from_u32(0), b"shutdown");
    if let Some(handle) = reconcile_handle {
        handle.abort();
    }
    eprintln!("Agent stopped.");
    Ok(())
}

/// Handle a single QUIC connection (may have multiple bi-directional streams).
async fn handle_connection(incoming: quinn::Incoming) -> Result<()> {
    let connection = incoming
        .await
        .with_context(|| "Failed to accept connection")?;

    loop {
        let stream = connection.accept_bi().await;
        match stream {
            Ok((send, recv)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(send, recv).await {
                        eprintln!("Stream error: {}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
            Err(e) => {
                return Err(anyhow::anyhow!("Connection error: {}", e));
            }
        }
    }

    Ok(())
}

/// Handle a single bi-directional QUIC stream: read request, dispatch, write response.
async fn handle_stream(mut send: quinn::SendStream, mut recv: quinn::RecvStream) -> Result<()> {
    let frame = read_frame(&mut recv).await?;

    let request: AgentRequest =
        serde_json::from_slice(&frame).with_context(|| "Failed to parse request")?;

    // Dispatch to handler on blocking thread (reconcile calls shell commands)
    let response = tokio::task::spawn_blocking(move || handle_request(request))
        .await
        .with_context(|| "Handler task failed")?;

    let response_bytes =
        serde_json::to_vec(&response).with_context(|| "Failed to serialize response")?;

    write_frame(&mut send, &response_bytes).await?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::pool::config::{DesiredCounts, InstanceResources};

    #[test]
    fn test_desired_state_roundtrip() {
        let state = DesiredState {
            schema_version: 1,
            node_id: "node-1".to_string(),
            tenants: vec![DesiredTenant {
                tenant_id: "acme".to_string(),
                network: DesiredTenantNetwork {
                    tenant_net_id: 3,
                    ipv4_subnet: "10.240.3.0/24".to_string(),
                },
                quotas: Default::default(),
                secrets_hash: Some("abc123".to_string()),
                pools: vec![DesiredPool {
                    pool_id: "workers".to_string(),
                    flake_ref: "github:org/repo".to_string(),
                    profile: "minimal".to_string(),
                    instance_resources: InstanceResources {
                        vcpus: 2,
                        mem_mib: 1024,
                        data_disk_mib: 0,
                    },
                    desired_counts: DesiredCounts {
                        running: 3,
                        warm: 1,
                        sleeping: 2,
                    },
                    seccomp_policy: "baseline".to_string(),
                    snapshot_compression: "zstd".to_string(),
                }],
            }],
            prune_unknown_tenants: true,
            prune_unknown_pools: true,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: DesiredState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.tenants.len(), 1);
        assert_eq!(parsed.tenants[0].pools[0].desired_counts.running, 3);
    }

    #[test]
    fn test_validate_desired_state_valid() {
        let state = DesiredState {
            schema_version: 1,
            node_id: "node-1".to_string(),
            tenants: vec![DesiredTenant {
                tenant_id: "acme".to_string(),
                network: DesiredTenantNetwork {
                    tenant_net_id: 3,
                    ipv4_subnet: "10.240.3.0/24".to_string(),
                },
                quotas: Default::default(),
                secrets_hash: None,
                pools: vec![DesiredPool {
                    pool_id: "workers".to_string(),
                    flake_ref: ".".to_string(),
                    profile: "minimal".to_string(),
                    instance_resources: InstanceResources {
                        vcpus: 2,
                        mem_mib: 512,
                        data_disk_mib: 0,
                    },
                    desired_counts: DesiredCounts::default(),
                    seccomp_policy: "baseline".to_string(),
                    snapshot_compression: "none".to_string(),
                }],
            }],
            prune_unknown_tenants: false,
            prune_unknown_pools: false,
        };

        let errors = validate_desired_state(&state);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validate_desired_state_bad_version() {
        let state = DesiredState {
            schema_version: 99,
            node_id: "node-1".to_string(),
            tenants: vec![],
            prune_unknown_tenants: false,
            prune_unknown_pools: false,
        };

        let errors = validate_desired_state(&state);
        assert!(!errors.is_empty());
        assert!(errors[0].contains("schema version"));
    }

    #[test]
    fn test_validate_desired_state_empty_tenant_id() {
        let state = DesiredState {
            schema_version: 1,
            node_id: "node-1".to_string(),
            tenants: vec![DesiredTenant {
                tenant_id: "".to_string(),
                network: DesiredTenantNetwork {
                    tenant_net_id: 1,
                    ipv4_subnet: "10.240.1.0/24".to_string(),
                },
                quotas: Default::default(),
                secrets_hash: None,
                pools: vec![],
            }],
            prune_unknown_tenants: false,
            prune_unknown_pools: false,
        };

        let errors = validate_desired_state(&state);
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_reconcile_report_default() {
        let report = ReconcileReport::default();
        assert_eq!(report.instances_created, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn test_agent_request_roundtrip() {
        let req = AgentRequest::NodeInfo;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AgentRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentRequest::NodeInfo));
    }

    #[test]
    fn test_agent_request_reconcile_roundtrip() {
        let state = DesiredState {
            schema_version: 1,
            node_id: "n1".to_string(),
            tenants: vec![],
            prune_unknown_tenants: false,
            prune_unknown_pools: false,
        };
        let req = AgentRequest::Reconcile(state);
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AgentRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentRequest::Reconcile(_)));
    }

    #[test]
    fn test_agent_request_wake_roundtrip() {
        let req = AgentRequest::WakeInstance {
            tenant_id: "acme".to_string(),
            pool_id: "workers".to_string(),
            instance_id: "i-abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AgentRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            AgentRequest::WakeInstance {
                tenant_id,
                pool_id,
                instance_id,
            } => {
                assert_eq!(tenant_id, "acme");
                assert_eq!(pool_id, "workers");
                assert_eq!(instance_id, "i-abc");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_agent_response_roundtrip() {
        let resp = AgentResponse::Error {
            code: 404,
            message: "not found".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: AgentResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            AgentResponse::Error { code, message } => {
                assert_eq!(code, 404);
                assert_eq!(message, "not found");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_agent_response_wake_result() {
        let resp = AgentResponse::WakeResult { success: true };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: AgentResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            AgentResponse::WakeResult { success } => assert!(success),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_all_request_variants_serialize() {
        let variants: Vec<AgentRequest> = vec![
            AgentRequest::Reconcile(DesiredState {
                schema_version: 1,
                node_id: "n".to_string(),
                tenants: vec![],
                prune_unknown_tenants: false,
                prune_unknown_pools: false,
            }),
            AgentRequest::NodeInfo,
            AgentRequest::NodeStats,
            AgentRequest::TenantList,
            AgentRequest::InstanceList {
                tenant_id: "t".to_string(),
                pool_id: None,
            },
            AgentRequest::WakeInstance {
                tenant_id: "t".to_string(),
                pool_id: "p".to_string(),
                instance_id: "i".to_string(),
            },
        ];

        for req in &variants {
            let json = serde_json::to_string(req).unwrap();
            let _: AgentRequest = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_all_response_variants_serialize() {
        let variants: Vec<AgentResponse> = vec![
            AgentResponse::ReconcileResult(ReconcileReport::default()),
            AgentResponse::NodeInfo(node::NodeInfo {
                node_id: "n".to_string(),
                hostname: "h".to_string(),
                arch: "aarch64".to_string(),
                total_vcpus: 4,
                total_mem_mib: 8192,
                lima_status: None,
                firecracker_version: None,
                jailer_available: false,
                cgroup_v2: false,
            }),
            AgentResponse::NodeStats(node::NodeStats::default()),
            AgentResponse::TenantList(vec!["t1".to_string()]),
            AgentResponse::InstanceList(vec![]),
            AgentResponse::WakeResult { success: false },
            AgentResponse::Error {
                code: 500,
                message: "err".to_string(),
            },
        ];

        for resp in &variants {
            let json = serde_json::to_string(resp).unwrap();
            let _: AgentResponse = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_default_listen_addr() {
        let addr: SocketAddr = DEFAULT_LISTEN.parse().unwrap();
        assert_eq!(addr.port(), 4433);
    }

    #[test]
    fn test_max_frame_size() {
        assert_eq!(MAX_FRAME_SIZE, 1024 * 1024);
    }
}
