//! Integration tests for the instance lifecycle using the shell_mock infrastructure.
//!
//! These tests exercise the public lifecycle API (create, warm, sleep, stop, destroy)
//! with an in-memory mock filesystem, validating state machine transitions, cleanup,
//! quota enforcement, and network identity preservation.

use mvm_core::instance::{InstanceState, InstanceStatus};
use mvm_core::tenant::TenantQuota;
use mvm_runtime::shell_mock::{self, SharedFs};
use mvm_runtime::vm::instance::lifecycle;
use mvm_runtime::vm::tenant::quota;

/// Set up the mock with a standard tenant ("acme") and pool ("workers").
fn setup_mock() -> (shell_mock::MockGuard, SharedFs) {
    let tenant_json = shell_mock::tenant_fixture("acme", 3, "10.240.3.0/24", "10.240.3.1");
    let pool_json = shell_mock::pool_fixture("acme", "workers");
    shell_mock::mock_fs()
        .with_file("/var/lib/mvm/tenants/acme/tenant.json", &tenant_json)
        .with_file(
            "/var/lib/mvm/tenants/acme/pools/workers/pool.json",
            &pool_json,
        )
        .install()
}

/// Read instance state from the mock filesystem.
fn read_instance(fs: &SharedFs, instance_id: &str) -> InstanceState {
    let path = format!(
        "/var/lib/mvm/tenants/acme/pools/workers/instances/{}/instance.json",
        instance_id
    );
    let fs_lock = fs.lock().unwrap();
    let json = fs_lock
        .get(&path)
        .unwrap_or_else(|| panic!("instance.json not found at {}", path));
    serde_json::from_str(json).expect("invalid instance.json")
}

/// Write instance state to the mock filesystem.
fn write_instance(fs: &SharedFs, state: &InstanceState) {
    let path = format!(
        "/var/lib/mvm/tenants/{}/pools/{}/instances/{}/instance.json",
        state.tenant_id, state.pool_id, state.instance_id
    );
    let json = serde_json::to_string_pretty(state).unwrap();
    fs.lock().unwrap().insert(path, json);
}

/// Simulate starting an instance by writing Running state with a fake PID.
///
/// `instance_start` cannot be tested with mock because it launches Firecracker
/// and reads a PID file that the mock cannot produce. Instead, we simulate the
/// state change directly to test subsequent lifecycle operations.
fn simulate_start(fs: &SharedFs, instance_id: &str) {
    let mut state = read_instance(fs, instance_id);
    state.status = InstanceStatus::Running;
    state.firecracker_pid = Some(99999);
    state.last_started_at = Some("2025-01-01T00:00:00Z".to_string());
    state.entered_running_at = Some("2025-01-01T00:00:00Z".to_string());
    write_instance(fs, &state);
}

// ---------------------------------------------------------------------------
// Test 1: Full lifecycle happy path
// ---------------------------------------------------------------------------

#[test]
fn test_full_lifecycle_happy_path() {
    let (_guard, fs) = setup_mock();

    // 1. Create → status should be Created
    let id = lifecycle::instance_create("acme", "workers").unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Created);
    assert_eq!(state.tenant_id, "acme");
    assert_eq!(state.pool_id, "workers");
    assert!(state.firecracker_pid.is_none());

    // 2. Simulate start → Running (can't call instance_start with mock)
    simulate_start(&fs, &id);
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Running);
    assert_eq!(state.firecracker_pid, Some(99999));

    // 3. Warm (Running → Warm) via real function
    lifecycle::instance_warm("acme", "workers", &id).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Warm);
    assert!(state.entered_warm_at.is_some());
    // PID should still be set (vCPUs paused, process alive)
    assert_eq!(state.firecracker_pid, Some(99999));

    // 4. Sleep (Warm → Sleeping) via real function (force=true to skip vsock)
    lifecycle::instance_sleep("acme", "workers", &id, true).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Sleeping);
    assert!(state.firecracker_pid.is_none());
    assert!(state.last_stopped_at.is_some());
    assert!(state.entered_running_at.is_none());
    assert!(state.entered_warm_at.is_none());

    // 5. Stop (Sleeping → Stopped) via real function
    lifecycle::instance_stop("acme", "workers", &id).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Stopped);

    // 6. Destroy
    lifecycle::instance_destroy("acme", "workers", &id, true).unwrap();

    // Verify instance directory is cleaned up
    let inst_prefix = format!("/var/lib/mvm/tenants/acme/pools/workers/instances/{}/", id);
    let remaining: Vec<String> = fs
        .lock()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&inst_prefix))
        .cloned()
        .collect();
    assert!(
        remaining.is_empty(),
        "Expected no instance files after destroy, found: {:?}",
        remaining
    );
}

