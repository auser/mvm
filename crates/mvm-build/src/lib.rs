pub mod app_deps;
pub mod app_deps_gate;
pub mod artifacts;
pub mod backend;
/// Plan 89 W2 — vsock dispatch wire types for the persistent
/// builder VM. Today this is types + tests only; the dispatch path
/// itself wires in W2 PR 2 and the persistent-VM lifecycle in W3.
pub mod builder_protocol;
pub mod builder_vm;
pub mod cache;
pub mod cpio;
pub mod firecracker;
/// Plan 76 Phase 6 — portable signed `.mvm` artifacts. A tar.gz
/// wrapper around kernel + rootfs + verity sidecars + cmdline,
/// with an Ed25519-signed manifest that hashes every payload.
pub mod packed_artifact;
/// Plan 89 W3 part 1 — host-side scaffold for the persistent
/// builder VM's dispatch supervisor. Spawning the actual libkrun
/// VM lives in W3 part 2 (`LibkrunPersistentBuilderVm`); this
/// module owns the dispatch wire over the socket libkrun creates.
pub mod persistent_builder;
/// Plan 85 Phase B — OCI-unpacked tree to ext4 rootfs image.
/// The host only allocates the sparse file; formatting and copying
/// happen inside the existing builder VM.
pub mod rootfs;
pub mod stage0;
pub mod template_reuse;

/// Plan 72 W1 — libkrun-backed builder VM (gated by
/// `builder-vm`). Currently scaffolding; the actual
/// VM launch lands in Plan 72 W2–W4. See module-level docs.
#[cfg(feature = "builder-vm")]
pub mod libkrun_builder;

pub mod nix;
/// Plan 74 W1.3a — OCI layer unpack to a staging rootfs directory.
/// Handles whiteouts, symlinks, hardlinks, ownership, permissions,
/// path traversal, the `/mvm` reserved-path collision check
/// (ADR-051), and per-entry + per-layer size caps (plan 74 §Risks
/// R10 decompression-bomb mitigation). ext4 generation lives in
/// W1.3b (`mke2fs -d` against the staging dir, run inside the
/// builder VM per ADR-050).
pub mod oci_to_rootfs;
pub mod pipeline;
/// Plan 74 W1.4b — host-side resolver for the mvm runtime
/// overlay disk per ADR-051. Picks the right ext4 + verity
/// sidecar + roothash for the running mvmctl version and host
/// arch from `~/.cache/mvm/runtime-overlay/<version>/<arch>/`.
/// The Nix flake that *produces* the artifact lands in a
/// follow-up W1.4b PR; backend wiring (attaching the second
/// drive + threading `mvm.runtime_roothash=` into the cmdline)
/// lives in W1.4b.2; the `mkGuest` refactor that stops baking
/// the agent into per-image closures is W1.4b.3.
pub mod runtime_overlay;

// Legacy re-exports — preserve `mvm_build::build::*`, `mvm_build::scripts::*`, etc.
pub use nix::manifest as nix_manifest;
pub use nix::scripts;
pub use pipeline::{build, dev_build, orchestrator, vsock_builder};
