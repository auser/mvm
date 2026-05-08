//! mvm-backend — `VmBackend` dispatch surface (re-export façade).
//!
//! ## Status
//!
//! Today this crate is a **thin re-export** of the dispatch types
//! (`AnyBackend`, `FirecrackerConfig`, etc.) that live in
//! `mvm_runtime::vm::backend`. The architectural intent is for the
//! concrete `VmBackend` *implementations* (`firecracker.rs`,
//! `microsandbox.rs`, `libkrun.rs`, `apple_container.rs`, `docker.rs`,
//! `microvm_nix.rs`) to live in this crate, with `mvm-runtime`
//! depending on us for them. **They don't yet** because they reach
//! back into `mvm_runtime::{config, shell, ui, vm::microvm, vm::image}`
//! at compile time, and breaking that coupling needs those modules
//! to either move down to a shared crate (likely `mvm-core` or a new
//! `mvm-runtime-shared`) or be replaced with caller-passed
//! abstractions.
//!
//! Plan-60 W6 carries the full extraction. Until then, this crate
//! exists so:
//!
//!   1. Workspace consumers (mvm-cli) can `use mvm_backend::AnyBackend`
//!      with a stable path that doesn't change when the impls finish
//!      moving.
//!   2. The user-facing facade (`mvmctl::backend`) has a real crate
//!      to point at.
//!   3. The backend boundary is documented in code, not just in plan
//!      docs — adding a new backend now starts here.
//!
//! ## What lives where (interim)
//!
//!   crates/mvm-runtime/src/vm/{firecracker,microsandbox,libkrun,
//!     apple_container,docker,microvm_nix,backend}.rs   ← impls
//!   crates/mvm-providers/src/{libkrun,apple_container}/  ← FFI shims
//!   crates/mvm-backend/src/lib.rs (this file)            ← re-exports

pub use mvm_runtime::vm::backend::{AnyBackend, FirecrackerConfig};
