//! Lima compatibility shim — DEPRECATED.
//!
//! ADR-013 dropped Lima entirely. This module exists as a thin shim
//! exposing the symbol shape of the previous `lima.rs` so that
//! callers in mvm-cli (and elsewhere) keep compiling while their
//! Lima-dependent branches are pruned in follow-up cleanup. Every
//! function here is a permanent no-op:
//!
//!   - `get_status()`   → returns `LimaStatus::NotFound`
//!   - `require_running()` → succeeds; no-op
//!   - `LimaStatus`     → kept as an enum (callers `match`/`Ok(…)` it)
//!
//! The macOS path is microsandbox+libkrun direct (ADR-013); the
//! `if needs_lima() { … }` branches across the codebase short-circuit
//! to `false` (see `Platform::needs_lima`) and the dead branches will
//! be deleted in a follow-up wave.

use anyhow::Result;

#[derive(Debug, PartialEq, Eq)]
pub enum LimaStatus {
    Running,
    Stopped,
    NotFound,
}

/// Always returns `NotFound`. Lima is no longer used.
pub fn get_status() -> Result<LimaStatus> {
    Ok(LimaStatus::NotFound)
}

/// Always returns `NotFound` for any name. Lima is no longer used.
pub fn get_vm_status(_vm_name: &str) -> Result<LimaStatus> {
    Ok(LimaStatus::NotFound)
}

/// No-op: succeeds without touching anything. The previous
/// implementation verified the Lima VM was running and started it
/// if not; today the macOS path is microsandbox-direct so there's
/// no Lima VM to verify.
pub fn require_running() -> Result<()> {
    Ok(())
}

/// No-op start. Existed to launch the Lima VM; now a stub.
pub fn start() -> Result<()> {
    Ok(())
}

/// No-op stop. Existed to shut down the Lima VM; now a stub.
pub fn stop() -> Result<()> {
    Ok(())
}

/// No-op destroy. Existed to delete the Lima VM; now a stub.
pub fn destroy() -> Result<()> {
    Ok(())
}

/// No-op ensure_running. Existed to bring the Lima VM up against a
/// generated lima.yaml; now a stub. The argument is accepted for
/// signature compatibility but ignored.
pub fn ensure_running<P: AsRef<std::path::Path>>(_lima_yaml: P) -> Result<()> {
    Ok(())
}
