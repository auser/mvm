// Backend impls — the concrete `VmBackend` implementations live
// here for now; plan-60 W7 moves the alt backends to `mvm-backend`,
// W8 will move Firecracker + MicrovmNix once the config/shell/
// `vm::microvm` substrate's Lima coupling is unwound.

pub mod apple_container;
pub mod backend;
pub mod cloud_hypervisor;
pub mod cow;
pub mod docker;
pub mod egress_proxy;
pub mod firecracker;
pub mod image;
pub mod instance_snapshot;
pub mod libkrun;
pub mod microsandbox;
pub mod microvm;
pub mod microvm_nix;
pub mod name_registry;
pub mod network;
pub mod template;

// `runtime_meta` lives in `mvm-runtime-base` (W7 substrate split).
// Re-exported here so existing `mvm_runtime::vm::runtime_meta::*`
// imports keep resolving — notably `mvm-cli/commands/vm/console.rs`
// and the W6.2 console gate's call sites.
pub use mvm_runtime_base::runtime_meta;
pub mod vminitd_client;
pub mod volume_registry;

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
