//! libkrun-backed `BuilderVm` implementation (plan 72 W1 / ADR-046).
//!
//! Replaces `MicrosandboxBuilderVm` on the user-facing Layer-2 build
//! path: instead of starting an OCI-image-backed microsandbox VM (whose
//! 4 GiB writable overlay overflows on the dev image's `nix build`
//! closure), `LibkrunBuilderVm` boots the purpose-built builder VM
//! image from `nix/images/builder-vm/` (plan 72 W2), running `mvm-builder-init`
//! as PID 1 (plan 72 W3). The host attaches three virtio-fs shares
//! (workspace at `/work`, artifact dir at `/out`, job dir at `/job`),
//! a virtio-blk holding the persistent `/nix` store at `/dev/vdb`, and
//! a virtio-net link. The init runs `/job/cmd.sh`, writes the exit
//! code to `/job/result`, and powers off; this code copies the
//! artifacts back from `/out` to the host's `mounts.artifact_out`.
//!
//! ## Status (plan 72 W1)
//!
//! **Scaffolding behind `backends-builder-vm-libkrun`.** The trait
//! contract is wired and the host-side staging (job dir, persistent
//! `/nix` image allocation, path validation, artifact resolution after
//! boot) is implemented. The libkrun boot itself returns
//! [`BuilderVmError::LibkrunUnavailable`] until plan 57 W3 lands its
//! `start_enter` + `shutdown_eventfd` thread-and-poll lifecycle. The
//! image-resolution path returns [`BuilderVmError::BuilderImageMissing`]
//! until plan 72 W5 wires `find_builder_vm_flake()` + `download_builder_vm_image`.
//!
//! ## Why split from `mvm-libkrun`
//!
//! `mvm-libkrun` carries only the libkrun FFI surface — kernel,
//! rootfs, vsock, virtio-fs, virtio-blk wiring. The build-pipeline
//! concerns (resolving the builder VM image, staging cmd.sh, copying
//! artifacts back, mapping exit codes to `BuilderVmError`) live here
//! so the FFI crate stays lean and the build crate keeps the
//! workflow-aware logic.

use std::fs;
use std::path::{Path, PathBuf};

use crate::builder_vm::{
    BuilderArtifacts, BuilderJob, BuilderMounts, BuilderVm, BuilderVmError,
    BUILDER_GUEST_OUT_DIR, BUILDER_GUEST_WORK_DIR,
};

/// Default vCPU count. Plan 72 W1 §Interface — `nix build` is CPU
/// heavy; 4 is the sweet spot for laptop-class hosts.
pub const DEFAULT_VCPUS: u8 = 4;
/// Default memory in MiB. Plan 72 W1 §Interface — covers nixpkgs
/// stdenv builds with headroom for the substituter fetcher's TLS
/// closure.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
/// Default size of the persistent `/nix` virtio-blk image, in MiB.
/// Sparse-allocated (only used blocks consume host disk). 64 GiB
/// accommodates the worst-case rustc + nixpkgs stdenv closure with
/// substantial cached output room.
pub const DEFAULT_NIX_STORE_MIB: u32 = 65_536;
/// In-guest mount point for the job directory (cmd.sh, env, result).
pub const BUILDER_GUEST_JOB_DIR: &str = "/job";
/// In-guest virtio-blk device for the persistent Nix store. Must
/// match `mvm-builder-init`'s `NIX_STORE_DEV` constant.
pub const NIX_STORE_GUEST_DEV: &str = "/dev/vdb";

/// virtio-fs tag the guest mounts to see the workspace. Must match
/// `mvm-builder-init`'s `VIRTIOFS_SHARES`.
pub const VIRTIOFS_TAG_WORK: &str = "mvm-work";
/// virtio-fs tag for the host's artifact output dir.
pub const VIRTIOFS_TAG_OUT: &str = "mvm-out";
/// virtio-fs tag for the host's job dir (cmd.sh + result).
pub const VIRTIOFS_TAG_JOB: &str = "mvm-job";

/// Max wall-clock for a single build. ADR-046 mentions a 30-minute
/// cap; surface it here so plan 57 W3's poll loop has a published
/// upper bound. Cold-cache builds for the dev image typically run
/// in 4-8 minutes; this is "something is wrong" territory.
pub const BUILD_TIMEOUT_SECS: u64 = 30 * 60;

/// libkrun-backed builder. See module docs for status.
#[derive(Debug, Clone)]
pub struct LibkrunBuilderVm {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub nix_store_mib: u32,
    /// Plan 72 W4 air-gapped variant. When `true`, the cmd.sh sets
    /// `NIX_CONFIG="substituters ="` so `nix build` won't hit the
    /// network — the builder VM is expected to satisfy the closure
    /// from the seed `/nix` baked into the rootfs image. The init
    /// also skips DHCP (best-effort; init runs `udhcpc` unconditionally
    /// today, but a missing route turns the substituter calls into
    /// no-ops). Tracked in plan 72 W4 §Air-gapped mode.
    pub offline: bool,
    /// Pre-resolved builder VM image. Injected by mvm-cli's
    /// `ensure_builder_vm_image` (plan 72 W5) so this crate doesn't
    /// take a dep on the CLI. When `None`, `run_build` returns
    /// `BuilderImageMissing` with a hint telling the caller to
    /// inject it.
    pub image: Option<BuilderVmImage>,
}

