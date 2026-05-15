//! Linux builder VM bootstrap (libkrun-backed).
//!
//! Implements the contract documented in ADR-013 §"Linux builder via
//! libkrun (no Lima)": on hosts that can't `nix build` Linux
//! derivations natively (macOS without `nix-darwin`'s `linux-builder`,
//! Windows-via-WSL2 without an in-WSL Nix install, Linux without host
//! Nix), `mvmctl build` bootstraps a small Linux builder microVM from
//! a pinned OCI image, runs `nix build` inside it, and extracts the
//! resulting rootfs back to the host.
//!
//! ## Status
//!
//! **Scaffolding.** The contract types and the 6-step flow are
//! locked; the actual bootstrap (OCI pull + sandbox spawn + bind-mount
//! wiring + artifact extraction) lands in a follow-up wave. Today
//! every method returns [`BuilderVmError::NotYetImplemented`] with a
//! pointer to the ADR section. Callers can wire the dispatch and
//! cover the error path in tests; the data-plane fills in
//! incrementally.
//!
//! ## Trust boundary
//!
//! The builder VM lives in a different trust zone than runtime VMs.
//! It pulls from network, runs arbitrary Nix derivations, and bind-
//! mounts the host's `/nix/store` for cache reuse. ADR-013's
//! "non-goal: OCI" applies to the **runtime** path; OCI is
//! deliberately *acceptable* for the builder. See
//! ADR-013 §"Linux builder via libkrun (no Lima)" for the
//! rationale.

use std::path::{Path, PathBuf};

use mvm_core::build_env::ShellEnvironment;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Pinned Nix-bearing OCI image. Bumped deliberately; the per-bump
/// audit (`xtask audit-flake` for flake inputs has a sister
/// `xtask audit-builder-image` that lands with the bootstrap impl)
/// re-checks the image's CVE surface.
///
/// `nixos/nix` is the upstream Nix project's image; we may switch to
/// a self-published image once we want to pin an exact substituter
/// configuration into the image rather than configure it at spawn
/// time.
pub const BUILDER_OCI_IMAGE: &str = "docker.io/nixos/nix:2.24.10";

/// SHA-256 digest the bootstrap verifies against after pull.
/// Empty until the bootstrap impl pins the digest in CI; an empty
/// expected-digest means "skip verification" (dev-only).
pub const BUILDER_OCI_DIGEST_SHA256: &str = "";

/// Cache directory for the pulled builder image, relative to the
/// user's cache root. Matches ADR-013 §"Linux builder…" step 2.
pub const BUILDER_IMAGE_CACHE_SUBDIR: &str = "builder-image";

/// Mount layout for a builder sandbox. ADR-013 step 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderMounts {
    /// User's flake source. Bind-mounted read-only at `/work`.
    pub flake_src: PathBuf,
    /// Host's `/nix/store`. Bind-mounted read-write at `/nix` so
    /// builds populate the cache and subsequent builds reuse it.
    /// `None` means "use a fresh in-sandbox store" (slower; first
    /// build pulls everything from substituters).
    pub host_nix_store: Option<PathBuf>,
    /// Writable artifact extraction directory. Bind-mounted at
    /// `/out`; the builder writes the rootfs + metadata sidecar
    /// here. ADR-013 step 5 extracts from this path back to the
    /// host's per-build artifact directory.
    pub artifact_out: PathBuf,
}

