//! Per-process registry of `StartMode::Attached` sandboxes.
//!
//! W7 closes the gap that's been documented in
//! [`microsandbox::start_with_mode`](crate::microsandbox) since
//! W6.2: previously, `mode` was *intent metadata* — recorded in
//! `~/.mvm/vms/<name>/mode.json` so subsequent calls knew the
//! caller's expectation, but the host process didn't actually
//! forward `Ctrl-C` (SIGINT) to the sandbox.
//!
//! This module owns the missing piece. Attached starts call
//! [`register_attached`]; the CLI's top-level signal handler calls
//! [`stop_all_attached`] to walk the registry and tear each
//! sandbox down gracefully. `stop`/`detach`/`stop_all` deregister
//! through [`deregister`] so we never try to stop something that's
//! already gone.
//!
//! ## Why this and not the `Sandbox` handle?
//!
//! microsandbox's `Sandbox` is async-bound and `!Send` across the
//! `VmBackend` sync trait boundary. Keeping the handle live across
//! `start_with_mode` → caller → SIGINT would force a `tokio::Runtime`
//! that outlives every backend call, plus a `Mutex<HashMap<VmId,
//! Sandbox>>` whose entries can't move between threads. The registry
//! sidesteps that: we keep just the *name* and call
//! [`microsandbox::Sandbox::get(name)`] from the signal handler,
//! reusing the same async bridge (`block_on`) the rest of the
//! backend does.
//!
//! ## What it does *not* do
//!
//! - It is not a process supervisor. If the parent process dies
//!   uncleanly (SIGKILL, panic without unwinding), attached
//!   sandboxes survive — microsandbox sandboxes always run as
//!   detached child processes at the OS level. Cleanup in that
//!   case is `mvmctl ps`/`mvmctl down` after restart.
//! - It is not cross-process. The registry is per-`mvmctl`-process;
//!   two concurrent `mvmctl up --attached` invocations don't see
//!   each other's sandboxes.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
// `VmBackend` + `VmId` are only used to call `MicrosandboxBackend::stop`
// when the feature is on. Gated with the import to avoid an unused-import
// warning on no-default-features builds.
#[cfg(feature = "contributor-bootstrap")]
use mvm_core::vm_backend::{VmBackend, VmId};

#[cfg(feature = "contributor-bootstrap")]
use crate::MicrosandboxBackend;

/// Metadata kept for each attached sandbox. The map's key is the
/// sandbox name, which is also the only thing the SIGINT handler
/// needs (`Sandbox::get(name).stop()`). The struct exists as the
/// value type so future entries can carry per-sandbox state (start
/// time, parent log file, etc.) without changing the signal-handler
/// protocol — the `#[allow(dead_code)]` `name` field is the
/// future-proofing seam.
#[derive(Debug, Clone)]
struct AttachedEntry {
    #[allow(dead_code)]
    name: String,
}

/// Registry of currently-attached sandbox names. Lazily initialized
/// on first use so a process that never starts an attached sandbox
/// pays no startup cost.
static REGISTRY: OnceLock<Mutex<HashMap<String, AttachedEntry>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, AttachedEntry>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an attached-mode sandbox so the SIGINT handler can find it.
///
/// Idempotent — re-registering the same name overwrites the entry.
pub fn register_attached(name: &str) {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(
        name.to_string(),
        AttachedEntry {
            name: name.to_string(),
        },
    );
}

/// Drop a sandbox from the registry. Called from `stop`/`detach`/
/// `stop_all` so the SIGINT handler doesn't try to stop something
/// that's already gone.
///
/// A missing entry is a no-op.
pub fn deregister(name: &str) {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.remove(name);
}

/// True if `name` is currently in the attached registry. Test
/// helper; production code shouldn't need this.
pub fn is_attached(name: &str) -> bool {
    let map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.contains_key(name)
}

/// Snapshot of currently-attached sandbox names. Caller-owned to
/// avoid holding the registry lock across the `Sandbox::get(...)`
/// async call.
fn attached_names() -> Vec<String> {
    let map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.keys().cloned().collect()
}

/// Stop every attached-mode sandbox in the registry. Best-effort:
/// errors stopping individual sandboxes are logged but don't block
/// the rest. Always clears the registry, even on partial failure.
///
/// Called from the host SIGINT handler. Returns the names that
/// were processed (whether or not the stop succeeded) so callers
/// can include them in shutdown logs.
pub fn stop_all_attached() -> Vec<String> {
    let names = attached_names();
    if names.is_empty() {
        return names;
    }
    #[cfg(feature = "contributor-bootstrap")]
    {
        let backend = MicrosandboxBackend;
        for name in &names {
            if let Err(e) = backend.stop(&VmId(name.clone())) {
                tracing::warn!(
                    sandbox = %name,
                    error = %e,
                    "ctrl-c: failed to stop attached sandbox; orphaned",
                );
            } else {
                tracing::info!(sandbox = %name, "ctrl-c: stopped attached sandbox");
            }
            deregister(name);
        }
    }
    #[cfg(not(feature = "contributor-bootstrap"))]
    {
        // Without the microsandbox backend, the registry is never
        // populated by any internal code — but `register_attached` is
        // a `pub fn` so an external caller could populate it anyway.
        // Drain defensively so the registry doesn't grow unbounded.
        for name in &names {
            deregister(name);
        }
    }
    names
}

/// Test-only helper that drains the registry without invoking the
/// real `MicrosandboxBackend::stop`. Lets the unit tests assert
/// `register/deregister` semantics without spawning sandboxes.
#[cfg(test)]
pub(crate) fn drain_for_test() -> Vec<String> {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let names: Vec<String> = map.keys().cloned().collect();
    map.clear();
    names
}

/// Public no-op kept on the public surface so callers (`mvm-cli`'s
/// SIGINT handler) can unconditionally call us regardless of whether
/// the microsandbox feature ever spawned anything.
///
/// Equivalent to `let _ = stop_all_attached()`. Exists so the
/// signal-handler call site doesn't need to discard the returned
/// `Vec<String>` explicitly.
pub fn stop_all_attached_silent() -> Result<()> {
    let _ = stop_all_attached();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests share the registry; the lock here ensures they don't
    /// race each other. Reuses `HOME_TEST_LOCK` because we want
    /// serialization with any other registry-touching test (no
    /// other lock needs).
    use mvm_base::runtime_meta::HOME_TEST_LOCK;

    #[test]
    fn register_then_deregister_is_idempotent() {
        let _g = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drain_for_test();

        register_attached("vm-a");
        assert!(is_attached("vm-a"));
        register_attached("vm-a"); // re-register overwrites
        assert!(is_attached("vm-a"));

        deregister("vm-a");
        assert!(!is_attached("vm-a"));
        deregister("vm-a"); // no-op on missing
        assert!(!is_attached("vm-a"));
    }

    #[test]
    fn drain_clears_registry() {
        let _g = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drain_for_test();

        register_attached("vm-1");
        register_attached("vm-2");
        register_attached("vm-3");

        let mut names = drain_for_test();
        names.sort();
        assert_eq!(names, vec!["vm-1", "vm-2", "vm-3"]);

        assert!(!is_attached("vm-1"));
        assert!(!is_attached("vm-2"));
        assert!(!is_attached("vm-3"));
    }

    #[test]
    fn stop_all_with_empty_registry_is_noop() {
        let _g = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drain_for_test();

        let names = stop_all_attached();
        assert!(names.is_empty());
    }

    #[test]
    fn stop_all_silent_returns_ok_when_empty() {
        let _g = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drain_for_test();

        assert!(stop_all_attached_silent().is_ok());
    }
}
