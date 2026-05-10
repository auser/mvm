//! mvm-backend — concrete `VmBackend` implementations.
//!
//! Plan-60 W7 ended this crate's life as a re-export façade. Five
//! concrete backends now live here directly — Apple Container, Cloud
//! Hypervisor, Docker, raw libkrun, and microsandbox. Each module
//! owns its `VmBackend` impl plus its tests.
//!
//! ## Dependency direction (post-W7)
//!
//!   mvm-core              ← VmBackend trait + types
//!   mvm-runtime-base      ← ui + runtime_meta + cow (substrate)
//!   mvm-providers         ← libkrun/Apple-VZ FFI shims
//!     ↓                     ↓                     ↓
//!     └─────── mvm-backend (this crate) ────────┘
//!                          ↑
//!                     mvm-runtime
//!                     (consumes us via `vm::backend::AnyBackend`)
//!
//! ## What does *not* live here yet (W8)
//!
//! `FirecrackerBackend`, `MicrovmNixBackend`, the `AnyBackend` dispatch
//! enum, and the Lima-coupled `vm::firecracker`/`vm::microvm`/`vm::image`
//! helpers stay in `mvm-runtime` until the W8 direct-launch rewrite
//! collapses their `run_in_vm` calls into host-only operations. See
//! `specs/SPRINT.md` "Up next" for the W8 scope.

pub mod apple_container;
pub mod cloud_hypervisor;
pub mod docker;
pub mod handle_registry;
pub mod libkrun;
pub mod microsandbox;

pub use apple_container::AppleContainerBackend;
pub use cloud_hypervisor::CloudHypervisorBackend;
pub use docker::DockerBackend;
pub use libkrun::LibkrunBackend;
pub use microsandbox::MicrosandboxBackend;

/// Crate-wide test serialization for tests that mutate `HOME` or
/// other process-global env vars. Re-exported from
/// [`mvm_runtime_base::runtime_meta::HOME_TEST_LOCK`] so the
/// alt-backend tests share the same mutex with `mvm-runtime` tests
/// — without sharing one lock the modules race each other when
/// their tests run on the same `cargo test` binary.
#[cfg(test)]
pub(crate) use mvm_runtime_base::runtime_meta::HOME_TEST_LOCK;
