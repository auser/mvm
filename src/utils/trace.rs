use serde::{Deserialize, Serialize};
use tracing_subscriber::{EnvFilter, filter::Directive};

pub fn init_tracing(log_level: Option<LogLevel>, ansi: bool) {
    if let Some(level) = log_level {
        let filter = EnvFilter::new(level.as_tracing_level().to_string()).add_directive(
            "mvm-runtime=info"
                .parse::<Directive>()
                .expect("unable to unwrap tracing directive"),
        );

        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_ansi(ansi)
            .init();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Trace,
}

impl LogLevel {
    pub const fn as_tracing_level(self) -> tracing::Level {
        match self {
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
            LogLevel::Trace => tracing::Level::TRACE,
        }
    }
}
