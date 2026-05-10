//! mvm-runtime-base — shared substrate for `mvm-runtime` + `mvm-backend`.
//!
//! Plan-60 W7 lifted the substrate that backend implementations needed
//! out of `mvm-runtime` so the concrete `VmBackend` impls could live in
//! `mvm-backend` without a back-edge into `mvm-runtime`. The split is
//! deliberately conservative: only modules with truly leaf-shaped
//! dependencies (no Lima coupling, no `mvm-runtime` internals) live
//! here. The rest of the substrate — `config`, `shell`, `linux_env`,
//! `vm::microvm`, `vm::image` — stays in `mvm-runtime` until the W8
//! Firecracker direct-launch rewrite unwinds the Lima coupling.

pub mod cow;
pub mod runtime_meta;
pub mod ui;
