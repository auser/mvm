//! OCI image distribution client for mvm.
//!
//! Plan 74 W1 (`specs/plans/74-claim-safe-sandbox-parity.md` §W1)
//! materializes OCI images into microVM-bootable templates without a
//! host Docker daemon. This crate owns the *distribution* half of
//! that pipeline:
//!
//! - **Reference parsing.** `<registry>/<repository>[:<tag>][@<digest>]`
//!   strings normalize to a structured [`ImageReference`], which knows
//!   whether it's tag-pinned or digest-pinned. Production-profile
//!   admission (plan 74 W1.6) rejects tag-pinned references.
//! - **Manifest fetch.** The [`ManifestFetcher`] trait fronts the
//!   actual registry call; [`OciManifestFetcher`] is the real impl
//!   over [`oci_client`]. Test code can stand in a fixture
//!   implementation without bringing up a registry.
//! - **Digest verification.** Every fetched manifest's content digest
//!   is verified before it leaves the fetcher.
//!   [`verify_sha256_digest`] is the standalone primitive — algorithm
//!   support is intentionally narrow (sha256 only in v1) so any
//!   future expansion is an explicit, reviewed decision.
//!
//! Layer fetch, ext4 unpacking (plan 74 §Risks R10 attack surface),
//! verity generation (ADR-050), template registration, and the
//! `mvmctl image pull` CLI surface land in subsequent W1 PRs.
//! Private-registry authentication (Bearer / `.docker/config.json`)
//! is also a later PR — W1.1 ships anonymous-only by design so the
//! credential-as-secret handling can be reviewed in isolation against
//! ADR-049's substitution machinery.
//!
//! Crate-level invariant: this crate **only** speaks the OCI
//! distribution wire format. It does not touch the host filesystem,
//! does not invoke `veritysetup`, and does not call into mvm's
//! template registry. Those interactions live in `mvm-build` (rootfs
//! materialization), `mvm-core` (template registry), and `mvm-cli`
//! (user-facing commands). Keeping the boundary tight is what lets
//! `mvmd` consume `mvm-oci` as a library without inheriting the
//! Nix-flake builder closure (ADR-048 §"Runtime Ownership"; the
//! cross-repo handoff to mvmd is tracked in issue #222).

#![forbid(unsafe_code)]

pub mod error;
pub mod manifest;
pub mod reference;

pub use error::OciError;
pub use manifest::{FetchedManifest, ManifestFetcher, OciManifestFetcher, verify_sha256_digest};
pub use reference::ImageReference;
