//! Integration tests for coordinator routing, idle tracking, and wake manager.
//!
//! Tests exercise the public coordinator API with in-memory state,
//! verifying route lookups, idle detection, and wake fast paths.

use std::net::SocketAddr;
use std::sync::Arc;

use mvm_coordinator::config::CoordinatorConfig;
use mvm_coordinator::idle::IdleTracker;
use mvm_coordinator::routing::{ResolvedRoute, RouteTable};
use mvm_coordinator::state::{MemStateStore, StateStore};
use mvm_coordinator::wake::WakeManager;

// ---------------------------------------------------------------------------
// Test 1: Configured routes resolve correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_route_lookup() {
    let config = CoordinatorConfig::parse(
        r#"
[coordinator]
idle_timeout_secs = 300

[[nodes]]
address = "127.0.0.1:4433"
name = "node-1"

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"

[[routes]]
tenant_id = "bob"
pool_id = "gateways"
listen = "0.0.0.0:8444"
node = "127.0.0.1:4433"
idle_timeout_secs = 600
"#,
    )
    .unwrap();

    let store: Arc<dyn StateStore> = Arc::new(MemStateStore::new());
    for route in &config.routes {
        let resolved = ResolvedRoute {
            tenant_id: route.tenant_id.clone(),
            pool_id: route.pool_id.clone(),
            node: route.node,
            idle_timeout_secs: route.idle_timeout(&config.coordinator),
        };
        store.set_route(&route.listen, &resolved).await.unwrap();
    }
    let table = RouteTable::new(store);

    // Two routes registered
    assert_eq!(table.listen_addrs().await.len(), 2);

    // Alice: global default timeout
    let addr: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    let route = table.lookup(&addr).await.unwrap();
    assert_eq!(route.tenant_id, "alice");
    assert_eq!(route.pool_id, "gateways");
    assert_eq!(route.idle_timeout_secs, 300);

    // Bob: per-route timeout override
    let addr: SocketAddr = "0.0.0.0:8444".parse().unwrap();
    let route = table.lookup(&addr).await.unwrap();
    assert_eq!(route.tenant_id, "bob");
    assert_eq!(route.idle_timeout_secs, 600);

    // Unknown address returns None
    let addr: SocketAddr = "0.0.0.0:9999".parse().unwrap();
    assert!(table.lookup(&addr).await.is_none());
}

// ---------------------------------------------------------------------------
// Test 2: Connection closes -> idle timer starts -> state transitions to Idle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_idle_sweep() {
    let tracker = IdleTracker::new();

    // Open connections for alice
    tracker.connection_opened("alice").await;
    tracker.connection_opened("alice").await;
    assert_eq!(tracker.active_connections("alice").await, 2);

    // Not idle while connections are active
    let idle = tracker.idle_tenants(0).await;
    assert!(
        idle.is_empty(),
        "Should not be idle with active connections"
    );

    // Close all connections
    tracker.connection_closed("alice").await;
    tracker.connection_closed("alice").await;
    assert_eq!(tracker.active_connections("alice").await, 0);

    // Now idle (timeout=0 means immediate)
    let idle = tracker.idle_tenants(0).await;
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0], "alice");

    // Total connections tracked
    assert_eq!(tracker.total_connections("alice").await, 2);

    // Reset clears idle state
    tracker.reset("alice").await;
    let idle = tracker.idle_tenants(0).await;
    assert!(idle.is_empty(), "Should not be idle after reset");
    assert_eq!(tracker.total_connections("alice").await, 0);
}

// ---------------------------------------------------------------------------
// Test 3: 3 concurrent requests for same tenant share one wake (fast path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_wake_coalescing() {
    let config = CoordinatorConfig::parse(
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
    .unwrap();

    let store: Arc<dyn StateStore> = Arc::new(MemStateStore::new());
    let wm = Arc::new(WakeManager::new(store, &config));

    // Pre-mark gateway as running (simulates already-woken state)
    let addr: SocketAddr = "10.240.1.5:8080".parse().unwrap();
    wm.mark_running("alice", addr).await;

    let route = ResolvedRoute {
        tenant_id: "alice".to_string(),
        pool_id: "gateways".to_string(),
        node: "127.0.0.1:4433".parse().unwrap(),
        idle_timeout_secs: 300,
    };

    // Spawn 3 concurrent ensure_running requests
    let mut handles = Vec::new();
    for _ in 0..3 {
        let wm = Arc::clone(&wm);
        let route = route.clone();
        let config = config.clone();
        handles.push(tokio::spawn(async move {
            wm.ensure_running(&route, &config).await
        }));
    }

    // All 3 should return the same address without triggering a wake
    for handle in handles {
        let result = handle.await.unwrap().unwrap();
        assert_eq!(
            result, addr,
            "All requests should get the same gateway address"
        );
    }

    // Gateway state should still be Running
    match wm.gateway_state("alice").await {
        mvm_coordinator::wake::GatewayState::Running { addr: a } => {
            assert_eq!(a, addr);
        }
        other => panic!("Expected Running, got {:?}", other),
    }
}
