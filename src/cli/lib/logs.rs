use clap::Args;

use crate::utils::LogLevel;

#[derive(Debug, Clone, Default, Args)]
pub struct LogArgs {
    /// Show error-level diagnostic logs.
    #[arg(long, global = true, conflicts_with_all = ["warn", "info", "debug", "trace"])]
    pub error: bool,

    /// Show warning and error diagnostic logs.
    #[arg(long, global = true, conflicts_with_all = ["error", "info", "debug", "trace"])]
    pub warn: bool,

    /// Show info, warning, and error diagnostic logs.
    #[arg(long, global = true, conflicts_with_all = ["error", "warn", "debug", "trace"])]
    pub info: bool,

    /// Show debug and higher diagnostic logs.
    #[arg(long, global = true, conflicts_with_all = ["error", "warn", "info", "trace"])]
    pub debug: bool,

    /// Show all diagnostic logs (most verbose).
    #[arg(long, global = true, conflicts_with_all = ["error", "warn", "info", "debug"])]
    pub trace: bool,
}

impl LogArgs {
    pub const fn use_level(&self) -> Option<LogLevel> {
        if self.trace {
            Some(LogLevel::Trace)
        } else if self.debug {
            Some(LogLevel::Debug)
        } else if self.info {
            Some(LogLevel::Info)
        } else if self.warn {
            Some(LogLevel::Warn)
        } else if self.error {
            Some(LogLevel::Error)
        } else {
            None
        }
    }
}