/// What the builder is asked to produce.
///
/// Plan 73 Followup B.2.0 generalised this from a single nix-build
/// shape into an enum so the same trait can dispatch both the
/// existing flake builds (`Flake`) and the application-dependency
/// install pipeline (`Install`) that Followup B.2 will wire. Each
/// variant pairs 1:1 with a [`BuilderArtifacts`] variant — see the
/// per-variant docs there for the expected outputs.
///
/// This is plumbing only: the `Install` variant is reserved here so
/// B.2 can land behavior changes against a stable shape, and today
/// every backend errors with [`BuilderVmError::NotYetImplemented`]
/// when it sees an `Install` job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderJob {
    /// Build a Nix flake attribute. The flake attribute path is
    /// system-specific; callers map host architecture to the
    /// matching Linux system (`aarch64-linux` on Apple Silicon,
    /// `x86_64-linux` on Intel/AMD).
    Flake {
        /// Flake reference (e.g. `git+file:///work?dir=.`, `.#default`,
        /// `path:./.`).
        flake_ref: String,
        /// Attribute path under the flake (e.g.
        /// `packages.x86_64-linux.tenant-worker`). Resolved by callers
        /// before invoking the builder.
        attr_path: String,
    },

    /// Application-dependency install pipeline (ADR-047, Followup
    /// B.2). The builder VM reads a serialised install spec from
    /// `spec_path` (lockfile + source-root + ecosystem + gate),
    /// runs the corresponding package manager (`uv pip install
    /// --no-deps`, `pnpm install --frozen-lockfile`, …) inside the
    /// VM, seals the resulting volume with SBOM + fetch log + CVE +
    /// attestations, and emits a `result.json` next to the volume.
    ///
    /// **Today every backend errors with
    /// [`BuilderVmError::NotYetImplemented`] for this variant.**
    /// Plan 73 Followup B.2 wires the libkrun backend; the
    /// libkrun backend never gets this variant.
    Install {
        /// Absolute host path to the install-spec JSON the builder
        /// VM reads at start-up. Followup B.2 defines the shape;
        /// today the orchestrator does not produce one.
        spec_path: PathBuf,
    },
}

/// Result of a successful build. The variant returned matches the
/// [`BuilderJob`] variant the caller passed: a `Flake` job yields
/// [`BuilderArtifacts::Image`]; an `Install` job yields
/// [`BuilderArtifacts::InstallVolume`].
///
/// Mirrors the host-backend's `BackendBuildResult` shape so the
/// runtime path can consume both transparently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderArtifacts {
    /// Output of a [`BuilderJob::Flake`] build — a kernel +
    /// rootfs pair ready for boot, plus revision metadata.
    Image {
        /// Absolute host path to the extracted rootfs (typically
        /// `~/.mvm/dev/builds/<rev>/rootfs.ext4`).
        rootfs_path: PathBuf,
        /// Optional kernel image path (some flakes emit one; verity
        /// initramfs is paired with the kernel).
        kernel_path: Option<PathBuf>,
        /// Nix store revision hash (the leading `<hash>` segment of
        /// the derivation's output store path). Used as the artifact
        /// dir name and for cache lookups.
        revision_hash: String,
        /// `flake.lock` SHA-256, recorded for cache tracking.
        lock_hash: Option<String>,
        /// `passthru.mvm.accessible` — wires through to
        /// `runtime_meta.accessible`, populating the W6.2 console
        /// gate. `None` means the flake didn't surface the field;
        /// callers default to `true` for backward compatibility
        /// (W6.2's same default).
        accessible: Option<bool>,
    },

    /// Output of a [`BuilderJob::Install`] run — a sealed deps
    /// volume on the host filesystem plus a structured result
    /// document. Followup B.2 will fill this in; today no backend
    /// constructs this variant.
    InstallVolume {
        /// Directory the builder VM sealed the application-deps
        /// volume into (content + SBOM + fetch log + CVE scan +
        /// attestations + meta). Caller hashes this with
        /// `mvm_sdk::compile::deps_audit::verify_sealed_volume` to
        /// derive the canonical `volume_hash`.
        volume_dir: PathBuf,
        /// JSON sidecar emitted by `mvm-builder-init` next to the
        /// volume describing the install outcome (exit code,
        /// installer stderr tail, timings). Shape pinned by
        /// Followup B.2.
        result_json_path: PathBuf,
    },
}

/// Filename for the sidecar manifest written next to a built
/// rootfs. Mirrors `passthru.mvm` from `mkGuest` so the runtime
/// path can populate `runtime_meta` (W6.2) without re-running
/// `nix eval`. Living next to the rootfs keeps the sidecar
/// atomic with the artifact — a stale sidecar on the filesystem
/// without a matching rootfs is impossible.
pub const SIDECAR_FILENAME: &str = "mvm-meta.json";

