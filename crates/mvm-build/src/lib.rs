pub mod artifacts;
pub mod backend;
pub mod builder_vm;
pub mod cache;
pub mod firecracker;
pub mod template_reuse;

// Plan 72 W1 — `LibkrunBuilderVm` scaffolding behind
// `backends-builder-vm-libkrun`. The module name `libkrun_builder`
// disambiguates from `mvm-libkrun` (the FFI crate) so search-grep
// for "libkrun_builder" lands on the trait impl, not the bindings.
#[cfg(feature = "backends-builder-vm-libkrun")]
pub mod libkrun_builder;

pub mod nix;
pub mod pipeline;

// Legacy re-exports — preserve `mvm_build::build::*`, `mvm_build::scripts::*`, etc.
pub use nix::manifest as nix_manifest;
pub use nix::scripts;
pub use pipeline::{build, dev_build, orchestrator, vsock_builder};
