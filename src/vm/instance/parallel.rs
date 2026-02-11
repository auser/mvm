use tracing::{info, warn};

use super::lifecycle;

/// Default concurrency limit for parallel instance operations.
const DEFAULT_CONCURRENCY: usize = 4;

/// Result of a parallel operation on a batch of instances.
#[derive(Debug, Default)]
pub struct BatchResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl BatchResult {
    pub fn success_count(&self) -> usize {
        self.succeeded.len()
    }

    pub fn failure_count(&self) -> usize {
        self.failed.len()
    }
}

/// Start multiple instances in parallel using a thread pool.
///
/// Each instance is started in its own thread (via `rayon` or manual threading).
/// Since lifecycle operations use `run_in_vm` which is sync (blocking shell commands),
/// we use `std::thread` for true parallelism.
pub fn parallel_start(
    tenant_id: &str,
    pool_id: &str,
    instance_ids: &[String],
    max_concurrent: Option<usize>,
) -> BatchResult {
    let concurrency = max_concurrent.unwrap_or(DEFAULT_CONCURRENCY);
    let mut result = BatchResult::default();

    info!(
        tenant_id,
        pool_id,
        count = instance_ids.len(),
        concurrency,
        "Starting instances in parallel"
    );

    // Process in chunks of `concurrency`
    for chunk in instance_ids.chunks(concurrency) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|id| {
                let t = tenant_id.to_string();
                let p = pool_id.to_string();
                let i = id.clone();
                std::thread::spawn(move || {
                    let res = lifecycle::instance_start(&t, &p, &i);
                    (i, res)
                })
            })
            .collect();

        for handle in handles {
            match handle.join() {
                Ok((id, Ok(_))) => result.succeeded.push(id),
                Ok((id, Err(e))) => {
                    warn!(instance_id = %id, error = %e, "Failed to start instance");
                    result.failed.push((id, e.to_string()));
                }
                Err(_) => {
                    warn!("Instance start thread panicked");
                    result
                        .failed
                        .push(("unknown".to_string(), "thread panicked".to_string()));
                }
            }
        }
    }

    info!(
        started = result.success_count(),
        failed = result.failure_count(),
        "Parallel start complete"
    );

    result
}

/// Stop multiple instances in parallel using a thread pool.
pub fn parallel_stop(
    tenant_id: &str,
    pool_id: &str,
    instance_ids: &[String],
    max_concurrent: Option<usize>,
) -> BatchResult {
    let concurrency = max_concurrent.unwrap_or(DEFAULT_CONCURRENCY);
    let mut result = BatchResult::default();

    info!(
        tenant_id,
        pool_id,
        count = instance_ids.len(),
        concurrency,
        "Stopping instances in parallel"
    );

    for chunk in instance_ids.chunks(concurrency) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|id| {
                let t = tenant_id.to_string();
                let p = pool_id.to_string();
                let i = id.clone();
                std::thread::spawn(move || {
                    let res = lifecycle::instance_stop(&t, &p, &i);
                    (i, res)
                })
            })
            .collect();

        for handle in handles {
            match handle.join() {
                Ok((id, Ok(_))) => result.succeeded.push(id),
                Ok((id, Err(e))) => {
                    warn!(instance_id = %id, error = %e, "Failed to stop instance");
                    result.failed.push((id, e.to_string()));
                }
                Err(_) => {
                    warn!("Instance stop thread panicked");
                    result
                        .failed
                        .push(("unknown".to_string(), "thread panicked".to_string()));
                }
            }
        }
    }

    info!(
        stopped = result.success_count(),
        failed = result.failure_count(),
        "Parallel stop complete"
    );

    result
}

/// Create multiple instances in parallel, returning their IDs.
pub fn parallel_create(
    tenant_id: &str,
    pool_id: &str,
    count: usize,
    max_concurrent: Option<usize>,
) -> BatchResult {
    let concurrency = max_concurrent.unwrap_or(DEFAULT_CONCURRENCY);
    let mut result = BatchResult::default();

    info!(
        tenant_id,
        pool_id, count, concurrency, "Creating instances in parallel"
    );

    // Create in sequential batches because instance_create assigns sequential IPs
    // that depend on each other. Use concurrency for the starts, not creates.
    for _ in 0..count {
        match lifecycle::instance_create(tenant_id, pool_id) {
            Ok(id) => result.succeeded.push(id),
            Err(e) => {
                warn!(error = %e, "Failed to create instance");
                result.failed.push(("new".to_string(), e.to_string()));
            }
        }
    }

    // Now start all created instances in parallel
    if !result.succeeded.is_empty() {
        let start_result = parallel_start(tenant_id, pool_id, &result.succeeded, Some(concurrency));
        for (id, err) in start_result.failed {
            result.failed.push((id, err));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_result_default() {
        let result = BatchResult::default();
        assert_eq!(result.success_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_batch_result_counts() {
        let result = BatchResult {
            succeeded: vec!["a".to_string(), "b".to_string()],
            failed: vec![("c".to_string(), "err".to_string())],
        };
        assert_eq!(result.success_count(), 2);
        assert_eq!(result.failure_count(), 1);
    }

    #[test]
    fn test_default_concurrency() {
        assert_eq!(DEFAULT_CONCURRENCY, 4);
    }
}
