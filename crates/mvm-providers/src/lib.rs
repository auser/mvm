//! mvm-providers — virtualization-framework primitives.
//!
//! Internal FFI / SDK shim layer. Each module wraps one underlying
//! virtualization framework:
//!
//!   - [`libkrun`]         — Red Hat libkrun C library (Linux KVM, macOS HVF)
//!   - [`apple_container`] — Apple Virtualization.framework (macOS only)
//!
//! [`mvm-backend`](https://docs.rs/mvm-backend) consumes these modules
//! to implement the `VmBackend` trait. End-user code should never
//! depend on this crate directly — it's an implementation detail of
//! the backend layer.
//!
//! # The naming question
//!
//! ADR-012 documents a separate, public-facing "Provider" concept
//! (e.g. `linux`, `mlx`) that lives in mvmd. The two share a name
//! but address different layers: this crate is *internal FFI*; ADR-012
//! talks about *user-selectable execution targets*. The disambiguation
//! note in ADR-012 carries the full story.

pub mod libkrun;

// `apple_container` is unconditionally exposed; the module itself uses
// `#[cfg(target_os = "macos")]` to gate the Virtualization.framework
// implementation behind `mod macos;` and provides non-macOS fallbacks
// at each public entry point. Cross-platform callers can therefore
// reference `mvm_providers::apple_container::*` without sprinkling
// `cfg` guards at every call site.
pub mod apple_container;
