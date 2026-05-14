//! Linux builder VM bootstrap (microsandbox-backed).
//!
//! Implements the contract documented in ADR-013 §"Linux builder via
//! microsandbox (no Lima)": on hosts that can't `nix build` Linux
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
//! ADR-013 §"Linux builder via microsandbox (no Lima)" for the
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

/// What the builder is asked to produce. The flake attribute path is
/// system-specific; the bootstrap maps host architecture to the
/// matching Linux system (`aarch64-linux` on Apple Silicon,
/// `x86_64-linux` on Intel/AMD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderJob {
    /// Flake reference (e.g. `git+file:///work?dir=.`, `.#default`,
    /// `path:./.`).
    pub flake_ref: String,
    /// Attribute path under the flake (e.g.
    /// `packages.x86_64-linux.tenant-worker`). Resolved by callers
    /// before invoking the builder.
    pub attr_path: String,
}

/// Result of a successful build. Mirrors the host-backend's
/// `BackendBuildResult` shape so the runtime path can consume both
/// transparently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderArtifacts {
    /// Absolute host path to the extracted rootfs (typically
    /// `~/.mvm/dev/builds/<rev>/rootfs.ext4`).
    pub rootfs_path: PathBuf,
    /// Optional kernel image path (some flakes emit one; verity
    /// initramfs is paired with the kernel).
    pub kernel_path: Option<PathBuf>,
    /// Nix store revision hash (the leading `<hash>` segment of the
    /// derivation's output store path). Used as the artifact dir
    /// name and for cache lookups.
    pub revision_hash: String,
    /// `flake.lock` SHA-256, recorded for cache tracking.
    pub lock_hash: Option<String>,
    /// `passthru.mvm.accessible` — wires through to
    /// `runtime_meta.accessible`, populating the W6.2 console gate.
    /// `None` means the flake didn't surface the field; callers
    /// default to `true` for backward compatibility (W6.2's same
    /// default).
    pub accessible: Option<bool>,
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
    /// Step 1 (ADR-013): probe whether the local environment is
    /// already capable of a Linux build (host Nix + nix-darwin
    /// linux-builder, or Linux + host Nix). When `true`, the caller
    /// should fall through to `HostBackend` and skip the builder VM.
    fn host_can_build(&self) -> Result<bool, BuilderVmError>;

    /// Steps 2-5 (ADR-013): pull the OCI image (if not cached),
    /// spawn a sandbox with the given mounts, run `nix build` for
    /// the job, and extract artifacts to `mounts.artifact_out`.
    /// Idempotent w.r.t. the image cache; not idempotent w.r.t. the
    /// artifact dir (caller cleans up).
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
        "microsandbox-as-Linux-builder bootstrap is in flight; \
         see ADR-013 §\"Linux builder via microsandbox (no Lima)\" \
         for the design and Sprint 50 for the schedule. \
         For now, install host Nix (Determinate Nix or upstream) \
         or configure `nix-darwin`'s `linux-builder`."
    )]
    NotYetImplemented,

    /// Microsandbox isn't installed or isn't on PATH.
    #[error("microsandbox not available: {0}")]
    MicrosandboxUnavailable(String),

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

    /// The libkrun builder VM is scaffolded (Plan 72 W1) but the
    /// actual VM launch hasn't shipped yet. Plan 72 W2–W4 fill this
    /// in. Distinct from [`Self::NotYetImplemented`] (which is the
    /// microsandbox path's pre-Sprint-50 sentinel) so callers can
    /// match precisely on which side is incomplete.
    #[error(
        "libkrun builder VM launch is scaffolded but not implemented yet; \
         see specs/plans/72-builder-vm-via-libkrun.md §W2-W4"
    )]
    LibkrunNotShipped,
}

/// Stub implementation. Every method returns
/// [`BuilderVmError::NotYetImplemented`]. Kept around for tests that
/// want a `BuilderVm` impl with deterministic error behavior;
/// production code uses [`MicrosandboxBuilderVm`].
#[derive(Debug, Default, Clone, Copy)]
pub struct StubBuilderVm;

impl BuilderVm for StubBuilderVm {
    fn host_can_build(&self) -> Result<bool, BuilderVmError> {
        Err(BuilderVmError::NotYetImplemented)
    }
    fn run_build(
        &self,
        _job: &BuilderJob,
        _mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        Err(BuilderVmError::NotYetImplemented)
    }
}

