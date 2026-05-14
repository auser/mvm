pub mod app_deps;
pub mod app_deps_gate;
pub mod artifacts;
pub mod backend;
pub mod builder_vm;
pub mod cache;
pub mod firecracker;
pub mod template_reuse;

/// Plan 72 W1 — libkrun-backed builder VM (gated by
/// `backends-builder-vm-libkrun`). Currently scaffolding; the actual
/// VM launch lands in Plan 72 W2–W4. See module-level docs.
#[cfg(feature = "backends-builder-vm-libkrun")]
pub mod libkrun_builder;

pub mod nix;
/// Plan 74 W1.3a — OCI layer unpack to a staging rootfs directory.
/// Handles whiteouts, symlinks, hardlinks, ownership, permissions,
/// path traversal, the `/mvm` reserved-path collision check
/// (ADR-051), and per-entry + per-layer size caps (plan 74 §Risks
/// R10 decompression-bomb mitigation). ext4 generation lives in
/// W1.3b (`mke2fs -d` against the staging dir, run inside the
/// builder VM per ADR-050).
pub mod oci_to_rootfs;
pub mod pipeline;

// Legacy re-exports — preserve `mvm_build::build::*`, `mvm_build::scripts::*`, etc.
pub use nix::manifest as nix_manifest;
pub use nix::scripts;
pub use pipeline::{build, dev_build, orchestrator, vsock_builder};
