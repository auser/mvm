//! mvm-sdk — build-time Rust SDK for declaring mvm workloads.
//!
//! Per ADR-0015: builder-pattern surface (no globals), build-time DSL
//! only in v1 (no `Session`/`RemoteFunction`), corpus byte-identity
//! gates release.
//!
//! # Two-layer architecture (ADR-0003)
//!
//! - **Lower layer:** the IR types are re-exported as-is from
//!   `mvm-ir`. No codegen; the Rust IR types are already
//!   `serde + JsonSchema + deny_unknown_fields`, which satisfies the
//!   schema-driven contract that Python and TypeScript SDKs achieve via
//!   `datamodel-code-generator` / `json-schema-to-typescript`.
//! - **Upper layer:** hand-authored builders rooted at [`workload`] and
//!   [`app`].
//!
//! # Subprocess contract (ADR-0002)
//!
//! [`emit`] honors `MVM_IR_OUT`: when set, writes the canonical IR
//! to that path and returns `Ok(())`. When unset, writes to stdout.
//! Validation errors and write errors return non-zero through
//! [`EmitError`].
//!
//! # Example
//!
//! ```no_run
//! use mvm_sdk::*;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let wl = workload("hello")
//!         .app(
//!             app("hello")
//!                 .source(local_path("."))
//!                 .image(nix_packages(["python312"]))
//!                 .entrypoint(entrypoint_command(["python", "-m", "hello"]))
//!                 .resources(resources(1, 256, 512))
//!                 .build()?,
//!         )
//!         .build()?;
//!     emit(&wl)?;
//!     Ok(())
//! }
//! ```

mod builder;
mod ctor;
mod emit;
mod error;

// Prelude — every previously-public item lives here so
// `use mvm_sdk::*;` resolves identically across the split.
pub use builder::{app, workload, AppBuilder, WorkloadBuilder};
pub use ctor::deps::{no_deps, node_deps, node_deps_with, python_deps, python_deps_with};
pub use ctor::entrypoint::{entrypoint_command, entrypoint_function, EntrypointExt};
pub use ctor::image::{nix_packages, oci_base};
pub use ctor::network::{
    dns_none, dns_resolver, dns_system, egress, host_port, network, NetworkExt,
};
pub use ctor::resources::resources;
pub use ctor::source::{local_path, nix_derivation, oci_image};
pub use emit::{emit, emit_json};
pub use error::{BuildError, EmitError};

// IR type re-exports — public surface aliases consumed by downstream
// fixtures (the corpus byte-identity gate from ADR-0015) and tests.
pub use mvm_ir::{
    ir_hash, App as IrApp, Dependencies as IrDependencies, Entrypoint as IrEntrypoint,
    EnvValue as IrEnvValue, Format as IrFormat, HostPort, Image as IrImage, Mount as IrMount,
    MountMode, MountSource, Network as IrNetwork, NetworkDns as IrNetworkDns,
    NetworkEgress as IrNetworkEgress, NetworkMode as IrNetworkMode, NodeTool as IrNodeTool,
    PortForward as IrPortForward, PortProto, PythonTool as IrPythonTool, Resources as IrResources,
    SecretMount, SecretRef, Source as IrSource, ValidationError, Volume as IrVolume,
    Workload as IrWorkload,
};