// =====================================================================
// microsandbox-backed builder (feature `contributor-bootstrap`)
//
// Everything below until `extract_revision_hash` is the microsandbox
// integration. Library consumers of the `mvmctl` facade can disable the
// whole block via `default-features = false`; the upstream microsandbox
// crate pulls sqlx-sqlite which collides with rusqlite-based DBs (see
// DRIFT-001). The `cfg`-attributes below all reference the single
// `contributor-bootstrap` feature flag.
// =====================================================================

/// Default vCPU count for the builder sandbox. Tuned for "fast
/// enough to feel native on a developer laptop" without saturating
/// the host. Override via [`MicrosandboxBuilderVm::with_resources`].
#[cfg(feature = "contributor-bootstrap")]
const BUILDER_DEFAULT_CPUS: u8 = 4;

/// Default memory for the builder sandbox, in MiB. Nix derivations
/// for guest rootfs builds peak well under 4 GiB; 4096 MiB leaves
/// headroom for the dev tooling closure plus jobserver fan-out.
#[cfg(feature = "contributor-bootstrap")]
const BUILDER_DEFAULT_MEMORY_MIB: u32 = 4096;

/// Where in the sandbox the user's flake gets bind-mounted.
/// ADR-013 step 3.
pub const BUILDER_GUEST_WORK_DIR: &str = "/work";
/// Where in the sandbox the host's nix store gets bind-mounted
/// (when [`BuilderMounts::host_nix_store`] is `Some`).
pub const BUILDER_GUEST_NIX_DIR: &str = "/nix";
/// Where in the sandbox artifacts get extracted.
pub const BUILDER_GUEST_OUT_DIR: &str = "/out";

/// Real builder. Pulls [`BUILDER_OCI_IMAGE`] on demand (microsandbox
/// manages the local OCI cache), spawns a sandbox with the three
/// bind-mounts from ADR-013 step 3, runs `nix build` inside via
/// `sandbox.shell()`, and copies the resolved output store path to
/// `/out` before tearing the sandbox down.
///
/// ## Lifecycle
///
/// `run_build` is a one-shot: each call creates a fresh sandbox and
/// drops it on the way out. Two reasons:
///   1. Builds are independent; reusing a long-lived sandbox would
///      mean coordinating concurrent `mvmctl build` invocations,
///      which the caller doesn't promise.
///   2. The OCI image cache is shared across invocations (microsandbox
///      owns it), so the per-call cost is just the sandbox spawn
///      (~200 ms) — not a repeat pull.
///
/// ## What it doesn't do
///
/// - **Per-call image-digest verification.** [`BUILDER_OCI_DIGEST_SHA256`]
///   is still empty; once the pin lands, this impl checks it after
///   pull and fails closed on mismatch. Until then, the pinned
///   `:2.24.10` tag is the contract.
/// - **Snapshot warm-pool.** ADR-013 hints at a future warm-pool of
///   pre-loaded builder sandboxes for sub-second cold-start. Out of
///   scope here.
#[cfg(feature = "contributor-bootstrap")]
#[derive(Debug, Clone)]
pub struct MicrosandboxBuilderVm {
    cpus: u8,
    memory_mib: u32,
}

#[cfg(feature = "contributor-bootstrap")]
impl Default for MicrosandboxBuilderVm {
    fn default() -> Self {
        Self {
            cpus: BUILDER_DEFAULT_CPUS,
            memory_mib: BUILDER_DEFAULT_MEMORY_MIB,
        }
    }
}

#[cfg(feature = "contributor-bootstrap")]
impl MicrosandboxBuilderVm {
    /// Override the default vCPU / memory pair. Useful for CI runners
    /// or low-memory hosts that can't afford the 4 GiB default.
    pub fn with_resources(mut self, cpus: u8, memory_mib: u32) -> Self {
        self.cpus = cpus;
        self.memory_mib = memory_mib;
        self
    }
}

/// Bridge sync `BuilderVm` calls into microsandbox's async API.
/// Same pattern as `mvm_backend::microsandbox::block_on` — the
/// `tokio::Runtime` is built fresh per call so the trait stays
/// `Send + Sync` and dyn-friendly. Per-call cost (~200 µs) is
/// dominated by sandbox spawn (~200 ms) so the trade is fine.
#[cfg(feature = "contributor-bootstrap")]
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build single-threaded tokio runtime for builder VM bridge");
    rt.block_on(fut)
}

