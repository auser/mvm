//! Stateful, lazy `.build()`-terminated builders for the top-level IR
//! types ([`Workload`](mvm_ir::Workload), [`App`](mvm_ir::App)).

pub(crate) const SCHEMA_VERSION: &str = "0.1";

mod app;
mod workload;

pub use app::{AppBuilder, app};
pub use workload::{WorkloadBuilder, workload};
