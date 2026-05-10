// Plan-60 W7 split:
//
//   * The 5 alt `VmBackend` impls (apple_container, cloud_hypervisor,
//     docker, libkrun, microsandbox) moved to `mvm-backend`.
//   * The leaf substrate (`ui`, `runtime_meta`, `cow`) moved to
//     `mvm-runtime-base`.
//
// What still lives here are the Firecracker-coupled pieces
// (`firecracker.rs` Lima-era helpers, `microvm.rs` lifecycle,
// `microvm_nix.rs`, the `AnyBackend` dispatch + `FirecrackerBackend`
// in `backend.rs`). W8 moves them once the Lima `run_in_vm`
// substrate is unwound.

pub mod backend;
pub mod egress_proxy;
pub mod firecracker;
pub mod image;
pub mod instance_snapshot;
pub mod microvm;
pub mod microvm_nix;
pub mod name_registry;
pub mod network;
pub mod template;
pub mod vminitd_client;
pub mod volume_registry;

// `runtime_meta` and `cow` live in `mvm-runtime-base` (W7 substrate
// split). Re-exported here so existing `mvm_runtime::vm::{cow,
// runtime_meta}::*` imports keep resolving — notably
// `mvm-cli/commands/vm/console.rs` and the W6.2 console gate's call
// sites for `runtime_meta`, and `vm::template::lifecycle` for `cow`.
pub use mvm_runtime_base::{cow, runtime_meta};

// The 5 alt backends live in `mvm-backend` (W7). Re-exported here so
// `mvm_runtime::vm::{apple_container, cloud_hypervisor, docker,
// libkrun, microsandbox}::*` paths keep resolving for any caller that
// addresses them by their old `vm::` location. New code should reach
// `mvm_backend::*` directly.
pub use mvm_backend::{apple_container, cloud_hypervisor, docker, libkrun, microsandbox};

/// Crate-wide test serialization for tests that mutate
/// `MVM_DATA_DIR` (and thus rely on a process-global env var).
/// Multiple modules under `vm/` use this — without sharing one
/// lock the modules race each other when their tests run on
/// the same `cargo test` binary.
///
/// W7 split: tests that mutate `HOME` use
/// [`mvm_runtime_base::runtime_meta::HOME_TEST_LOCK`] instead. Both
/// locks coexist because they guard different env vars; merging
/// would over-serialize unrelated tests.
#[cfg(test)]
pub(crate) static DATA_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
