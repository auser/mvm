//! Integration tests for the warm-process worker pool (plan 43).
//!
//! Drives a real `WorkerPool` against the `fake-runner` test binary
//! to exercise the recycling paths (call-count, wrapper crash) and
//! the FIFO queue. Tests run on macOS dev hosts and Linux equally —
//! the only Linux-specific bit is `/proc/<pid>/statm` RSS sampling,
//! which the cross-platform tests in this file don't depend on. RSS
//! recycling lives in the Linux-gated test below.
//!
//! These tests never touch `handle_run_entrypoint` or the cold-tier
//! M12 lock; the cold path is unaffected.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mvm_guest::entrypoint::ValidatedEntrypoint;
use mvm_guest::runtime_config::{InProcessMode, WarmProcessConfig};
use mvm_guest::worker_pool::{DispatchOutcome, SlotSnapshot, WorkerPool};
use mvm_guest::worker_protocol::WorkerOutcome;

/// Cargo sets `CARGO_BIN_EXE_<name>` at compile time for integration
/// tests, pointing at the built test binary.
const FAKE_RUNNER: &str = env!("CARGO_BIN_EXE_fake-runner");

fn validated_for_fake_runner() -> ValidatedEntrypoint {
    let path = PathBuf::from(FAKE_RUNNER);
    let file = File::open(&path).expect("fake-runner exists");
    ValidatedEntrypoint {
        resolved: path,
        file,
    }
}

fn cfg(pool_size: usize, max_calls: u64, max_rss_mb: u64) -> WarmProcessConfig {
    WarmProcessConfig {
        max_calls_per_worker: max_calls,
        max_rss_mb,
        pool_size,
        in_process: InProcessMode::Serial,
        max_queue_depth: None,
    }
}

fn start_pool(cfg: WarmProcessConfig) -> Arc<WorkerPool> {
    start_pool_with_behavior(cfg, None)
}

fn start_pool_with_behavior(cfg: WarmProcessConfig, behavior: Option<&str>) -> Arc<WorkerPool> {
    let entry = Arc::new(validated_for_fake_runner());
    let env: Vec<(String, String)> = behavior
        .map(|b| vec![("MVM_FAKE_RUNNER_BEHAVIOR".to_string(), b.to_string())])
        .unwrap_or_default();
    WorkerPool::start(cfg, entry, env).expect("pool start")
}

fn dispatch(pool: &Arc<WorkerPool>, payload: &[u8]) -> DispatchOutcome {
    pool.dispatch(payload.to_vec(), 30)
        .expect("dispatch returns outcome")
}

fn idle_pid(pool: &Arc<WorkerPool>) -> u32 {
    let snap = pool.snapshot();
    snap.iter()
        .find_map(|s| match s {
            SlotSnapshot::Idle { pid, .. } => Some(*pid),
            _ => None,
        })
        .expect("at least one idle slot")
}

fn idle_call_count(pool: &Arc<WorkerPool>, pid: u32) -> u64 {
    let snap = pool.snapshot();
    snap.iter()
        .find_map(|s| match s {
            SlotSnapshot::Idle {
                pid: p, call_count, ..
            } if *p == pid => Some(*call_count),
            _ => None,
        })
        .unwrap_or(0)
}

#[test]
fn warm_process_round_trip_pid_stable() {
    // SAFETY: tests in this binary run sequentially? No — Cargo runs
    // them in parallel by default. Each test that sets env vars must
    // either coordinate or use a separate behavior flag. For "ok"
    // (default) we don't need to set anything.
    let pool = start_pool(cfg(1, 100, 1024));
    let pid_before = idle_pid(&pool);

    let out1 = dispatch(&pool, b"hello");
    assert_eq!(out1.stdout, b"hello");
    assert!(matches!(out1.outcome, WorkerOutcome::Exit { code: 0 }));

    let out2 = dispatch(&pool, b"world");
    assert_eq!(out2.stdout, b"world");
    assert!(matches!(out2.outcome, WorkerOutcome::Exit { code: 0 }));

    assert_eq!(idle_pid(&pool), pid_before, "PID stable across calls");
    assert_eq!(idle_call_count(&pool, pid_before), 2);
}

#[test]
fn user_code_fault_does_not_recycle() {
    // The fake runner returns Exit { code: 1 } on every call; the
    // pool must NOT recycle on user-code-level failure.
    let pool = start_pool_with_behavior(cfg(1, 100, 1024), Some("user_fault"));
    let pid_before = idle_pid(&pool);

    for _ in 0..20 {
        let out = dispatch(&pool, b"x");
        assert!(matches!(out.outcome, WorkerOutcome::Exit { code: 1 }));
    }
    assert_eq!(idle_pid(&pool), pid_before, "PID stable for user faults");
    assert_eq!(idle_call_count(&pool, pid_before), 20);
}

#[test]
fn recycle_on_call_count_exceeded() {
    let pool = start_pool(cfg(1, 3, 1024)); // recycle after 3 calls
    let first_pid = idle_pid(&pool);

    for _ in 0..3 {
        let out = dispatch(&pool, b"a");
        assert!(matches!(out.outcome, WorkerOutcome::Exit { code: 0 }));
    }
    // After 3 calls, the next call should be served by a fresh
    // worker because release saw call_count >= max.
    let second_pid = idle_pid(&pool);
    assert_ne!(second_pid, first_pid, "worker recycled after call cap");
}