/// Wire-format mirror of `mkGuest`'s `passthru.mvm`. Build paths
/// emit this; runtime paths consume it.
///
/// Field names are camelCase to match the Nix passthru shape
/// directly — a future `nix eval --json` path can dump straight
/// into this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSidecar {
    /// Name from `mkGuest { name = …; }`.
    pub name: String,
    /// Whether `mvmctl console` may attach. Drives the W6.2 gate.
    pub accessible: bool,
    /// Inverse of `accessible` — sealed images refuse exec/console.
    pub sealed: bool,
    /// Form of the entrypoint declaration: "shell", "command", or
    /// "services". Information; not load-bearing for runtime gates.
    pub entrypoint_kind: String,
    /// Init system in use; "busybox" today (W5.1).
    pub init_system: String,
    /// Per-backend boot floor in milliseconds (ADR-013 §"Per-backend
    /// boot budgets"). Used by perf gates to flag regressions.
    pub expected_boot_ms: u32,
    /// Agent binary kind: "stub" (W6.1.1 placeholder) or "real"
    /// (W6.1.2 cross-compiled Rust). Production policies should
    /// require "real".
    pub agent_binary: String,
    /// Whether the entrypoint runs as a non-root uid.
    pub rootless_entrypoint: bool,
    /// Active hypervisor declaration.
    pub hypervisor: String,
}

impl ArtifactSidecar {
    /// Path the sidecar lives at, given a directory containing the
    /// rootfs. Single source of truth for both writers and readers.
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(SIDECAR_FILENAME)
    }

    /// Write the sidecar JSON to `dir/mvm-meta.json`. Creates the
    /// directory if missing. Errors propagate — sidecar writes are
    /// load-bearing for the W6.2 gate, unlike `runtime_meta::write`
    /// which is best-effort.
    pub fn write_to_dir(&self, dir: &Path) -> Result<PathBuf, std::io::Error> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir);
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, format!("{body}\n"))?;
        Ok(path)
    }

    /// Read the sidecar from a directory. Returns `Ok(None)` if the
    /// sidecar doesn't exist (pre-W7.x.1 build artifacts; runtime
    /// path falls through to the W6.2 default-accessible behavior).
    /// Errors only on malformed JSON.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, anyhow::Error> {
        let path = Self::path_in(dir);
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::Error::new(e)),
        };
        let sidecar: Self = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(Some(sidecar))
    }
}

/// Builder VM driver. Today this is a marker trait shape — the
/// concrete impl arrives with the bootstrap wave. Defining it now
/// lets call sites be wired against the future API and lets tests
/// cover the error path.
pub trait BuilderVm {
    /// Steps 2-5 (ADR-013): pull the OCI image (if not cached),
    /// spawn a sandbox with the given mounts, run `nix build` for
    /// the job, and extract artifacts to `mounts.artifact_out`.
    /// Idempotent w.r.t. the image cache; not idempotent w.r.t. the
    /// artifact dir (caller cleans up).
    ///
    /// There is no host-Nix fallback. The CLAUDE.md invariant —
    /// *"Host Nix is never used by mvmctl, even when present"* —
    /// rules out a `host_can_build`-style probe: every Nix evaluation
    /// must go through a VM we launched, so we always take the
    /// builder-VM path.
    fn run_build(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError>;

    /// Tear down any persistent state (warm builder pool entries,
    /// pulled images older than N days, etc.). No-op for stateless
    /// implementations.
    fn cleanup(&self) -> Result<(), BuilderVmError> {
        Ok(())
    }
}

/// Errors from the builder VM.
#[derive(Debug, Error)]
pub enum BuilderVmError {
    /// Bootstrap is not implemented yet. Returned by the stub impl
    /// until the follow-up wave fills in the data plane.
    #[error(
        "libkrun-as-Linux-builder bootstrap is in flight; \
         see ADR-013 §\"Linux builder via libkrun (no Lima)\" \
         for the design and Sprint 50 for the schedule. \
         For now, install host Nix (Determinate Nix or upstream) \
         or configure `nix-darwin`'s `linux-builder`."
    )]
    NotYetImplemented,

    /// Libkrun isn't installed or isn't on PATH.
    #[error("libkrun not available: {0}")]
    LibkrunUnavailable(String),

    /// OCI image pull failed (network, registry auth, digest
    /// mismatch). Wraps the underlying error.
    #[error("OCI image pull failed: {0}")]
    ImagePullFailed(String),

    /// `nix build` returned non-zero inside the sandbox.
    #[error("nix build failed inside builder sandbox: {0}")]
    NixBuildFailed(String),

    /// Artifact extraction failed (missing rootfs, permissions,
    /// extraction-dir issue).
    #[error("extracting artifacts from builder sandbox: {0}")]
    ExtractionFailed(String),
}

