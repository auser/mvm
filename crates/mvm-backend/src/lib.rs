//! mvm-backend — concrete `VmBackend` implementations.
//!
//! Plan-60 W7+W8 ended this crate's life as a re-export façade.
//! Every concrete backend now lives here:
//!
//! - **Firecracker** (`backend::FirecrackerBackend`) + the `AnyBackend`
//!   dispatch enum + `FirecrackerConfig` — the production Tier 1 path.
//! - **Apple Container** (`apple_container::AppleContainerBackend`) —
//!   macOS 26+ Apple Virtualization.framework.
//! - **Cloud Hypervisor** (`cloud_hypervisor::CloudHypervisorBackend`)
//!   — Tier 1 KVM peer of Firecracker (opt-in).
//! - **Docker** (`docker::DockerBackend`) — Tier 3 fallback.
//! - **libkrun** (`libkrun::LibkrunBackend`) — raw libkrun shim
//!   (Linux KVM / macOS HVF).
//! - **microsandbox** (`microsandbox::MicrosandboxBackend`) —
//!   libkrun-backed sandbox; Phase 1 default.
//! - **microvm.nix** (`microvm_nix::MicrovmNixBackend`) — Firecracker
//!   via the upstream microvm.nix runner.
//!
//! Plus the FC support modules: `firecracker` (installer helpers),
//! `microvm` (lifecycle), `image` (Mvmfile.toml), `network` (TAP/
//! bridge wiring).
//!
//! ## Dependency direction (post-W8)
//!
//!   mvm-core              ← VmBackend trait + types
//!   mvm-runtime-base      ← config + shell + linux_env + ui +
//!                           runtime_meta + cow (substrate)
//!   mvm-providers         ← libkrun/Apple-VZ FFI shims
//!     ↓                     ↓                     ↓
//!     └─────── mvm-backend (this crate) ────────┘
//!                          ↑
//!                     mvm-runtime
//!                     (consumes us via `vm::backend::AnyBackend`)
//!                     mvm-cli
//!                     (consumes us directly)

pub mod apple_container;
pub mod backend;
pub mod cloud_hypervisor;
pub mod docker;
pub mod firecracker;
pub mod handle_registry;
pub mod image;
pub mod libkrun;
pub mod microsandbox;
pub mod microvm;
pub mod microvm_nix;
pub mod network;

pub use apple_container::AppleContainerBackend;
pub use backend::{AnyBackend, FirecrackerBackend, FirecrackerConfig};
pub use cloud_hypervisor::CloudHypervisorBackend;
pub use docker::DockerBackend;
pub use libkrun::LibkrunBackend;
pub use microsandbox::MicrosandboxBackend;
pub use microvm_nix::{MicrovmNixBackend, MicrovmNixConfig};

/// Crate-wide test serialization for tests that mutate `HOME` or
/// other process-global env vars. Re-exported from
/// [`mvm_runtime_base::runtime_meta::HOME_TEST_LOCK`] so the
/// alt-backend tests share the same mutex with `mvm-runtime` tests
/// — without sharing one lock the modules race each other when
/// their tests run on the same `cargo test` binary.
#[cfg(test)]
pub(crate) use mvm_runtime_base::runtime_meta::HOME_TEST_LOCK;

