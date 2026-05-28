//! Wire protocol, signing, routing, and the VmBackend trait contract.

pub mod broker;
pub mod handler;
#[allow(clippy::module_inception)]
pub mod protocol;
pub mod routing;
pub mod signing;
pub mod vm_backend;

// Flatten protocol.rs contents up to `mvm_core::protocol::*`.
pub use self::protocol::*;
