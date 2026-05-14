//! OCI layer unpack to a staging rootfs directory.
//!
//! [`ImageStaging`] orchestrates the unpack:
//!
//! 1. Caller creates an `ImageStaging` rooted at a directory.
//! 2. Caller calls `apply_layer` once per OCI layer, in the order
//!    the manifest lists them. The unpacker applies tar entries
//!    with security checks layered on top of the standard `tar`
//!    crate extraction (path traversal, symlink/hardlink escape,
//!    reserved-path collision, size caps, unsupported entry
//!    types).
//! 3. Caller calls `finalize` to release the staging directory and
//!    get a [`StagedRootfs`] descriptor pointing at it. ext4
//!    materialization (W1.3b) consumes that descriptor next.
//!
//! Layers are assumed to arrive *decompressed*. The caller (W1.5
//! CLI orchestrator) is responsible for piping fetched layer bytes
//! through the right decoder based on the manifest's
//! `mediaType` — gzip via `flate2::read::GzDecoder`, zstd via
//! `zstd::stream::Decoder`, plain tar passes through. Keeping the
//! decoder choice out of the unpack module means hostile-tar tests
//! don't need to compress every fixture.
//!
//! ## Whiteouts
//!
//! OCI whiteouts (`<dir>/.wh.<name>` for "delete `<name>` from
//! previous layers", `<dir>/.wh..wh..opq` for "clear this
//! directory's contents from previous layers") are processed
//! inline. The whiteout marker file itself is never written to
//! staging; its effect is to remove the corresponding path.
//!
//! ## Plan-74 R10 attack-surface coverage
//!
//! The integration tests under `mvm-build/tests/oci_unpack_*` are
//! the canonical R10 mitigation evidence. Every variant in
//! [`OciUnpackError`] has at least one negative test that proves
//! the corresponding attack class is rejected without a partial
//! write landing in staging.

pub mod error;
mod path_validation;
mod unpack;

pub use error::OciUnpackError;
pub use unpack::{ImageStaging, LayerStats, StagedRootfs, StagingOptions};
