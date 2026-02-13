//! # mvm — Multi-tenant Firecracker microVM fleet manager
//!
//! Facade crate that re-exports the mvm workspace crates so consumers
//! can depend on a single `mvm` library.
//!
//! ## Crate breakdown
//!
//! | Module | Crate | Purpose |
//! |--------|-------|---------|
//! | [`core`] | mvm-core | Types, IDs, config, protocol, signing, routing |
//! | [`runtime`] | mvm-runtime | Shell execution, security, VM lifecycle |
//! | [`build`] | mvm-build | Nix builder pipeline |
//! | [`guest`] | mvm-guest | Vsock protocol, integration manifest |
//! | [`agent`] | mvm-agent | Reconcile engine, coordinator client |
//! | [`coordinator`] | mvm-coordinator | Gateway load-balancer, TCP proxy, wake manager |

pub use mvm_agent as agent;
pub use mvm_build as build;
pub use mvm_coordinator as coordinator;
pub use mvm_core as core;
pub use mvm_guest as guest;
pub use mvm_runtime as runtime;
