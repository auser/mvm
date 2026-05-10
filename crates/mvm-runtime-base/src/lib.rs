//! mvm-runtime-base — shared substrate for `mvm-runtime` + `mvm-backend`.
//!
//! Plan-60 W7+W8 lifted the substrate that backend implementations
//! needed out of `mvm-runtime` so the concrete `VmBackend` impls could
//! live in `mvm-backend` without a back-edge into `mvm-runtime`.
//!
//! ## What lives here
//!
//! | Module          | Purpose                                                |
//! |-----------------|--------------------------------------------------------|
//! | `ui`            | `[mvm]` printing + spinners + interactive prompts      |
//! | `runtime_meta`  | Per-VM `~/.mvm/vms/<name>/mode.json` (W6.2 console gate) |
//! | `cow`           | Reflink (CoW) file cloning + `clone_rootfs_for_instance` |
//! | `config`        | Builder VM name, FC network/path constants, wire types |
//! | `shell`         | Host + Linux-env command execution helpers             |
//! | `linux_env`     | Dispatch trait impls (NativeEnv, AppleContainerEnv)    |
//!
//! ## Re-exports kept by `mvm-runtime`
//!
//! `mvm-runtime`'s `lib.rs` re-exports the modules at their old
//! paths so the mvmd contract surface (`mvmctl::runtime::shell`,
//! `mvmctl::runtime::ui`, `mvmctl::runtime::shell_mock`) and the
//! W6.2 console gate (`mvm_runtime::vm::runtime_meta`) keep
//! resolving.

pub mod config;
pub mod cow;
pub mod linux_env;
pub mod runtime_meta;
pub mod shell;
pub mod snapshot_integrity;
pub mod ui;

// Legacy re-export: `mvm_runtime_base::shell_mock::*` matches the
// pre-W8 `mvm_runtime::shell_mock::*` path that mvmd's quic_integration
// test relies on.
pub use shell::mock as shell_mock;