/// Stub implementation. Every method returns
/// [`BuilderVmError::NotYetImplemented`]. Kept around for tests that
/// want a `BuilderVm` impl with deterministic error behavior;
/// production code uses [`LibkrunBuilderVm`].
#[derive(Debug, Default, Clone, Copy)]
pub struct StubBuilderVm;

impl BuilderVm for StubBuilderVm {
    fn run_build(
        &self,
        _job: &BuilderJob,
        _mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        Err(BuilderVmError::NotYetImplemented)
    }
}

/// Resolve the host architecture's matching Linux system for flake
/// attribute construction. Mirrors `mvm-build/src/backend/host.rs`'s
/// `resolve_build_attribute_host`'s system selection.
pub fn host_system_linux() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

/// Best-effort sidecar emission: query `passthru.mvm` against the
/// already-built flake/attr and write it to
/// `<build_dir>/mvm-meta.json` so the consumer in
/// `mvm::vm::runtime_meta::from_sidecar` can populate
/// `accessible` for the W6.2 console gate.
///
/// Failure modes (all log+continue, never fail the build):
/// - Flake doesn't surface `passthru.mvm` (older mkGuest, third-
///   party flakes): query returns non-zero → log warning
/// - `nix` not on PATH: query errors → log warning
/// - JSON shape doesn't match `ArtifactSidecar` (drift between
///   mkGuest and our wire type): parse error → log warning
/// - Disk write fails: log warning
///
/// The consumer side (W6.2) defaults `accessible: true` when the
/// sidecar is missing, so a logged warning here is the only
/// user-visible signal — the build still succeeds.
///
/// `dev_override` and `impure_flag` are passed through verbatim to
/// the underlying invocation so the dev path's `mvm` flake
/// override (which requires `--impure`) is honored. The mvmd /
/// orchestrated path passes empty strings.
pub fn emit_sidecar_via_passthru_query(
    env: &dyn ShellEnvironment,
    attr: &str,
    build_dir: &str,
    dev_override: &str,
    impure_flag: &str,
) {
    let passthru_attr = format!("{}.passthru.mvm", attr);
    let cmd = format!(
        "nix eval --json {}{}{}",
        shell_quote(&passthru_attr),
        impure_flag,
        dev_override,
    );
    let json = match env.shell_exec_stdout(&cmd) {
        Ok(s) => s,
        Err(e) => {
            env.log_warn(&format!(
                "sidecar: nix eval passthru.mvm failed (console gate stays accessible-by-default): {e}"
            ));
            return;
        }
    };
    let sidecar: ArtifactSidecar = match serde_json::from_str(json.trim()) {
        Ok(s) => s,
        Err(e) => {
            env.log_warn(&format!(
                "sidecar: passthru.mvm shape doesn't match ArtifactSidecar (mkGuest drift?): {e}"
            ));
            return;
        }
    };
    match sidecar.write_to_dir(Path::new(build_dir)) {
        Ok(path) => env.log_info(&format!("Wrote sidecar: {}", path.display())),
        Err(e) => env.log_warn(&format!("sidecar: write failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_image_is_namespaced() {
        // Sanity: pinning a top-level image like `nix:2.24.10` is
        // ambiguous across registries. The constant must include the
        // registry + namespace.
        assert!(
            BUILDER_OCI_IMAGE.starts_with("docker.io/")
                || BUILDER_OCI_IMAGE.starts_with("ghcr.io/")
                || BUILDER_OCI_IMAGE.starts_with("registry."),
            "image must be fully qualified: {BUILDER_OCI_IMAGE}"
        );
        assert!(
            BUILDER_OCI_IMAGE.contains(':'),
            "image must carry a tag: {BUILDER_OCI_IMAGE}"
        );
    }

    #[test]
    fn host_system_is_linux() {
        let s = host_system_linux();
        assert!(s.ends_with("-linux"), "got {s}");
    }

    #[test]
    fn stub_returns_not_yet_implemented_for_run_build() {
        let stub = StubBuilderVm;
        let job = BuilderJob::Flake {
            flake_ref: ".".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let mounts = BuilderMounts {
            flake_src: PathBuf::from("/tmp/flake"),
            host_nix_store: None,
            artifact_out: PathBuf::from("/tmp/out"),
        };
        let err = stub.run_build(&job, &mounts).expect_err("stub returns err");
        assert!(matches!(err, BuilderVmError::NotYetImplemented));
    }

    #[test]
    fn stub_returns_not_yet_implemented_for_install_job() {
        // The B.2.0 plumbing reserves the Install variant; until B.2
        // wires behavior, every backend (including the stub) must
        // surface NotYetImplemented for it.
        let stub = StubBuilderVm;
        let job = BuilderJob::Install {
            spec_path: PathBuf::from("/tmp/spec.json"),
        };
        let mounts = BuilderMounts {
            flake_src: PathBuf::from("/tmp/flake"),
            host_nix_store: None,
            artifact_out: PathBuf::from("/tmp/out"),
        };
        let err = stub.run_build(&job, &mounts).expect_err("stub returns err");
        assert!(matches!(err, BuilderVmError::NotYetImplemented));
    }

    #[test]
    fn cleanup_default_is_ok() {
        // Stateless implementations get a free no-op cleanup.
        assert!(StubBuilderVm.cleanup().is_ok());
    }

    #[test]
    fn error_message_points_at_recovery_path() {
        let err = BuilderVmError::NotYetImplemented;
        let msg = err.to_string();
        assert!(msg.contains("libkrun builder") && msg.contains("does not use host Nix"));
    }

    fn fixture_sidecar() -> ArtifactSidecar {
        ArtifactSidecar {
            name: "test-vm".to_string(),
            accessible: true,
            sealed: false,
            entrypoint_kind: "shell".to_string(),
            init_system: "busybox".to_string(),
            expected_boot_ms: 300,
            agent_binary: "stub".to_string(),
            rootless_entrypoint: false,
            hypervisor: "libkrun".to_string(),
        }
    }

    #[test]
    fn sidecar_write_then_read_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sidecar = fixture_sidecar();
        let path = sidecar.write_to_dir(tmp.path()).expect("write");
        assert_eq!(path, tmp.path().join(SIDECAR_FILENAME));
        let read = ArtifactSidecar::read_from_dir(tmp.path())
            .expect("read")
            .expect("present");
        assert_eq!(read, sidecar);
    }

    #[test]
    fn sidecar_read_missing_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = ArtifactSidecar::read_from_dir(tmp.path()).expect("ok");
        assert!(result.is_none());
    }

    #[test]
    fn sidecar_read_malformed_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(SIDECAR_FILENAME), "{not valid json")
            .expect("write malformed");
        let result = ArtifactSidecar::read_from_dir(tmp.path());
        assert!(result.is_err(), "malformed sidecar should error");
    }

    #[test]
    fn sidecar_uses_camel_case_on_disk() {
        // The on-disk format mirrors `passthru.mvm` so a future
        // `nix eval --json` path can dump straight into this struct.
        // Asserting the field names guards against accidental rename.
        let tmp = tempfile::tempdir().expect("tempdir");
        fixture_sidecar().write_to_dir(tmp.path()).expect("write");
        let body = std::fs::read_to_string(tmp.path().join(SIDECAR_FILENAME)).expect("read raw");
        assert!(body.contains("\"entrypointKind\""), "got: {body}");
        assert!(body.contains("\"expectedBootMs\""), "got: {body}");
        assert!(body.contains("\"agentBinary\""), "got: {body}");
        assert!(body.contains("\"rootlessEntrypoint\""), "got: {body}");
        // The accessible field is the W6.2 wire — check it's present.
        assert!(body.contains("\"accessible\""), "got: {body}");
    }
}
