// Dev mode
pub mod apple_container;
pub mod backend;
pub mod docker;
pub mod egress_proxy;
pub mod firecracker;
pub mod image;
pub mod instance_snapshot;
pub mod lima;
pub mod lima_state;
pub mod microvm;
pub mod microvm_nix;
pub mod name_registry;
pub mod network;
pub mod share_registry;
pub mod template;
pub mod vminitd_client;

/// Crate-wide test serialization for tests that mutate
/// `MVM_DATA_DIR` (and thus rely on a process-global env var).
/// Multiple modules under `vm/` use this — without sharing one
/// lock the modules race each other when their tests run on
/// the same `cargo test` binary.
#[cfg(test)]
pub(crate) static DATA_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
