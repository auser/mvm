// mvm-runtime: Shell execution and VM lifecycle
// Depends on mvm-core, mvm-guest, mvm-build, mvm-runtime-base.

pub mod build_env;
pub mod config;
pub mod linux_env;
pub mod security;
pub mod shell;
pub mod storage;
pub mod vsock_transport;

pub mod vm;

// `ui` lives in `mvm-runtime-base` (W7 substrate split). Re-exported
// here so `mvm_runtime::ui::*` (and `mvmctl::runtime::ui::*` via the
// facade) continue to resolve for downstream consumers — notably
// mvmd, which imports `mvmctl::runtime::ui` from
// `mvmd-cli/src/dev_cluster.rs` and `mvmd-runtime/src/build_env.rs`.
pub use mvm_runtime_base::ui;

// Legacy re-export — preserve `mvm_runtime::shell_mock::*` path.
pub use shell::mock as shell_mock;