/// Unique-per-build sandbox name derived from a flake reference +
/// attr path + a timestamp. Doesn't need to be cryptographically
/// random — microsandbox uses it as a handle for `Sandbox::get` etc.,
/// and the sandbox is torn down at the end of `run_build` so name
/// collisions only matter for concurrent invocations.
#[cfg(feature = "contributor-bootstrap")]
fn sandbox_name(job: &BuilderJob) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    job.flake_ref.hash(&mut h);
    job.attr_path.hash(&mut h);
    format!("mvm-builder-{:x}-{}", h.finish(), stamp)
}

/// Quote a single shell argument for safe interpolation into a
/// bash `-c` script. Same shape as
/// `mvm-base::shell::shell_escape` (deleted in W7 with
/// the rest of the Lima paths) — kept inline here so the builder
/// crate doesn't take a dep on a runtime-side helper for one
/// function.
#[cfg(feature = "contributor-bootstrap")]
fn shell_quote_arg(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

/// Probe whether the host already has Nix available. True covers
/// both:
///   - Linux with host Nix (`nix` on PATH; `nix build` runs natively).
///   - macOS with nix-darwin's `linux-builder` configured (`nix` on
///     PATH; nix transparently routes Linux derivations to the
///     configured SSH-backed Linux VM).
///
/// Returns false on macOS Intel / pre-26 / no-KVM-Linux without
/// host Nix — the case the microsandbox builder serves.
#[cfg(feature = "contributor-bootstrap")]
fn host_nix_available() -> bool {
    which::which("nix").is_ok()
}

#[cfg(feature = "contributor-bootstrap")]
impl BuilderVm for MicrosandboxBuilderVm {
    fn host_can_build(&self) -> Result<bool, BuilderVmError> {
        Ok(host_nix_available())
    }

    fn run_build(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        // Validate caller-supplied paths early; microsandbox will
        // also reject these but with less useful messages.
        if !mounts.flake_src.exists() {
            return Err(BuilderVmError::ExtractionFailed(format!(
                "flake source path does not exist: {}",
                mounts.flake_src.display()
            )));
        }
        std::fs::create_dir_all(&mounts.artifact_out).map_err(|e| {
            BuilderVmError::ExtractionFailed(format!(
                "creating artifact output dir {}: {e}",
                mounts.artifact_out.display()
            ))
        })?;

        let params = RunBuildParams {
            name: sandbox_name(job),
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            image: BUILDER_OCI_IMAGE.to_string(),
            flake_src: mounts.flake_src.clone(),
            host_nix_store: mounts.host_nix_store.clone(),
            artifact_out: mounts.artifact_out.clone(),
            flake_ref: job.flake_ref.clone(),
            attr_path: job.attr_path.clone(),
        };

        block_on(async move { run_build_async(params).await })
    }
}

/// Parameter bundle for [`run_build_async`]. Carried as a struct so
/// the async helper stays clippy-clean against `too_many_arguments`
/// — CLAUDE.md forbids suppressing that lint, and pulling 9 owned
/// values across an `await` boundary one by one isn't readable
/// anyway.
#[cfg(feature = "contributor-bootstrap")]
struct RunBuildParams {
    name: String,
    cpus: u8,
    memory_mib: u32,
    image: String,
    flake_src: PathBuf,
    host_nix_store: Option<PathBuf>,
    artifact_out: PathBuf,
    flake_ref: String,
    attr_path: String,
}

/// Async body of [`MicrosandboxBuilderVm::run_build`]. Lifted out so
/// the sync trait method stays narrow and the async surface is
/// testable in isolation when integration coverage lands.
#[cfg(feature = "contributor-bootstrap")]
async fn run_build_async(params: RunBuildParams) -> Result<BuilderArtifacts, BuilderVmError> {
    let RunBuildParams {
        name,
        cpus,
        memory_mib,
        image,
        flake_src,
        host_nix_store,
        artifact_out,
        flake_ref,
        attr_path,
    } = params;

    // 1. Spawn the sandbox.
    //
    // Force anonymous auth for `BUILDER_OCI_IMAGE` (public Docker Hub
    // image). microsandbox's default `resolve_registry_auth` consults
    // the host's Docker credential helper (`~/.docker/config.json` +
    // osxkeychain on macOS) before falling through to anonymous, and a
    // stale Docker Hub PAT in the keychain poisons the token exchange:
    // `oci-client::_auth` returns `Err(AuthenticationFailure)`, which
    // `get_auth_token` then silently swallows via `.ok()??` — the
    // manifest request goes out unauthenticated, returns 401, and the
    // surfaced error is the misleading "Not authorized: url
    // …/manifests/2.24.10". A public builder image never needs creds,
    // so bypassing the helper makes that failure mode impossible.
    let mut builder = microsandbox::Sandbox::builder(&name)
        .image(image)
        .pull_policy(microsandbox::sandbox::PullPolicy::IfMissing)
        .registry(|r| r.auth(microsandbox::RegistryAuth::Anonymous))
        .cpus(cpus)
        .memory(memory_mib)
        .volume(BUILDER_GUEST_WORK_DIR, |m| {
            m.bind(flake_src.as_path()).readonly()
        })
        .volume(BUILDER_GUEST_OUT_DIR, |m| m.bind(artifact_out.as_path()));
    if let Some(store) = host_nix_store.as_ref() {
        builder = builder.volume(BUILDER_GUEST_NIX_DIR, |m| m.bind(store.as_path()));
    }

    let sandbox = builder
        .create_detached()
        .await
        .map_err(|e| BuilderVmError::MicrosandboxUnavailable(format!("create_detached: {e}")))?;

    // 2. Run `nix build` inside. Use `--no-link` so we don't
    //    accumulate result symlinks across builds; capture the
    //    output store path via `--print-out-paths` so the extraction
    //    step knows what to copy.
    //
    // `--no-write-lock-file --impure` is what unblocks builds inside
    // the sandbox when the flake has path inputs (`path:..`-style):
    //   - The bind-mounted `/work` is read-only, so any attempt to
    //     write the lock fails with EROFS.
    //   - Path inputs are "unlocked" by construction (no content hash
    //     to verify against), so strict pure-mode rejects them with
    //     "lock file contains unlocked input '{"path":"...","type":"path"}'".
    //   - The lockfile is also nuked from `/work/<flake>` on the host
    //     before this script runs (see the xtask), so there's nothing
    //     stale to re-validate.
    let build_script = format!(
        r#"set -euo pipefail
# Run `git config --global` BEFORE cd'ing into the workspace mount.
# `safe.directory = *` neutralises the cross-uid check git 2.35+
# enforces on bind-mounted repos — engaged by nix's git fetcher when
# the flake path lives under a `.git` dir (`/work` does). `--global`
# writes to /root inside the sandbox (fresh per spawn; never leaks
# to the host), but git's *startup* still walks cwd upward looking
# for a repo; in a git-worktree workspace the discovered `.git` is
# a *file* whose `gitdir:` redirect points to a host path that
# doesn't exist in the sandbox, so the discovery itself errors
# "not a git repository" with exit 128 before `--global` is
# processed. Running from `/` sidesteps the cwd scan entirely.
git config --global --add safe.directory '*'
cd {work}
export NIX_CONFIG="experimental-features = nix-command flakes"
# Tell the flake where the workspace lives in this sandbox. Without
# this, the flake's relative path import resolves against the
# store-copied flake dir (because `path:` URL semantics) and ends
# up at `/`, tripping on sandbox-internal files like
# `/.msb/agent.sock`. The flake reads this under `--impure`.
export MVM_WORKSPACE_PATH={work}
nix build {flake_ref}#{attr_path} --no-link --print-out-paths --no-write-lock-file --impure
"#,
        work = shell_quote_arg(BUILDER_GUEST_WORK_DIR),
        flake_ref = flake_ref,
        attr_path = attr_path,
    );

    let build_out = sandbox
        .shell(build_script)
        .await
        .map_err(|e| BuilderVmError::NixBuildFailed(format!("sandbox.shell(nix build): {e}")))?;
    if !build_out.status().success {
        let stderr = build_out
            .stderr()
            .unwrap_or_else(|_| "<non-utf8 stderr>".to_string());
        // Tear down before returning — best-effort.
        let _ = sandbox.stop().await;
        return Err(BuilderVmError::NixBuildFailed(format!(
            "exit {} — stderr:\n{}",
            build_out.status().code,
            stderr
        )));
    }

    let nix_output_path = build_out
        .stdout()
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("stdout was non-UTF-8: {e}")))?
        .lines()
        .rev()
        .find(|l| l.starts_with("/nix/store/"))
        .ok_or_else(|| {
            BuilderVmError::ExtractionFailed(
                "nix build inside sandbox produced no /nix/store output path".into(),
            )
        })?
        .trim()
        .to_string();
    let revision_hash = extract_revision_hash(&nix_output_path);

    // 3. Copy artifacts from the in-sandbox store path to /out.
    //    Mirrors `copy_dev_artifacts` in `pipeline::dev_build` so the
    //    on-host layout matches what the runtime path expects.
    let copy_script = format!(
        r#"set -euo pipefail
out={out}
src={src}
cp -L "$src/vmlinux" "$out/vmlinux" 2>/dev/null || true
cp -L "$src/rootfs.ext4" "$out/rootfs.ext4"
[ -f "$src/initrd" ] && cp -L "$src/initrd" "$out/initrd"
[ -f "$src/initrd.cpio.gz" ] && cp -L "$src/initrd.cpio.gz" "$out/initrd.cpio.gz"
[ -f "$src/mvm-meta.json" ] && cp -L "$src/mvm-meta.json" "$out/mvm-meta.json"
# Plan 72 W2 outputs: builder-vm flake emits cmdline.txt +
# manifest.json alongside vmlinux + rootfs.ext4. dev-shell flakes
# don't, so the test is a no-op there.
[ -f "$src/cmdline.txt" ] && cp -L "$src/cmdline.txt" "$out/cmdline.txt"
[ -f "$src/manifest.json" ] && cp -L "$src/manifest.json" "$out/manifest.json"
chmod -R u+w "$out"
"#,
        out = shell_quote_arg(BUILDER_GUEST_OUT_DIR),
        src = shell_quote_arg(&nix_output_path),
    );

    let copy_out = sandbox
        .shell(copy_script)
        .await
        .map_err(|e| BuilderVmError::ExtractionFailed(format!("sandbox.shell(cp): {e}")))?;
    if !copy_out.status().success {
        let stderr = copy_out
            .stderr()
            .unwrap_or_else(|_| "<non-utf8 stderr>".to_string());
        let _ = sandbox.stop().await;
        return Err(BuilderVmError::ExtractionFailed(format!(
            "artifact copy failed (exit {}): {stderr}",
            copy_out.status().code,
        )));
    }

    // 4. Tear the sandbox down. Best-effort: a failure here doesn't
    //    invalidate the artifacts that already landed in `out`. The
    //    handle drops at function end either way, but explicit stop
    //    frees the libkrun process slot now rather than at GC time.
    if let Err(e) = sandbox.stop().await {
        tracing::warn!(error = %e, "builder sandbox stop failed (artifacts intact)");
    }

    // 5. Resolve accessible flag from the sidecar that mkGuest
    //    emits inside the store path. Mirrors what
    //    `emit_sidecar_via_passthru_query` does on the host path —
    //    the sidecar is already on the host filesystem thanks to
    //    the bind-mount, so no separate query is needed.
    let sidecar_path = ArtifactSidecar::path_in(&artifact_out);
    let accessible = if sidecar_path.exists() {
        ArtifactSidecar::read_from_dir(&artifact_out)
            .ok()
            .flatten()
            .map(|s| s.accessible)
    } else {
        None
    };

    let rootfs_path = artifact_out.join("rootfs.ext4");
    let kernel_path = {
        let p = artifact_out.join("vmlinux");
        if p.exists() { Some(p) } else { None }
    };

    Ok(BuilderArtifacts {
        rootfs_path,
        kernel_path,
        revision_hash,
        lock_hash: None,
        accessible,
    })
}