impl Default for LibkrunBuilderVm {
    fn default() -> Self {
        Self {
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            nix_store_mib: DEFAULT_NIX_STORE_MIB,
            offline: false,
            image: None,
        }
    }
}

impl LibkrunBuilderVm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the default CPU + memory pair.
    pub fn with_resources(mut self, vcpus: u8, memory_mib: u32) -> Self {
        self.vcpus = vcpus;
        self.memory_mib = memory_mib;
        self
    }

    /// Override the persistent `/nix` store image size in MiB. The
    /// image is sparse-allocated, so a larger value only costs more
    /// when actually used; leaving it at the default is almost always
    /// the right call.
    pub fn with_nix_store_size(mut self, mib: u32) -> Self {
        self.nix_store_mib = mib;
        self
    }

    /// Plan 72 W4 §Air-gapped mode. Build everything from the seed
    /// `/nix` baked into the rootfs — no substituter calls, no
    /// network dependence. Useful for CI lanes that need to prove
    /// the seed closure is complete and for offline contributor
    /// workflows.
    pub fn with_offline(mut self) -> Self {
        self.offline = true;
        self
    }

    /// Inject a pre-resolved [`BuilderVmImage`]. mvm-cli's
    /// `ensure_builder_vm_image` builds one via the source-checkout
    /// cache lookup (plan 72 W5) or release-download path
    /// (plan 72 W5 follow-on) and hands it here. The DI shape keeps
    /// mvm-build from needing to know about mvm-cli's cache layout.
    pub fn with_image(mut self, image: BuilderVmImage) -> Self {
        self.image = Some(image);
        self
    }

    /// Variant of [`BuilderVm::run_build`] that threads a
    /// [`ConsoleCapture`] through the boot lifecycle. Plan 57 W3's
    /// `start_enter` + `shutdown_eventfd` poll will feed each
    /// console line into `capture.line(...)`; for now the seam is
    /// in place but no lines are emitted because the libkrun start
    /// call returns before booting.
    ///
    /// `BuilderVm::run_build` calls this with a
    /// [`PrintlnConsoleCapture`] so callers that don't care about
    /// live progress get the default behaviour (stdout passthrough).
    pub fn run_build_with_capture(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
        capture: &mut dyn ConsoleCapture,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        self.run_build_inner(job, mounts, capture)
    }

    fn run_build_inner(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
        capture: &mut dyn ConsoleCapture,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        validate_mounts(mounts)?;

        let image = match self.image.as_ref() {
            Some(img) => img.clone(),
            None => ensure_builder_vm_image()?,
        };

        let cache = cache_dir()?;
        let job_id = job_id_for(job);
        let job_dir = stage_job_dir(&cache, &job_id, job, self.offline)?;
        let nix_store_img = ensure_nix_store_image(&cache, self.nix_store_mib)?;

        let plan = LaunchPlan {
            image,
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            nix_store_img,
            job_dir,
            mounts: mounts.clone(),
            offline: self.offline,
        };

        launch_and_wait(&plan, capture)?;

        collect_artifacts(mounts, &plan.job_dir)
    }
}

// ──────────────────────── console capture ──────────────────────────

/// Origin of a console line emitted by the builder VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleSource {
    /// Primary kernel console (boot messages, `/init` output, the
    /// builder VM's cmd.sh stdout+stderr merged).
    Boot,
    /// Job-script stderr fanned out over vsock — populated by plan
    /// 57 W3's vsock console plumbing when it lands. Today the
    /// boot console carries everything and this variant is unused.
    JobStderr,
}

/// Sink for console lines emitted while the builder VM boots and
/// runs the job. Pluggable so the CLI can colorise/forward to a UI
/// progress bar, tests can record into a buffer, and headless
/// production paths can no-op.
///
/// `Send` so plan 57 W3's threaded poll can hand the capture across
/// the boot-thread boundary; no `Sync` requirement because each call
/// is owned by the single drainer thread.
pub trait ConsoleCapture: Send {
    /// Called once per line. Implementations format as they see fit;
    /// no newline is included in `line`.
    fn line(&mut self, source: ConsoleSource, line: &str);
}

/// Default capture: prints every line to stdout, prefixed with the
/// source tag. Plan 57 W3 will swap in a streaming variant in the
/// CLI; this lives here so library consumers + tests have a
/// concrete impl to reach for.
#[derive(Debug, Default)]
pub struct PrintlnConsoleCapture;

impl ConsoleCapture for PrintlnConsoleCapture {
    fn line(&mut self, source: ConsoleSource, line: &str) {
        let tag = match source {
            ConsoleSource::Boot => "builder",
            ConsoleSource::JobStderr => "job",
        };
        println!("[{tag}] {line}");
    }
}

/// In-memory capture used by tests + ad-hoc diagnostics. Stores
/// every line with its source tag so the test can assert ordering
/// and content.
#[derive(Debug, Default)]
pub struct RecordingConsoleCapture {
    pub lines: Vec<(ConsoleSource, String)>,
}

impl ConsoleCapture for RecordingConsoleCapture {
    fn line(&mut self, source: ConsoleSource, line: &str) {
        self.lines.push((source, line.to_string()));
    }
}

