use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable colored output (for interactive CLI use).
    Human,
    /// Structured JSON output (for daemon/agent mode).
    Json,
}

/// Initialize the global tracing subscriber.
///
/// Call once at program startup. Respects `RUST_LOG` env var for filtering.
/// Default filter: `mvm=info` (show info+ from mvm, warnings from dependencies).
pub fn init(format: LogFormat) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("mvm=info,warn"));

    match format {
        LogFormat::Human => {
            let subscriber = fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .compact();
            tracing_subscriber::registry()
                .with(env_filter)
                .with(subscriber)
                .init();
        }
        LogFormat::Json => {
            let subscriber = fmt::layer().json().with_target(true);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(subscriber)
                .init();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_format_equality() {
        assert_eq!(LogFormat::Human, LogFormat::Human);
        assert_eq!(LogFormat::Json, LogFormat::Json);
        assert_ne!(LogFormat::Human, LogFormat::Json);
    }
}
