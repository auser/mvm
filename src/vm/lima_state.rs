use std::sync::OnceLock;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::lima::{self, LimaStatus};
use crate::infra::config;

/// Tracks whether we have already verified (and possibly started) the Lima VM.
static LIMA_READY: OnceLock<bool> = OnceLock::new();

/// Ensure the Lima VM is running before executing VM-side operations.
///
/// On the first call, checks Lima status and starts it if stopped.
/// Subsequent calls return immediately (cached via `OnceLock`).
/// Returns an error if Lima is not found or cannot be started.
pub fn ensure_lima_ready() -> Result<()> {
    let ready = LIMA_READY.get_or_init(|| match check_and_start_lima() {
        Ok(()) => true,
        Err(e) => {
            warn!(error = %e, "Lima VM not ready");
            false
        }
    });

    if *ready {
        Ok(())
    } else {
        anyhow::bail!(
            "Lima VM '{}' is not available. Run 'mvm setup' or 'mvm bootstrap' first.",
            config::VM_NAME
        )
    }
}

fn check_and_start_lima() -> Result<()> {
    let status = lima::get_status().with_context(|| "Failed to check Lima VM status")?;

    match status {
        LimaStatus::Running => {
            info!("Lima VM is running");
            Ok(())
        }
        LimaStatus::Stopped => {
            info!("Lima VM is stopped, starting...");
            lima::start().with_context(|| "Failed to start Lima VM")?;
            info!("Lima VM started");
            Ok(())
        }
        LimaStatus::NotFound => {
            anyhow::bail!("Lima VM not found. Run 'mvm setup' first.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lima_ready_static_exists() {
        // Just verify the OnceLock compiles and is accessible
        let _ = LIMA_READY.get();
    }
}