impl BuilderVm for LibkrunBuilderVm {
    fn host_can_build(&self) -> Result<bool, BuilderVmError> {
        // CLAUDE.md §"Host Nix is never used by mvmctl" — the libkrun
        // path never delegates to host Nix, even when installed. This
        // is a deliberate divergence from `MicrosandboxBuilderVm::host_can_build`
        // (which checks `which::which("nix")`) and the reason is
        // determinism: the same mvmctl binary should produce the same
        // artifacts on every host regardless of host tooling.
        Ok(false)
    }

    fn run_build(
        &self,
        job: &BuilderJob,
        mounts: &BuilderMounts,
    ) -> Result<BuilderArtifacts, BuilderVmError> {
        // Trait callers get the println capture by default. Callers
        // wanting custom routing use `run_build_with_capture`.
        let mut capture = PrintlnConsoleCapture;
        self.run_build_inner(job, mounts, &mut capture)
    }
}

// ─────────────────────── public helpers (testable) ──────────────────

/// Root cache directory for builder VM artifacts: `${mvm cache}/builder-vm`.
/// Delegates to `mvm_core::config::mvm_cache_dir()` which honors
/// `MVM_CACHE_DIR` → `XDG_CACHE_HOME/mvm` → `$HOME/.cache/mvm`, so
/// the libkrun builder shares the same precedence as every other
/// mvm cache consumer (dev images, default microVM images, etc.).
pub fn cache_dir() -> Result<PathBuf, BuilderVmError> {
    Ok(PathBuf::from(mvm_core::config::mvm_cache_dir()).join("builder-vm"))
}

/// Path to the persistent `/nix` store virtio-blk image for the
/// running architecture. One image per arch since the closure shapes
/// differ; sparse-allocated up to `nix_store_mib` on first creation.
pub fn nix_store_image_path(cache: &Path) -> PathBuf {
    cache.join(format!("nix-store-{}.img", host_arch_tag()))
}

/// Architecture tag used in cache filenames. Matches the release
/// artifact arch suffix (`aarch64`, `x86_64`) so a contributor moving
/// between machines on the same arch reuses the cache.
pub fn host_arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => other,
    }
}

/// Validate the caller-supplied mounts. Mirrors microsandbox's
/// `flake_src.exists()` + `create_dir_all(artifact_out)` checks;
/// additionally rejects non-UTF-8 paths because libkrun's FFI
/// requires `CString` and a non-UTF-8 path can't survive that.
pub fn validate_mounts(mounts: &BuilderMounts) -> Result<(), BuilderVmError> {
    if !mounts.flake_src.exists() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "flake source path does not exist: {}",
            mounts.flake_src.display()
        )));
    }
    if path_has_nul_or_non_utf8(&mounts.flake_src) {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "flake source path is not UTF-8 / contains NUL: {}",
            mounts.flake_src.display()
        )));
    }
    if path_has_nul_or_non_utf8(&mounts.artifact_out) {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "artifact output path is not UTF-8 / contains NUL: {}",
            mounts.artifact_out.display()
        )));
    }
    fs::create_dir_all(&mounts.artifact_out).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating artifact output dir {}: {e}",
            mounts.artifact_out.display()
        ))
    })?;
    if let Some(store) = mounts.host_nix_store.as_deref()
        && path_has_nul_or_non_utf8(store)
    {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "host_nix_store path is not UTF-8 / contains NUL: {}",
            store.display()
        )));
    }
    Ok(())
}

/// Deterministic job ID for a (flake_ref, attr_path) pair. Pure hash
/// — no timestamp — so re-running the same build reuses the same
/// job dir, which makes incremental diffing easier in CI logs. If
/// callers need uniqueness across concurrent runs, they pass distinct
/// `BuilderJob` values; we don't paper over that here.
pub fn job_id_for(job: &BuilderJob) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    job.flake_ref.hash(&mut h);
    job.attr_path.hash(&mut h);
    format!("{:x}", h.finish())
}

// ──────────────────── private staging + launch ──────────────────────

struct LaunchPlan {
    image: BuilderVmImage,
    vcpus: u8,
    memory_mib: u32,
    nix_store_img: PathBuf,
    job_dir: PathBuf,
    mounts: BuilderMounts,
    offline: bool,
}

/// Resolved builder VM image — paths to the kernel, rootfs, and
/// kernel cmdline string. The image-acquisition logic itself lives
/// in `mvm-cli` (plan 72 W5); this type is `pub` so the CLI can
/// construct values and inject them via `LibkrunBuilderVm::with_image`.
#[derive(Debug, Clone)]
pub struct BuilderVmImage {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub cmdline: String,
}

impl BuilderVmImage {
    /// File name conventions matching `nix/images/builder-vm/flake.nix`'s
    /// emitted artifacts. Kept here as constants so the CLI's cache
    /// layout and this module's loader can't drift.
    pub const KERNEL_FILENAME: &'static str = "vmlinux";
    pub const ROOTFS_FILENAME: &'static str = "rootfs.ext4";
    pub const CMDLINE_FILENAME: &'static str = "cmdline";