#[test]
fn wrapper_crash_returns_error_and_recycles() {
    // crash_during_call_n=2: call 1 succeeds, call 2 crashes
    // (worker exits without writing response). After recycle, the
    // fresh worker also has crash_during_call_n=2 set (the env is
    // baked into the pool), so we only assert through to call 2's
    // crash. PID changes proves the recycle fired.
    let pool = start_pool_with_behavior(cfg(1, 100, 1024), Some("crash_during_call_n=2"));
    let first_pid = idle_pid(&pool);

    let out1 = pool
        .dispatch(b"first".to_vec(), 5)
        .expect("dispatch returns outcome");
    assert!(matches!(out1.outcome, WorkerOutcome::Exit { code: 0 }));

    let out2 = pool
        .dispatch(b"second".to_vec(), 5)
        .expect("dispatch returns outcome even on crash");
    assert!(
        matches!(out2.outcome, WorkerOutcome::Error { .. }),
        "out2 outcome = {:?}",
        out2.outcome
    );
    if let WorkerOutcome::Error { kind, .. } = out2.outcome {
        assert_eq!(kind, "wrapper_crash");
    }

    // Recycle should have replaced the worker.
    let second_pid = idle_pid(&pool);
    assert_ne!(second_pid, first_pid, "crashed worker replaced");
}

#[test]
fn pool_size_4_parallel_dispatches() {
    let pool = start_pool(cfg(4, 100, 1024));
    let pids_before: std::collections::HashSet<u32> = pool
        .snapshot()
        .into_iter()
        .filter_map(|s| match s {
            SlotSnapshot::Idle { pid, .. } => Some(pid),
            _ => None,
        })
        .collect();
    assert_eq!(pids_before.len(), 4);

    // Fire 12 dispatches across 12 threads.
    let pool_clone = Arc::clone(&pool);
    let mut handles = Vec::new();
    for i in 0..12 {
        let p = Arc::clone(&pool_clone);
        handles.push(std::thread::spawn(move || {
            let payload = format!("call-{i}");
            let out = p.dispatch(payload.clone().into_bytes(), 30).expect("ok");
            assert_eq!(out.stdout, payload.as_bytes());
            assert!(matches!(out.outcome, WorkerOutcome::Exit { code: 0 }));
        }));
    }
    for h in handles {
        h.join().expect("thread completes");
    }

    // All 4 workers should still be the same PIDs — call_count below
    // recycle threshold.
    let pids_after: std::collections::HashSet<u32> = pool
        .snapshot()
        .into_iter()
        .filter_map(|s| match s {
            SlotSnapshot::Idle { pid, .. } => Some(pid),
            _ => None,
        })
        .collect();
    assert_eq!(pids_after, pids_before);
}

#[test]
fn fifo_queue_serializes_callers() {
    // pool_size=1, max_queue_depth=4 + 5 concurrent dispatches:
    // 1 in-flight, up to 4 queued, total 5 fits exactly. All
    // complete; the queue serializes them.
    let mut wpc = cfg(1, 100, 1024);
    wpc.max_queue_depth = Some(4);
    let pool = start_pool(wpc);
    let pool_clone = Arc::clone(&pool);
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..5u8 {
        let p = Arc::clone(&pool_clone);
        handles.push(std::thread::spawn(move || {
            p.dispatch(vec![i], 5).expect("dispatch")
        }));
    }
    let outs: Vec<DispatchOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();
    assert_eq!(outs.len(), 5);
    for out in &outs {
        assert!(matches!(out.outcome, WorkerOutcome::Exit { code: 0 }));
    }
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "5 quick calls under pool_size=1 finish promptly"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn recycle_on_rss_exceeded() {
    // Linux-only: relies on /proc/<pid>/statm. The fake runner
    // allocates 50 MiB per call; max_rss_mb=10 forces recycle on
    // call 1. The replacement also has leak_50mib_per_call (env
    // baked into pool), but call 1 alone is enough to verify the
    // recycle fired.
    let pool = start_pool_with_behavior(cfg(1, 100, 10), Some("leak_50mib_per_call"));
    let first_pid = idle_pid(&pool);

    let _ = dispatch(&pool, b"call-1");
    // After call 1, RSS should be over 10 MiB → worker recycled.
    let second_pid = idle_pid(&pool);
    assert_ne!(
        second_pid, first_pid,
        "worker recycled after RSS exceeded cap"
    );
}

#[test]
fn queue_full_returns_error() {
    // pool_size=1, max_queue_depth=1. With one in-flight slow caller
    // and one queued, the third caller must be rejected with QueueFull.
    let mut wpc = cfg(1, 100, 1024);
    wpc.max_queue_depth = Some(1);
    let pool = start_pool_with_behavior(wpc, Some("slow_secs=2"));

    let p1 = Arc::clone(&pool);
    let p2 = Arc::clone(&pool);
    let h1 = std::thread::spawn(move || p1.dispatch(b"a".to_vec(), 10));
    // Give the first dispatch time to enter the worker (occupy slot).
    std::thread::sleep(Duration::from_millis(200));
    let h2 = std::thread::spawn(move || p2.dispatch(b"b".to_vec(), 10));
    // Give the second dispatch time to enter the queue.
    std::thread::sleep(Duration::from_millis(200));

    // Third should hit QueueFull.
    let res = pool.dispatch(b"c".to_vec(), 10);
    assert!(
        res.is_err(),
        "third dispatch must hit queue cap, got {:?}",
        res
    );

    let _ = h1.join().expect("h1");
    let _ = h2.join().expect("h2");
}