// ---------------------------------------------------------------------------
// Test 2: Invalid state transition is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_transition_rejected() {
    let (_guard, fs) = setup_mock();

    let id = lifecycle::instance_create("acme", "workers").unwrap();
    simulate_start(&fs, &id);

    // Running → Sleeping is INVALID (must go through Warm first)
    let result = lifecycle::instance_sleep("acme", "workers", &id, true);
    assert!(result.is_err(), "Expected error for Running→Sleeping");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Invalid state transition"),
        "Expected transition error, got: {}",
        err
    );

    // Verify state is unchanged — still Running
    let state = read_instance(&fs, &id);
    assert_eq!(state.status, InstanceStatus::Running);

    // Also verify: Created → Running is invalid (must go through Ready)
    let id2 = lifecycle::instance_create("acme", "workers").unwrap();
    let mut state2 = read_instance(&fs, &id2);
    assert_eq!(state2.status, InstanceStatus::Created);

    // Try to warm a Created instance (invalid: Created → Warm)
    let result2 = lifecycle::instance_warm("acme", "workers", &id2);
    assert!(result2.is_err(), "Expected error for Created→Warm");

    // Stopped → Warm is also invalid
    state2.status = InstanceStatus::Stopped;
    write_instance(&fs, &state2);
    let result3 = lifecycle::instance_warm("acme", "workers", &id2);
    assert!(result3.is_err(), "Expected error for Stopped→Warm");
}

// ---------------------------------------------------------------------------
// Test 3: Quota enforcement
// ---------------------------------------------------------------------------

#[test]
fn test_quota_enforcement() {
    // Tight quota: only 4 vCPUs, 2048 MiB, 2 running
    let tight_quota = TenantQuota {
        max_vcpus: 4,
        max_mem_mib: 2048,
        max_running: 2,
        ..TenantQuota::default()
    };

    // Usage at the limit
    let at_limit = quota::TenantUsage {
        total_vcpus: 4,
        total_mem_mib: 2048,
        running_count: 2,
        ..Default::default()
    };

    // vCPU limit exceeded
    let result = quota::check_quota(&tight_quota, &at_limit, 2, 1024);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("vCPUs"));

    // Memory limit exceeded
    let mem_usage = quota::TenantUsage {
        total_vcpus: 0,
        total_mem_mib: 2000,
        running_count: 0,
        ..Default::default()
    };
    let result = quota::check_quota(&tight_quota, &mem_usage, 1, 1024);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("memory"));

    // Running count exceeded
    let run_usage = quota::TenantUsage {
        total_vcpus: 0,
        total_mem_mib: 0,
        running_count: 2,
        ..Default::default()
    };
    let result = quota::check_quota(&tight_quota, &run_usage, 1, 512);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("running"));

    // Under all limits — should succeed
    let ok_usage = quota::TenantUsage {
        total_vcpus: 2,
        total_mem_mib: 1024,
        running_count: 1,
        ..Default::default()
    };
    assert!(quota::check_quota(&tight_quota, &ok_usage, 2, 1024).is_ok());
}

// ---------------------------------------------------------------------------
// Test 4: Instance destroy cleans up all files
// ---------------------------------------------------------------------------

