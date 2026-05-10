// Plan-60 W7+W8 split:
//
//   * Every concrete `VmBackend` impl + the FC support modules
//     (apple_container, cloud_hypervisor, docker, libkrun,
//     microsandbox, firecracker, microvm, microvm_nix, image,
//     network, backend) live in `mvm-backend`.
//   * The leaf substrate (`ui`, `runtime_meta`, `cow`,
//     `snapshot_integrity`) lives in `mvm-runtime-base`.
//
// What's left here is the orchestration layer — instance/pool/
// template/tenant lifecycle, name + volume registries, the egress
// proxy, vminitd client, and the host-side rootfs snapshot helper.

pub mod egress_proxy;
pub mod instance_snapshot;
pub mod name_registry;
pub mod template;
pub mod vminitd_client;
pub mod volume_registry;

// Substrate re-exports — preserve the `mvm_runtime::vm::{cow,
// runtime_meta}::*` paths for back-compat (mvmd's `mvmctl::runtime`
// surface and the W6.2 console gate consume them).
pub use mvm_runtime_base::{cow, runtime_meta};

// Backend re-exports — preserve the `mvm_runtime::vm::*` paths so
// the in-tree mvm-cli/mvm-supervisor consumers don't all migrate at
// once. New code should reach `mvm_backend::*` directly; W8.B.3
// migrated the load-bearing call sites already.
pub use mvm_backend::{
    apple_container, backend, cloud_hypervisor, docker, firecracker, image, libkrun,
    microsandbox, microvm, microvm_nix, network,
};

/// Crate-wide test serialization for tests that mutate
/// `MVM_DATA_DIR` (and thus rely on a process-global env var).
/// Tests that mutate `HOME` use
/// [`mvm_runtime_base::runtime_meta::HOME_TEST_LOCK`] instead.
/// Tests that mutate other process-globals can grab their own lock
/// or pile onto one of these — the goal is "no two tests touch the
/// same env var concurrently."
#[cfg(test)]
pub(crate) static DATA_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