    /// Build a `BuilderVmImage` from a directory containing the three
    /// expected files. The CLI's cache layout writes them under
    /// `~/.cache/mvm/builder-vm/<key>/{vmlinux,rootfs.ext4,cmdline}`
    /// so this is the canonical inverse of "stash a freshly-built
    /// image into the cache."
    ///
    /// Errors with `BuilderImageMissing` when any file is absent —
    /// the message names the missing entry so the caller can hint at
    /// "did the build complete?" vs "wrong cache dir?".
    pub fn load_from_dir(dir: &Path) -> Result<Self, BuilderVmError> {
        let kernel = dir.join(Self::KERNEL_FILENAME);
        let rootfs = dir.join(Self::ROOTFS_FILENAME);
        let cmdline_path = dir.join(Self::CMDLINE_FILENAME);
        for (name, path) in [
            (Self::KERNEL_FILENAME, &kernel),
            (Self::ROOTFS_FILENAME, &rootfs),
            (Self::CMDLINE_FILENAME, &cmdline_path),
        ] {
            if !path.exists() {
                return Err(BuilderVmError::BuilderImageMissing(format!(
                    "missing {name} in builder VM image dir {}",
                    dir.display()
                )));
            }
        }
        let cmdline = fs::read_to_string(&cmdline_path)
            .map_err(|e| {
                BuilderVmError::BuilderImageMissing(format!(
                    "reading {}: {e}",
                    cmdline_path.display()
                ))
            })?
            .trim()
            .to_string();
        if cmdline.is_empty() {
            return Err(BuilderVmError::BuilderImageMissing(format!(
                "cmdline file is empty: {}",
                cmdline_path.display()
            )));
        }
        Ok(Self {
            kernel,
            rootfs,
            cmdline,
        })
    }
}

fn ensure_builder_vm_image() -> Result<BuilderVmImage, BuilderVmError> {
    // Plan 72 W5 implementation lives in `mvm-cli` (`builder_vm_image`
    // module). Callers in mvm-cli inject a pre-resolved image via
    // `LibkrunBuilderVm::with_image` so this fallback never fires in
    // production. Library consumers that construct a bare
    // `LibkrunBuilderVm` and call `run_build` hit this branch and get
    // an actionable hint.
    Err(BuilderVmError::BuilderImageMissing(format!(
        "Layer-1 builder VM image not injected — call \
         `LibkrunBuilderVm::with_image()` before `run_build`, or use the \
         resolver in `mvm-cli` (plan 72 W5). For ad-hoc testing, build \
         the image with `nix build path:./nix/images/builder-vm#packages.{}-linux.default` \
         and load it via `BuilderVmImage::load_from_dir`.",
        host_arch_tag()
    )))
}

fn ensure_nix_store_image(cache: &Path, size_mib: u32) -> Result<PathBuf, BuilderVmError> {
    fs::create_dir_all(cache).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating builder-vm cache dir {}: {e}",
            cache.display()
        ))
    })?;
    let path = nix_store_image_path(cache);
    if path.exists() {
        return Ok(path);
    }
    // Sparse-allocate: open + seek + write 1 byte at SIZE-1 produces
    // a sparse hole on every filesystem that supports them (ext4,
    // APFS, ZFS, btrfs). Filesystems without sparse support get a
    // dense file; rare enough that we don't special-case.
    let size_bytes = u64::from(size_mib) * 1024 * 1024;
    let f = fs::File::create(&path).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "create /nix store image {}: {e}",
            path.display()
        ))
    })?;
    f.set_len(size_bytes).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "truncate /nix store image to {size_bytes} bytes: {e}"
        ))
    })?;
    // mvm-builder-init formats this on first boot via mkfs.ext4 —
    // host-side formatting would need a host-installed mke2fs that
    // CLAUDE.md says we don't depend on. Leaving the file blank is
    // intentional.
    Ok(path)
}

pub fn stage_job_dir(
    cache: &Path,
    job_id: &str,
    job: &BuilderJob,
    offline: bool,
) -> Result<PathBuf, BuilderVmError> {
    let jobs_root = cache.join("jobs");
    fs::create_dir_all(&jobs_root).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating jobs dir {}: {e}",
            jobs_root.display()
        ))
    })?;
    let job_dir = jobs_root.join(job_id);
    fs::create_dir_all(&job_dir).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "creating job dir {}: {e}",
            job_dir.display()
        ))
    })?;

    fs::write(job_dir.join("cmd.sh"), build_cmd_script(job, offline)).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("writing cmd.sh: {e}"))
    })?;
    // Empty result file — mvm-builder-init overwrites with the exit
    // code on poweroff. Creating it host-side makes "did the build
    // run at all?" easy to detect: a zero-length result means the
    // init never finished writing.
    fs::write(job_dir.join("result"), "").map_err(|e| {
        BuilderVmError::ExtractionFailed(format!("writing empty result file: {e}"))
    })?;

    Ok(job_dir)
}