#[test]
fn test_instance_destroy_cleanup() {
    let (_guard, fs) = setup_mock();

    let id = lifecycle::instance_create("acme", "workers").unwrap();
    let inst_dir = format!("/var/lib/mvm/tenants/acme/pools/workers/instances/{}", id);

    // Populate instance directory with typical runtime artifacts
    {
        let mut fs_lock = fs.lock().unwrap();
        fs_lock.insert(
            format!("{}/runtime/firecracker.socket", inst_dir),
            String::new(),
        );
        fs_lock.insert(format!("{}/runtime/fc.pid", inst_dir), "12345".into());
        fs_lock.insert(
            format!("{}/runtime/firecracker.log", inst_dir),
            "boot log".into(),
        );
        fs_lock.insert(format!("{}/runtime/v.sock", inst_dir), String::new());
        fs_lock.insert(format!("{}/volumes/data.ext4", inst_dir), "data".into());
        fs_lock.insert(
            format!("{}/volumes/secrets.ext4", inst_dir),
            "secrets".into(),
        );
        fs_lock.insert(
            format!("{}/snapshots/delta/vmstate.delta.bin", inst_dir),
            "snap".into(),
        );
        fs_lock.insert(
            format!("{}/snapshots/delta/mem.delta.bin", inst_dir),
            "mem".into(),
        );
    }

    // Count files before destroy
    let before_count = fs
        .lock()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&inst_dir))
        .count();
    assert!(
        before_count >= 5,
        "Expected at least 5 instance files, found {}",
        before_count
    );

    // Destroy with wipe_volumes=true
    lifecycle::instance_destroy("acme", "workers", &id, true).unwrap();

    // All instance files should be gone
    let remaining: Vec<String> = fs
        .lock()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&inst_dir))
        .cloned()
        .collect();
    assert!(
        remaining.is_empty(),
        "Expected no instance files after destroy(wipe=true), found: {:?}",
        remaining
    );

    // Tenant and pool configs should still exist
    assert!(
        fs.lock()
            .unwrap()
            .contains_key("/var/lib/mvm/tenants/acme/tenant.json")
    );
    assert!(
        fs.lock()
            .unwrap()
            .contains_key("/var/lib/mvm/tenants/acme/pools/workers/pool.json")
    );
}

// ---------------------------------------------------------------------------
// Test 5: Network identity is preserved across lifecycle transitions
// ---------------------------------------------------------------------------

#[test]
fn test_network_identity_preserved() {
    let (_guard, fs) = setup_mock();

    let id = lifecycle::instance_create("acme", "workers").unwrap();
    let initial = read_instance(&fs, &id);
    let original_ip = initial.net.guest_ip.clone();
    let original_mac = initial.net.mac.clone();
    let original_tap = initial.net.tap_dev.clone();
    let original_gateway = initial.net.gateway_ip.clone();
    let original_cidr = initial.net.cidr;

    // Sanity: network fields are populated
    assert!(original_ip.starts_with("10.240.3."));
    assert!(original_mac.starts_with("02:"));
    assert!(!original_tap.is_empty());
    assert_eq!(original_gateway, "10.240.3.1");
    assert_eq!(original_cidr, 24);

    // Simulate start → Running
    simulate_start(&fs, &id);

    // Running → Warm
    lifecycle::instance_warm("acme", "workers", &id).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.net.guest_ip, original_ip, "IP changed after warm");
    assert_eq!(state.net.mac, original_mac, "MAC changed after warm");
    assert_eq!(state.net.tap_dev, original_tap, "TAP changed after warm");

    // Warm → Sleeping
    lifecycle::instance_sleep("acme", "workers", &id, true).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.net.guest_ip, original_ip, "IP changed after sleep");
    assert_eq!(state.net.mac, original_mac, "MAC changed after sleep");
    assert_eq!(state.net.tap_dev, original_tap, "TAP changed after sleep");

    // Sleeping → Stopped
    lifecycle::instance_stop("acme", "workers", &id).unwrap();
    let state = read_instance(&fs, &id);
    assert_eq!(state.net.guest_ip, original_ip, "IP changed after stop");
    assert_eq!(state.net.mac, original_mac, "MAC changed after stop");
    assert_eq!(state.net.tap_dev, original_tap, "TAP changed after stop");
    assert_eq!(
        state.net.gateway_ip, original_gateway,
        "Gateway changed after stop"
    );
    assert_eq!(state.net.cidr, original_cidr, "CIDR changed after stop");
}
