//! Build & artifact commands — flake/Mvmfile builds + flake validation.
//! (Plan 40: `image` catalog moved to top-level `catalog` module;
//! `flake` validation renamed to `validate`.)

#[allow(clippy::module_inception)]
pub(super) mod build;
pub(super) mod compile;
/// Shared helpers for the SDK record-mode auto-exec path. Used by
/// `mvmctl compile <Sandbox-script>` and `mvmctl run --mode plan`.
pub(in crate::commands) mod sandbox_record;
pub(super) mod validate;

pub(super) use super::{Cli, shared};
