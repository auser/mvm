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

#[cfg(target_os = "macos")]
pub mod apple_container;

#[cfg(not(target_os = "macos"))]
pub mod apple_container {
    //! Stub: Apple Virtualization.framework only exists on macOS.
    //! Non-macOS targets get a no-op surface so cross-platform
    //! callers don't need `cfg(target_os = "macos")` at every site.
    pub fn is_available() -> bool { false }
}