/// Generate the in-guest build script. Mirrors `MicrosandboxBuilderVm`'s
/// `build_script` shape so the two backends produce identical artifacts
/// for the same job — only the launcher differs.
///
/// When `offline` is true, sets `NIX_CONFIG` to disable substituters so
/// `nix build` resolves the closure entirely from the seed `/nix`
/// baked into the rootfs (plan 72 W4 §Air-gapped mode).
pub fn build_cmd_script(job: &BuilderJob, offline: bool) -> String {
    let flake_ref = shell_quote_arg(&job.flake_ref);
    let attr_path = shell_quote_arg(&job.attr_path);
    let work = shell_quote_arg(BUILDER_GUEST_WORK_DIR);
    let out = shell_quote_arg(BUILDER_GUEST_OUT_DIR);
    // Two-line NIX_CONFIG when offline: first the experimental-features
    // toggle nix needs to recognise the flake CLI, then `substituters =`
    // (empty list) which prevents nix from reaching for cache.nixos.org.
    // Online mode keeps the single experimental-features line; nix uses
    // its default substituter set.
    let nix_config_line = if offline {
        // Use a HEREDOC-style multiline value; nix splits NIX_CONFIG on
        // newlines. `substituters =` (empty RHS) is the canonical way
        // to disable all binary cache fetchers in nix.conf.
        r#"export NIX_CONFIG='experimental-features = nix-command flakes
substituters =
'"#
        .to_string()
    } else {
        "export NIX_CONFIG=\"experimental-features = nix-command flakes\"".to_string()
    };
    // Note: same `safe.directory` + `MVM_WORKSPACE_PATH` dance as the
    // microsandbox path (see `MicrosandboxBuilderVm::run_build_async`).
    // The libkrun path inherits the same constraints — virtio-fs share
    // metadata can trip git discovery the same way an msb agent sock
    // could.
    format!(
        r#"set -euo pipefail
git config --global --add safe.directory '*'
cd {work}
{nix_config_line}
export MVM_WORKSPACE_PATH={work}
out_path=$(nix build {flake_ref}#{attr_path} \
  --no-link --print-out-paths --no-write-lock-file --impure \
  | tail -n 1)
test -n "$out_path" || {{ echo "nix build produced no output path" >&2; exit 1; }}
cp -L "$out_path/vmlinux"     {out}/vmlinux     2>/dev/null || true
cp -L "$out_path/rootfs.ext4" {out}/rootfs.ext4
[ -f "$out_path/initrd" ]          && cp -L "$out_path/initrd"          {out}/initrd
[ -f "$out_path/initrd.cpio.gz" ]  && cp -L "$out_path/initrd.cpio.gz"  {out}/initrd.cpio.gz
[ -f "$out_path/mvm-meta.json" ]   && cp -L "$out_path/mvm-meta.json"   {out}/mvm-meta.json
chmod -R u+w {out}
echo "$out_path" > {out}/.nix-out-path
"#
    )
}

/// Single-quote a shell argument the same way microsandbox's builder
/// does. Same helper duplicated rather than shared because the two
/// backends are deliberately independent — when microsandbox is
/// deleted in W5+, this stays.
fn shell_quote_arg(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn launch_and_wait(
    plan: &LaunchPlan,
    capture: &mut dyn ConsoleCapture,
) -> Result<(), BuilderVmError> {
    #[cfg(feature = "backends-builder-vm-libkrun")]
    {
        use mvm_libkrun::KrunContext;

        if !mvm_libkrun::is_available() {
            return Err(BuilderVmError::LibkrunUnavailable(format!(
                "libkrun shared library not found on this host. {}",
                mvm_libkrun::install_hint()
            )));
        }

        let kernel_path = string_or_err(&plan.image.kernel, "kernel")?;
        let rootfs_path = string_or_err(&plan.image.rootfs, "rootfs")?;
        let mut ctx = KrunContext::new("mvm-builder-vm", kernel_path, rootfs_path)
            .with_resources(plan.vcpus, plan.memory_mib);
        ctx.kernel_cmdline = Some(plan.image.cmdline.clone());

        // The actual virtio-fs / virtio-blk / vsock wiring + start +
        // shutdown-eventfd poll lives in plan 57 W3. `mvm_libkrun::start`
        // returns `NotYetWired` until that lands; we surface that as
        // a typed `BuilderVmError` so callers get a consistent error
        // shape regardless of which dependency is missing.
        //
        // The virtual-disk + virtio-fs + vsock plumbing (host_path bindings
        // for the three shares + the /dev/vdb image) is captured in plan
        // 72 W4 — that wave reshapes this match to call the actual
        // `start_with_config(...)` signature exposed by plan 57 W3.
        // For now, the `capture` parameter is plumbed through but no
        // lines fire because the boot doesn't complete.
        let _ = (&plan.nix_store_img, &plan.job_dir, &plan.mounts, plan.offline);
        // Surface the launch intent on the capture so callers verify
        // wiring works even before booting. Plan 57 W3 replaces this
        // with real console-line callbacks.
        capture.line(
            ConsoleSource::Boot,
            "mvm-builder-vm: launching (plan 57 W3 pending)",
        );
        mvm_libkrun::start(&ctx).map_err(|e| match e {
            mvm_libkrun::Error::NotYetWired { tracking } => BuilderVmError::LibkrunUnavailable(
                format!("libkrun boot wiring not landed yet (tracking: {tracking})"),
            ),
            mvm_libkrun::Error::NotInstalled { install_hint } => {
                BuilderVmError::LibkrunUnavailable(format!(
                    "libkrun not installed on this host. {install_hint}"
                ))
            }
            mvm_libkrun::Error::Krun(rc) => {
                BuilderVmError::LibkrunUnavailable(format!("libkrun call failed: rc {rc}"))
            }
            mvm_libkrun::Error::InvalidCString => BuilderVmError::LibkrunUnavailable(
                "libkrun rejected a path argument (NUL byte or non-UTF-8)".into(),
            ),
        })?;

        // Plan 57 W3 turns this into "block until shutdown eventfd
        // fires or BUILD_TIMEOUT_SECS elapses"; today the call above
        // returns immediately without booting and the result file is
        // empty.
        Ok(())
    }

    #[cfg(not(feature = "backends-builder-vm-libkrun"))]
    {
        let _ = (plan, capture);
        Err(BuilderVmError::LibkrunUnavailable(
            "compiled without `backends-builder-vm-libkrun` feature".into(),
        ))
    }
}

fn collect_artifacts(
    mounts: &BuilderMounts,
    job_dir: &Path,
) -> Result<BuilderArtifacts, BuilderVmError> {
    let result_path = job_dir.join("result");
    let result_body = fs::read_to_string(&result_path).map_err(|e| {
        BuilderVmError::ExtractionFailed(format!(
            "reading {}: {e}",
            result_path.display()
        ))
    })?;
    let trimmed = result_body.trim();
    if trimmed.is_empty() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "job result file {} is empty — builder VM didn't finish",
            result_path.display()
        )));
    }
    let exit_code: i32 = trimmed
        .parse()
        .map_err(|e| BuilderVmError::ExtractionFailed(format!(
            "job result {trimmed:?} not an integer: {e}"
        )))?;
    if exit_code != 0 {
        return Err(BuilderVmError::NixBuildFailed(format!(
            "build script exited {exit_code} inside builder VM"
        )));
    }

    let rootfs_path = mounts.artifact_out.join("rootfs.ext4");
    if !rootfs_path.exists() {
        return Err(BuilderVmError::ExtractionFailed(format!(
            "build reported success but {} is missing",
            rootfs_path.display()
        )));
    }
    let kernel_path = {
        let p = mounts.artifact_out.join("vmlinux");
        if p.exists() { Some(p) } else { None }
    };
    let revision_hash = read_revision_hash(&mounts.artifact_out);
    let accessible = crate::builder_vm::ArtifactSidecar::read_from_dir(&mounts.artifact_out)
        .ok()
        .flatten()
        .map(|s| s.accessible);

    Ok(BuilderArtifacts {
        rootfs_path,
        kernel_path,
        revision_hash,
        lock_hash: None,
        accessible,
    })
}

