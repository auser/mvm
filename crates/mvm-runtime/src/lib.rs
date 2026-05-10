// mvm-runtime — VM lifecycle orchestration on top of `mvm-runtime-base`.
//
// Plan-60 W8 lifted the leaf substrate (`config`, `shell`, `linux_env`,
// `ui`, `runtime_meta`, `cow`) into `mvm-runtime-base` so the FC backend
// stack can move into `mvm-backend` without a back-edge. The
// `pub use mvm_runtime_base::*` re-exports below preserve the old
// `mvm_runtime::{config, shell, linux_env, ui, shell_mock}` paths so
// the mvmd contract surface (consumed via `mvmctl::runtime::*`) keeps
// resolving without forcing a sibling-repo update.

pub mod build_env;
pub mod security;
pub mod storage;
pub mod vsock_transport;

pub mod vm;

// Substrate re-exports — see crate doc comment.
pub use mvm_runtime_base::{config, linux_env, shell, ui};

// Legacy re-export — preserves `mvm_runtime::shell_mock::*` (used by
// mvmd's `mvmd-agent::quic_integration` test and a handful of
// `mvmd-runtime` security modules).
pub use mvm_runtime_base::shell_mock;