/// Extract the 32-character store-path hash from a path like
/// `/nix/store/<hash>-<name>`. Returns the empty string if the path
/// shape is unexpected. Mirrors
/// `pipeline::dev_build::extract_revision_hash`'s behavior so the
/// two code paths produce the same artifact-dir name for identical
/// derivations.
///
/// Only `run_build_async` (gated on `contributor-bootstrap`) and the
/// unit tests exercise this helper; the `#[allow(dead_code)]` keeps
/// no-default-features library builds warning-clean without forcing
/// a wider cfg expression.
#[allow(dead_code)]
fn extract_revision_hash(nix_output_path: &str) -> String {
    nix_output_path
        .trim_start_matches("/nix/store/")
        .split('-')
        .next()
        .unwrap_or("")
        .to_string()
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
    fn stub_returns_not_yet_implemented_for_host_can_build() {
        let stub = StubBuilderVm;
        let err = stub.host_can_build().expect_err("stub returns err");
        assert!(
            matches!(err, BuilderVmError::NotYetImplemented),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("ADR-013"),
            "error should point at ADR: {err}"
        );
    }

    #[test]
    fn stub_returns_not_yet_implemented_for_run_build() {
        let stub = StubBuilderVm;
        let job = BuilderJob {
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
    fn cleanup_default_is_ok() {
        // Stateless implementations get a free no-op cleanup.
        assert!(StubBuilderVm.cleanup().is_ok());
    }

    #[test]
    fn error_message_points_at_recovery_path() {
        let err = BuilderVmError::NotYetImplemented;
        let msg = err.to_string();
        assert!(msg.contains("install host Nix") || msg.contains("nix-darwin"));
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
            hypervisor: "microsandbox".to_string(),
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
    fn extract_revision_hash_pulls_leading_segment() {
        let h = extract_revision_hash("/nix/store/abc123def456-mvm-rootfs-1.0.0");
        assert_eq!(h, "abc123def456");
    }

    #[test]
    fn extract_revision_hash_handles_malformed() {
        assert_eq!(extract_revision_hash(""), "");
        assert_eq!(extract_revision_hash("not-a-store-path"), "not");
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn sandbox_name_has_stable_prefix() {
        // Same flake+attr produces the same hash segment; only the
        // timestamp varies. Lets us assert the prefix without
        // hard-coding the full name.
        let job = BuilderJob {
            flake_ref: "git+file:///work".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let name = sandbox_name(&job);
        assert!(name.starts_with("mvm-builder-"), "got {name}");
        assert!(
            name.len() > "mvm-builder-".len() + 4,
            "name must carry a discriminator: {name}"
        );
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn microsandbox_builder_has_sensible_defaults() {
        let b = MicrosandboxBuilderVm::default();
        assert_eq!(b.cpus, BUILDER_DEFAULT_CPUS);
        assert_eq!(b.memory_mib, BUILDER_DEFAULT_MEMORY_MIB);
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn microsandbox_builder_with_resources_overrides() {
        let b = MicrosandboxBuilderVm::default().with_resources(2, 2048);
        assert_eq!(b.cpus, 2);
        assert_eq!(b.memory_mib, 2048);
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn shell_quote_arg_escapes_single_quotes() {
        assert_eq!(shell_quote_arg("simple"), "'simple'");
        assert_eq!(shell_quote_arg("with space"), "'with space'");
        // Single quote inside: close, escaped quote, reopen.
        assert_eq!(shell_quote_arg("it's"), "'it'\\''s'");
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn run_build_validates_missing_flake_src() {
        // Skip the path that actually spawns microsandbox — just
        // exercise the input validation. Caller supplied a
        // nonexistent flake src; we expect a clear error before
        // anything heavy fires.
        let b = MicrosandboxBuilderVm::default();
        let job = BuilderJob {
            flake_ref: ".".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let mounts = BuilderMounts {
            flake_src: PathBuf::from("/definitely/does/not/exist"),
            host_nix_store: None,
            artifact_out: PathBuf::from("/tmp/mvm-builder-test-out"),
        };
        let err = b
            .run_build(&job, &mounts)
            .expect_err("nonexistent flake should err");
        assert!(
            matches!(err, BuilderVmError::ExtractionFailed(_)),
            "got {err:?}"
        );
        assert!(err.to_string().contains("does not exist"), "msg: {err}");
    }

    #[cfg(feature = "contributor-bootstrap")]
    #[test]
    fn host_can_build_is_a_pure_pathfn() {
        // Result depends on whether the test runner has `nix` on
        // PATH — both outcomes are valid. The test asserts the
        // function returns Ok rather than erroring, since the impl
        // shouldn't ever return Err here (only the absence vs.
        // presence of nix matters).
        let b = MicrosandboxBuilderVm::default();
        assert!(b.host_can_build().is_ok());
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