fn read_revision_hash(out: &Path) -> String {
    // `.nix-out-path` is written by `build_cmd_script` so the host
    // can recover the source store path without re-parsing nix's
    // stdout. Extract just the hash prefix to match
    // `extract_revision_hash` from the microsandbox path.
    let marker = out.join(".nix-out-path");
    let Ok(body) = fs::read_to_string(&marker) else {
        return String::new();
    };
    body.trim()
        .trim_start_matches("/nix/store/")
        .split('-')
        .next()
        .unwrap_or("")
        .to_string()
}

fn string_or_err(path: &Path, label: &str) -> Result<String, BuilderVmError> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        BuilderVmError::ExtractionFailed(format!(
            "{label} path is not UTF-8: {}",
            path.display()
        ))
    })
}

fn path_has_nul_or_non_utf8(path: &Path) -> bool {
    // libkrun's FFI takes paths via `CString`, which forbids interior
    // NULs and (by way of the `&Path` → `&str` step we do in
    // `string_or_err`) non-UTF-8 sequences. Reject both here so the
    // failure surfaces as a typed BuilderVmError with the offending
    // path in the message rather than the opaque `Error::InvalidCString`
    // libkrun would return at boot.
    match path.as_os_str().to_str() {
        None => true,
        Some(s) => s.as_bytes().contains(&0),
    }
}

