// Backend impls — the concrete `VmBackend` implementations live
// here for now; plan-60 W6 schedules the move into mvm-backend
// once the config/shell/ui dependency-cycle is unwound. The
// `mvm-backend` crate exists today as a thin re-export facade so
// new consumers can use the stable path.

pub mod apple_container;
pub mod backend;
pub mod cow;
pub mod docker;
pub mod egress_proxy;
pub mod firecracker;
pub mod image;
pub mod instance_snapshot;
pub mod libkrun;
// Deprecated no-op shim — ADR-013 dropped Lima. The module exists
// only to keep mvm-cli's existing imports compiling; its functions
// all return "not running" / "no-op" / "NotFound." Cleanup wave
// removes the call sites + this module.
pub mod lima;
pub mod microsandbox;
pub mod microvm;
pub mod microvm_nix;
pub mod name_registry;
pub mod network;
pub mod template;
pub mod vminitd_client;
pub mod volume_registry;

/// Crate-wide test serialization for tests that mutate
/// `MVM_DATA_DIR` (and thus rely on a process-global env var).
/// Multiple modules under `vm/` use this — without sharing one
/// lock the modules race each other when their tests run on
/// the same `cargo test` binary.
#[cfg(test)]
pub(crate) static DATA_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