// ──────────────────────── tests ────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::tempdir;

    fn make_job() -> BuilderJob {
        BuilderJob {
            flake_ref: "path:/work".to_string(),
            attr_path: "packages.aarch64-linux.default".to_string(),
        }
    }

    fn make_mounts(flake_src: &Path, artifact_out: &Path) -> BuilderMounts {
        BuilderMounts {
            flake_src: flake_src.to_path_buf(),
            host_nix_store: None,
            artifact_out: artifact_out.to_path_buf(),
        }
    }

    #[test]
    fn defaults_match_plan_72_w1() {
        let vm = LibkrunBuilderVm::new();
        assert_eq!(vm.vcpus, 4);
        assert_eq!(vm.memory_mib, 4096);
        assert_eq!(vm.nix_store_mib, 65_536);
        assert!(!vm.offline);
        assert!(vm.image.is_none());
    }

    fn write_fake_image(dir: &Path) -> BuilderVmImage {
        fs::write(dir.join(BuilderVmImage::KERNEL_FILENAME), b"fake kernel").unwrap();
        fs::write(dir.join(BuilderVmImage::ROOTFS_FILENAME), b"fake rootfs").unwrap();
        fs::write(
            dir.join(BuilderVmImage::CMDLINE_FILENAME),
            "console=hvc0 root=/dev/vda ro init=/usr/local/bin/mvm-builder-init\n",
        )
        .unwrap();
        BuilderVmImage::load_from_dir(dir).unwrap()
    }

    #[test]
    fn builder_vm_image_load_from_dir_reads_all_three_files() {
        let dir = tempdir().unwrap();
        let img = write_fake_image(dir.path());
        assert!(img.kernel.ends_with(BuilderVmImage::KERNEL_FILENAME));
        assert!(img.rootfs.ends_with(BuilderVmImage::ROOTFS_FILENAME));
        assert!(
            img.cmdline.contains("init=/usr/local/bin/mvm-builder-init"),
            "cmdline should include init=; got {:?}",
            img.cmdline
        );
        assert!(
            !img.cmdline.ends_with('\n'),
            "load_from_dir should trim trailing newline"
        );
    }

    #[test]
    fn builder_vm_image_missing_file_errors() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(BuilderVmImage::KERNEL_FILENAME), b"k").unwrap();
        fs::write(dir.path().join(BuilderVmImage::ROOTFS_FILENAME), b"r").unwrap();
        // cmdline absent
        let err = BuilderVmImage::load_from_dir(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            matches!(err, BuilderVmError::BuilderImageMissing(_)),
            "want BuilderImageMissing, got {msg}"
        );
        assert!(
            msg.contains(BuilderVmImage::CMDLINE_FILENAME),
            "error should name the missing file: {msg}"
        );
    }

    #[test]
    fn builder_vm_image_empty_cmdline_errors() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(BuilderVmImage::KERNEL_FILENAME), b"k").unwrap();
        fs::write(dir.path().join(BuilderVmImage::ROOTFS_FILENAME), b"r").unwrap();
        fs::write(dir.path().join(BuilderVmImage::CMDLINE_FILENAME), b"   \n").unwrap();
        let err = BuilderVmImage::load_from_dir(dir.path()).unwrap_err();
        assert!(
            matches!(err, BuilderVmError::BuilderImageMissing(_)),
            "empty-after-trim cmdline should error: {err:?}"
        );
    }

    #[test]
    fn with_image_injects_resolved_image() {
        let dir = tempdir().unwrap();
        let img = write_fake_image(dir.path());
        let vm = LibkrunBuilderVm::new().with_image(img.clone());
        assert!(vm.image.is_some());
        let stored = vm.image.unwrap();
        assert_eq!(stored.kernel, img.kernel);
        assert_eq!(stored.rootfs, img.rootfs);
        assert_eq!(stored.cmdline, img.cmdline);
    }

    #[test]
    fn with_resources_overrides_fields() {
        let vm = LibkrunBuilderVm::new()
            .with_resources(2, 1024)
            .with_nix_store_size(8192);
        assert_eq!(vm.vcpus, 2);
        assert_eq!(vm.memory_mib, 1024);
        assert_eq!(vm.nix_store_mib, 8192);
    }

    #[test]
    fn host_can_build_always_false() {
        // CLAUDE.md §"Host Nix is never used by mvmctl" — even on a
        // host with nix installed and `which nix` succeeding, the
        // libkrun path must not delegate. This test pins the
        // invariant so a future "well, let's check…" PR can't sneak
        // it in.
        let vm = LibkrunBuilderVm::new();
        assert!(!vm.host_can_build().unwrap());
    }

    #[test]
    fn validate_mounts_rejects_missing_flake_src() {
        let out = tempdir().unwrap();
        let mounts = make_mounts(Path::new("/nonexistent/path/xyz"), out.path());
        let err = validate_mounts(&mounts).unwrap_err();
        assert!(
            matches!(err, BuilderVmError::ExtractionFailed(_)),
            "want ExtractionFailed, got {err:?}"
        );
    }

    #[test]
    fn validate_mounts_creates_artifact_out() {
        let flake_src = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let artifact_out = parent.path().join("new-subdir");
        assert!(!artifact_out.exists());
        let mounts = make_mounts(flake_src.path(), &artifact_out);
        validate_mounts(&mounts).expect("create_dir_all should succeed");
        assert!(artifact_out.exists(), "artifact_out not created");
    }

    #[test]
    fn job_id_is_deterministic_for_same_job() {
        let job = make_job();
        let id_a = job_id_for(&job);
        let id_b = job_id_for(&job);
        assert_eq!(id_a, id_b);
        assert!(!id_a.is_empty(), "job id should be non-empty");
    }

    #[test]
    fn job_id_differs_for_different_jobs() {
        let job_a = make_job();
        let mut job_b = make_job();
        job_b.attr_path = "packages.x86_64-linux.default".to_string();
        assert_ne!(job_id_for(&job_a), job_id_for(&job_b));
    }

    #[test]
    fn stage_job_dir_writes_cmd_and_result() {
        let cache = tempdir().unwrap();
        let job = make_job();
        let id = job_id_for(&job);
        let dir = stage_job_dir(cache.path(), &id, &job, false).unwrap();
        let cmd = fs::read_to_string(dir.join("cmd.sh")).unwrap();
        assert!(cmd.contains("nix build"), "cmd.sh missing nix build line");
        assert!(
            cmd.contains("'path:/work'"),
            "cmd.sh missing single-quoted flake_ref: {cmd}"
        );
        let result = fs::read_to_string(dir.join("result")).unwrap();
        assert_eq!(result, "", "result file should be empty pre-boot");
    }

    #[test]
    fn ensure_nix_store_image_sparse_allocates_once() {
        let cache = tempdir().unwrap();
        let img_a = ensure_nix_store_image(cache.path(), 16).unwrap();
        let meta_a = fs::metadata(&img_a).unwrap();
        assert_eq!(meta_a.len(), 16 * 1024 * 1024);

        // Idempotent: second call returns the same path without
        // re-truncating (file is preserved as-is).
        let img_b = ensure_nix_store_image(cache.path(), 1024).unwrap();
        assert_eq!(img_a, img_b);
        let meta_b = fs::metadata(&img_b).unwrap();
        assert_eq!(
            meta_b.len(),
            16 * 1024 * 1024,
            "second call must not resize an existing image"
        );
    }

    #[test]
    fn ensure_builder_vm_image_signals_missing_w5() {
        let err = ensure_builder_vm_image().unwrap_err();
        assert!(
            matches!(err, BuilderVmError::BuilderImageMissing(_)),
            "want BuilderImageMissing (plan 72 W5 placeholder), got {err:?}"
        );
    }

    #[test]
    fn virtiofs_tags_match_guest_init() {
        // mvm-builder-init's VIRTIOFS_SHARES must match these tags or
        // the guest can't mount what the host attaches. Locking the
        // contract here on the host side; the matching test on the
        // guest side lives in mvm_builder_init's tests.
        assert_eq!(VIRTIOFS_TAG_WORK, "mvm-work");
        assert_eq!(VIRTIOFS_TAG_OUT, "mvm-out");
        assert_eq!(VIRTIOFS_TAG_JOB, "mvm-job");
        // BUILDER_GUEST_JOB_DIR doubles as the in-guest path; the
        // init crate's JOB_DIR constant matches.
        assert_eq!(BUILDER_GUEST_JOB_DIR, "/job");
    }

    #[test]
    fn build_cmd_script_quotes_unusual_chars() {
        let job = BuilderJob {
            flake_ref: "weird's flake".to_string(),
            attr_path: "packages.x86_64-linux.default".to_string(),
        };
        let script = build_cmd_script(&job, false);
        assert!(
            script.contains("'weird'\\''s flake'"),
            "single quotes not properly escaped: {script}"
        );
    }

    #[test]
    fn build_cmd_script_online_omits_substituters() {
        let job = make_job();
        let script = build_cmd_script(&job, false);
        assert!(
            !script.contains("substituters ="),
            "online build script must not disable substituters: {script}"
        );
        assert!(
            script.contains("experimental-features = nix-command flakes"),
            "online build script must still enable flakes CLI: {script}"
        );
    }

    #[test]
    fn build_cmd_script_offline_disables_substituters() {
        let job = make_job();
        let script = build_cmd_script(&job, true);
        // Offline mode emits a multi-line NIX_CONFIG that disables
        // every substituter so nix builds entirely from the seed /nix.
        assert!(
            script.contains("substituters ="),
            "offline build script must zero out substituters: {script}"
        );
        assert!(
            script.contains("experimental-features = nix-command flakes"),
            "offline build script must still enable flakes CLI: {script}"
        );
    }

    #[test]
    fn with_offline_flips_field() {
        let online = LibkrunBuilderVm::new();
        assert!(!online.offline);
        let offline = LibkrunBuilderVm::new().with_offline();
        assert!(offline.offline);
        // Other resources unaffected.
        assert_eq!(offline.vcpus, online.vcpus);
        assert_eq!(offline.memory_mib, online.memory_mib);
    }

    #[test]
    fn recording_console_capture_collects_lines_in_order() {
        let mut cap = RecordingConsoleCapture::default();
        cap.line(ConsoleSource::Boot, "boot line 1");
        cap.line(ConsoleSource::JobStderr, "job err");
        cap.line(ConsoleSource::Boot, "boot line 2");
        assert_eq!(cap.lines.len(), 3);
        assert_eq!(cap.lines[0], (ConsoleSource::Boot, "boot line 1".to_string()));
        assert_eq!(
            cap.lines[1],
            (ConsoleSource::JobStderr, "job err".to_string())
        );
        assert_eq!(cap.lines[2], (ConsoleSource::Boot, "boot line 2".to_string()));
    }

    #[test]
    fn println_console_capture_is_send_and_constructible() {
        // Compile-time check: the trait object must be Send so plan 57
        // W3's poll thread can own it. Construction is a no-arg default,
        // so callers don't need to know what fields exist.
        fn assert_send<T: Send>() {}
        assert_send::<PrintlnConsoleCapture>();
        assert_send::<Box<dyn ConsoleCapture>>();
        let _: Box<dyn ConsoleCapture> = Box::new(PrintlnConsoleCapture);
    }

    #[test]
    fn validate_mounts_rejects_non_utf8_flake_src() {
        // Construct a non-UTF-8 OsString — Unix-only because Windows
        // OsStrings are 16-bit and the failure shape differs. Skip
        // on platforms where invalid sequences can't be expressed.
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid_bytes = vec![0xFFu8, 0xFE, b'/', b'f', b'l', b'a', b'k', b'e'];
            let os_path = OsString::from_vec(invalid_bytes);
            let flake_src = PathBuf::from(os_path);
            // We can't easily create() a file with this path on every
            // filesystem; instead skip the existence check by checking
            // the helper directly.
            assert!(
                path_has_nul_or_non_utf8(&flake_src),
                "non-UTF-8 path should fail the helper"
            );
        }
        // Make sure the test compiles on non-unix too — no-op.
        let _ = OsString::new();
    }

    #[test]
    fn validate_mounts_rejects_nul_byte_path() {
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::ffi::OsStringExt;
            let bytes = vec![b'/', b'f', b'l', 0, b'a', b'k', b'e'];
            let os_path = OsString::from_vec(bytes);
            let path = PathBuf::from(os_path);
            assert!(
                path_has_nul_or_non_utf8(&path),
                "path with embedded NUL should fail the helper"
            );
        }
    }
}
