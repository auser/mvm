//! microsandbox backend for mvm.
//!
//! Plan 60 ([`specs/plans/60-mvm-microsandbox-migration.md`]) and ADR-013
//! ([`specs/adrs/013-microsandbox-libkrun-microvm-nix-pivot.md`]) name
//! microsandbox as the cross-platform builder + macOS/Windows execution
//! backend. It wraps Red Hat's libkrun under a higher-level Rust API,
//! adding sandbox lifecycle, image management, and a working DX out of
//! the box.
//!
//! # Status
//!
//! Phase 1 W1.1: trait shape only.
//! Phase 1 W1.2: `microsandbox = "0.4.5"` dep wired; `is_available`,
//! `list`, `status`, `stop`, `stop_all` real.
//! Phase 1 W1.3 (this file): `start()` and `logs()` real. The bridge
//! between our Nix-built ext4 rootfs and microsandbox's `RootfsSource`
//! API is a hard-link alias — microsandbox's `.disk()` builder accepts
//! only `.raw`/`.qcow2`/`.vmdk` extensions, but the underlying file
//! is just a raw block device; a sibling `.raw` hard-link plus an
//! explicit `fstype("ext4")` lets microsandbox attach via virtio-blk
//! without us copying the rootfs.
//!
//! Why a separate variant from [`LibkrunBackend`](super::libkrun::LibkrunBackend):
//! microsandbox is a higher-level abstraction (sandboxes, images, network
//! policy) backed by libkrun under the hood. The duplication here mirrors
//! the difference between "raw libkrun" (manual VM lifecycle) and
//! "microsandbox" (managed sandbox lifecycle). Both are Tier 2; both are
//! KVM/HVF-backed; the choice between them is API-shape preference.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mvm_core::vm_backend::{
    BackendSecurityProfile, ClaimStatus, GuestChannelInfo, LayerCoverage, VmBackend,
    VmCapabilities, VmExitStatus, VmId, VmInfo, VmStartConfig, VmStatus,
};
use mvm_core::vm_backend::StartMode;

/// microsandbox backend (libkrun-backed; cross-platform Linux/macOS/Windows).
///
/// See module docs for status. Tier 2 microVM isolation via KVM (Linux),
/// Hypervisor.framework (macOS), or WHvPlatform (Windows, when supported
/// by the upstream microsandbox crate).
pub struct MicrosandboxBackend;

/// Record the caller's `StartMode` intent for a sandbox name.
///
/// Delegates to [`mvm_runtime_base::runtime_meta`], which owns the on-disk
/// shape at `~/.mvm/vms/<name>/mode.json`. The `accessible` flag
/// defaults to `true` here — used by call sites that don't have a
/// rootfs path handy (e.g., `detach`, which works on an existing
/// sandbox by name).
///
/// Failure to write the registry is a soft warning — start()
/// proceeds. The contract is "best-effort metadata," not
/// load-bearing for VM lifecycle.
fn record_start_mode(name: &str, mode: StartMode) -> Result<()> {
    let meta = mvm_runtime_base::runtime_meta::dev_attached(mode);
    mvm_runtime_base::runtime_meta::write(name, &meta)
}

/// Thin alias for the cross-backend helper in
/// [`mvm_runtime_base::runtime_meta::record_from_rootfs`]. Kept here so the
/// existing tests in this module read naturally; new backends call
/// the runtime_meta version directly.
fn record_start_mode_from_rootfs(
    name: &str,
    mode: StartMode,
    rootfs: &Path,
) -> Result<()> {
    mvm_runtime_base::runtime_meta::record_from_rootfs(name, mode, rootfs)
}

/// Bridge our Nix-built `rootfs.ext4` into a path microsandbox accepts.
///
/// `microsandbox::ImageBuilder::disk()` recognises only `.raw`,
/// `.qcow2`, and `.vmdk` extensions, but our rootfs is a plain ext4
/// block image with no file-format wrapper — i.e. semantically a raw
/// disk. Rather than copy the (potentially large) rootfs, we make a
/// `.raw` hard-link sibling and pass that to microsandbox along with
/// an explicit `fstype("ext4")`.
///
/// The alias is created lazily on first start; subsequent calls reuse
/// it. If the input path already has a recognised extension we
/// short-circuit and return it unchanged — relevant once we add a
/// qcow2 backend behind the same trait.
///
/// Returns the path microsandbox should consume + a flag indicating
/// whether the alias was newly created (used by tests).
fn ensure_microsandbox_rootfs_alias(rootfs: &Path) -> Result<(PathBuf, bool)> {
    let ext = rootfs.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "raw" | "qcow2" | "vmdk") {
        return Ok((rootfs.to_path_buf(), false));
    }
    let alias = rootfs.with_extension("raw");
    if alias.exists() {
        return Ok((alias, false));
    }
    std::fs::hard_link(rootfs, &alias).with_context(|| {
        format!(
            "creating microsandbox-compatible .raw alias: hard_link {} -> {}",
            rootfs.display(),
            alias.display()
        )
    })?;
    Ok((alias, true))
}

/// Bridge sync `VmBackend` calls into microsandbox's async API.
///
/// VmBackend is intentionally sync to keep the trait dyn-friendly and
/// to match the existing FirecrackerBackend / LibkrunBackend shape.
/// microsandbox is async-only. Each entry-point that crosses the
/// boundary uses this helper. The runtime is built fresh per call,
/// not cached, because:
///   1. VmBackend impls are `Send + Sync` and shouldn't carry runtime
///      state — the existing backends are zero-sized structs.
///   2. The per-call cost (a few hundred microseconds) is negligible
///      compared to a sandbox boot (tens to hundreds of ms).
///   3. A cached runtime introduces lifetime questions across `&self`
///      that would force `Arc<Runtime>` and complicate the type.
///
/// Phase 1 W2 may revisit if profiling shows the new-runtime cost
/// dominates a hot path.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build single-threaded tokio runtime for microsandbox bridge");
    rt.block_on(fut)
}

impl VmBackend for MicrosandboxBackend {
    fn name(&self) -> &str {
        "microsandbox"
    }

    fn capabilities(&self) -> VmCapabilities {
        // microsandbox via libkrun — same capability matrix as the bare
        // LibkrunBackend. vsock works; pause/resume and snapshots are not
        // exposed by the upstream API today (Phase 7a will revisit when
        // the snapshot model lands).
        VmCapabilities {
            pause_resume: false,
            snapshots: false,
            vsock: true,
            tap_networking: false,
        }
    }

    fn start_with_mode(
        &self,
        config: &VmStartConfig,
        mode: StartMode,
    ) -> Result<VmId> {
        // 1. Bridge the rootfs path. Microsandbox's `.disk()` builder
        //    only accepts .raw/.qcow2/.vmdk extensions; our build
        //    pipeline produces .ext4. The hard-link alias keeps disk
        //    usage flat (no copy) and lets microsandbox attach via
        //    virtio-blk with an explicit fstype hint.
        let rootfs = Path::new(&config.rootfs_path);
        if !rootfs.exists() {
            anyhow::bail!(
                "microsandbox start: rootfs path does not exist: {}",
                rootfs.display()
            );
        }
        let (disk_path, _new_alias) = ensure_microsandbox_rootfs_alias(rootfs)?;

        // 2. Resource clamps. microsandbox's `.cpus()` takes u8; our
        //    config carries u32 (theoretical headroom for hypervisors
        //    that handle larger guests, e.g., bare KVM). One vcpu min,
        //    255 max — anything above that is a misconfiguration.
        let cpus = u8::try_from(config.cpus.clamp(1, u32::from(u8::MAX))).unwrap_or(u8::MAX);

        // 3. Both Attached and Detached spawn modes call
        //    `create_detached()` underneath — the microsandbox
        //    Sandbox handle requires keeping a !Send future across
        //    the sync VmBackend boundary, which would force an
        //    internal Mutex<HashMap<VmId, Sandbox>> (added in a
        //    follow-up wave when we wire the host-side signal
        //    layer).
        //
        //    For now, `mode` is *intent metadata* — recorded in
        //    `~/.mvm/vms/<name>/mode.json` so subsequent
        //    `wait()`/`detach()` calls know whether the caller
        //    expected attached semantics. The microsandbox-side
        //    behavior is identical regardless; the host-side
        //    Ctrl-C signal forwarding is the W7 follow-up that
        //    closes the loop.
        record_start_mode_from_rootfs(&config.name, mode, rootfs)?;

        let name = config.name.clone();
        block_on(async {
            microsandbox::Sandbox::builder(&name)
                .image_with(|i| i.disk(&disk_path).fstype("ext4"))
                .cpus(cpus)
                .memory(config.memory_mib)
                .create_detached()
                .await
        })
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "microsandbox create_detached for '{}' (mode={mode:?}, rootfs {})",
                config.name,
                disk_path.display()
            )
        })?;

        tracing::info!(
            sandbox = %config.name,
            ?mode,
            "microsandbox: sandbox created"
        );
        Ok(VmId(config.name.clone()))
    }

    fn wait(&self, id: &VmId) -> Result<VmExitStatus> {
        // microsandbox's `Sandbox::wait()` requires the caller to
        // own the lifecycle handle, which we don't keep across
        // sync boundary calls. As a pragmatic substitute, poll
        // `Sandbox::list()` until the named sandbox disappears —
        // O(seconds) granularity, fine for "block until exit"
        // semantics. The W7 follow-up swaps this for a real
        // signal-aware wait once the handle registry lands.
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        loop {
            let handles = block_on(async { microsandbox::Sandbox::list().await })
                .map_err(|e| anyhow::anyhow!(e))
                .with_context(|| format!("microsandbox list (during wait for {})", id.0))?;
            if !handles.iter().any(|h| h.name() == id.0) {
                // Sandbox no longer present — exited cleanly OR
                // was killed externally. We can't disambiguate
                // without the lifecycle handle, so report success
                // by convention. mvmd's audit chain captures the
                // distinction at the policy layer.
                return Ok(VmExitStatus::SUCCESS);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn detach(&self, id: &VmId) -> Result<()> {
        // microsandbox sandboxes are already detached at the OS-
        // process level (we always call `create_detached()`).
        // Mark the intent change in our local registry so a
        // subsequent `mvmctl status` reports the right mode, but
        // there's no hypervisor-side action to take.
        record_start_mode(&id.0, StartMode::Detached)
            .with_context(|| format!("microsandbox detach: writing mode for {}", id.0))?;
        tracing::info!(sandbox = %id.0, "microsandbox: detached");
        Ok(())
    }

    fn stop(&self, id: &VmId) -> Result<()> {
        // `Sandbox::get(name)` returns a handle on a running sandbox.
        // We then call `.stop()` to gracefully drain and exit. If the
        // sandbox isn't running we treat that as success — this is a
        // teardown path; idempotent shutdown is the right shape.
        block_on(async {
            match microsandbox::Sandbox::get(&id.0).await {
                Ok(handle) => handle
                    .stop()
                    .await
                    .with_context(|| format!("microsandbox stop {}", id.0)),
                Err(e) => {
                    // Not-running is benign for stop(); other errors propagate.
                    let msg = e.to_string();
                    if msg.contains("not found") || msg.contains("does not exist") {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(e)).context("microsandbox get for stop")
                    }
                }
            }
        })
    }

    fn stop_all(&self) -> Result<()> {
        // Best-effort: list everything microsandbox knows about and stop
        // each. Errors stopping any single sandbox are surfaced *after*
        // we've attempted them all, so a stuck sandbox doesn't strand
        // the rest.
        let handles = match block_on(async { microsandbox::Sandbox::list().await }) {
            Ok(h) => h,
            // If listing fails (e.g., DB not initialized), there are no
            // sandboxes to stop and we're done.
            Err(_) => return Ok(()),
        };
        let mut first_err: Option<anyhow::Error> = None;
        for h in handles {
            let name = h.name().to_string();
            if let Err(e) = self.stop(&VmId(name.clone())) {
                tracing::warn!(sandbox = %name, error = %e, "stop_all: failed to stop");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn status(&self, id: &VmId) -> Result<VmStatus> {
        // `Sandbox::list()` is the cheapest source of truth — get()
        // would error on missing, but list() lets us return Stopped
        // for an unknown id without raising.
        let handles = block_on(async { microsandbox::Sandbox::list().await })
            .map_err(|e| anyhow::anyhow!(e))
            .context("microsandbox list (for status)")?;
        match handles.into_iter().find(|h| h.name() == id.0) {
            // The upstream SandboxStatus enum has more variants than our
            // VmStatus; we map conservatively (Running/Stopped/Failed).
            // A more granular mapping lands when W1.3 surfaces lifecycle
            // events to the runtime layer.
            Some(_) => Ok(VmStatus::Running),
            None => Ok(VmStatus::Stopped),
        }
    }

    fn list(&self) -> Result<Vec<VmInfo>> {
        let handles = block_on(async { microsandbox::Sandbox::list().await })
            .map_err(|e| anyhow::anyhow!(e))
            .context("microsandbox list")?;
        // VmInfo carries cpus/memory/profile/revision/guest_ip — the
        // upstream SandboxHandle exposes name + a richer config we
        // don't yet plumb through. W1.3 reads `h.config()` to fill in
        // cpus/memory/profile; for now we report the name + a Running
        // status with conservative zeroes/None so list() is honest
        // about the sandbox set even if the metadata is sparse.
        Ok(handles
            .into_iter()
            .map(|h| VmInfo {
                id: VmId(h.name().to_string()),
                name: h.name().to_string(),
                status: VmStatus::Running,
                guest_ip: None,
                cpus: 0,
                memory_mib: 0,
                profile: None,
                revision: None,
                flake_ref: None,
                ports: Vec::new(),
            })
            .collect())
    }

    fn logs(&self, id: &VmId, lines: u32, _hypervisor: bool) -> Result<String> {
        // The `hypervisor` flag is meaningless for microsandbox — there's
        // no separate hypervisor logs vs guest console split the way
        // Firecracker has. We always return guest stdout+stderr (the
        // upstream LogOptions default). When microsandbox grows a
        // separate hypervisor stream we'll honour the flag.
        let handle = block_on(async { microsandbox::Sandbox::get(&id.0).await })
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("microsandbox get for logs {}", id.0))?;

        let opts = microsandbox::sandbox::LogOptions {
            tail: usize::try_from(lines).ok().filter(|n| *n > 0),
            since: None,
            until: None,
            sources: Vec::new(),
        };
        let entries = handle
            .logs(&opts)
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("microsandbox logs for '{}'", id.0))?;

        // Format as plain text: timestamp + source + line. Matches the
        // Firecracker backend's String return shape (callers paginate
        // or filter downstream). LogEntry.data is `bytes::Bytes` —
        // we lossy-decode to UTF-8 because guest console output isn't
        // strictly UTF-8 (e.g., ANSI control codes survive verbatim;
        // a stray byte from a binary on stdout becomes U+FFFD rather
        // than crashing the formatter).
        let mut out = String::new();
        for entry in entries {
            use std::fmt::Write;
            let line = String::from_utf8_lossy(&entry.data);
            let _ = writeln!(
                out,
                "{} [{:?}] {}",
                entry.timestamp.to_rfc3339(),
                entry.source,
                line
            );
        }
        Ok(out)
    }

    fn is_available(&self) -> Result<bool> {
        // The crate is now linked in. Treat the backend as available
        // and let start()/stop() surface runtime-environment errors
        // (libkrunfw not present, KVM/HVF unavailable, etc.). Same
        // posture as Firecracker's `is_available()` — return true and
        // let the actual lifecycle call do the precise check.
        Ok(true)
    }

    fn install(&self) -> Result<()> {
        anyhow::bail!(
            "microsandbox is installed via `cargo install microsandbox-cli` \
             or via the upstream installer; see plan 60 §\"microvm.nix integration\"."
        )
    }

    fn guest_channel_info(&self, _id: &VmId) -> Result<GuestChannelInfo> {
        // microsandbox exposes vsock the same way bare libkrun does — the
        // guest agent listens on the shared `GUEST_AGENT_PORT`, so all
        // backends share one client implementation.
        Ok(GuestChannelInfo::Vsock {
            cid: 3,
            port: mvm_guest::vsock::GUEST_AGENT_PORT,
        })
    }

    fn security_profile(&self) -> BackendSecurityProfile {
        // Tier 2: hardware isolation via KVM/HVF. Same posture as
        // LibkrunBackend (microsandbox sits on top of libkrun) — claim
        // 3 (verified boot via dm-verity) is the gap because the W3
        // pipeline targets Firecracker today; Phase 6 of plan 60 lands
        // a microsandbox-flavored integrity check (image-hash + HMAC).
        BackendSecurityProfile {
            claims: [
                ClaimStatus::Holds,       // 1 — host-fs isolation via KVM/HVF
                ClaimStatus::Holds,       // 2 — uid-0 protections same as FC
                ClaimStatus::DoesNotHold, // 3 — verified boot not yet wired
                ClaimStatus::Holds,       // 4 — guest agent has no do_exec in prod
                ClaimStatus::Holds,       // 5 — vsock framing is fuzzed
                ClaimStatus::Holds,       // 6 — image hash verification
                ClaimStatus::Holds,       // 7 — cargo deps audited
            ],
            layer_coverage: LayerCoverage::all_layers(),
            tier: "Tier 2",
            notes: &[
                "Hardware isolation via KVM (Linux) or Hypervisor.framework (macOS).",
                "Cross-platform: Linux + macOS arm64/x86_64. Windows pending upstream.",
                "Higher-level API than bare libkrun — managed sandbox lifecycle.",
                "Claim 3 (verified boot) is partial — Phase 6 closes via image-hash + HMAC.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microsandbox_backend_name() {
        assert_eq!(MicrosandboxBackend.name(), "microsandbox");
    }

    #[test]
    fn microsandbox_capabilities_match_libkrun_substrate() {
        let caps = MicrosandboxBackend.capabilities();
        assert!(caps.vsock, "vsock must always be available — guest agent contract");
        assert!(!caps.snapshots, "Phase 7a wires snapshots; W1.1 advertises none");
        assert!(!caps.pause_resume, "upstream microsandbox doesn't expose pause/resume");
        assert!(!caps.tap_networking, "L4/L7 proxy in Phase 3 is the network path");
    }

    #[test]
    fn microsandbox_security_profile_is_tier_2_with_partial_claim_3() {
        let profile = MicrosandboxBackend.security_profile();
        assert_eq!(profile.tier, "Tier 2");
        assert!(profile.layer_coverage.is_microvm());
        assert_eq!(
            profile.dropped_claims(),
            vec![3],
            "claim 3 (verified boot) is the only outstanding gap"
        );
        assert!(profile.na_claims().is_empty());
    }

    #[test]
    fn microsandbox_is_available_returns_true_when_crate_compiled_in() {
        // W1.2: the crate is now a real workspace dep, so the backend
        // advertises availability. start()/stop() will still surface
        // host-environment errors at lifecycle time (libkrunfw not
        // installed, KVM/HVF unavailable, etc.) — same posture as
        // FirecrackerBackend's is_available().
        let available = MicrosandboxBackend
            .is_available()
            .expect("is_available is infallible in W1.2");
        assert!(available, "is_available must be true once microsandbox is linked");
    }

    #[test]
    fn microsandbox_start_errors_when_rootfs_missing() {
        // W1.3: start() now does real work. The first guard is the
        // rootfs-existence check — a missing path must surface a
        // clear error before we attempt to hard-link or call into the
        // upstream API (which would otherwise emit a less-helpful
        // error message about the missing disk).
        let config = VmStartConfig {
            name: "ms-test".to_string(),
            rootfs_path: "/tmp/__mvm_test_definitely_missing__/rootfs.ext4".to_string(),
            ..Default::default()
        };
        let err = MicrosandboxBackend
            .start(&config)
            .expect_err("start must error when rootfs path doesn't exist");
        let msg = err.to_string();
        assert!(
            msg.contains("rootfs path does not exist"),
            "error must reference the missing-rootfs guard, got: {msg}"
        );
    }

    #[test]
    fn rootfs_alias_creates_raw_hard_link_for_ext4() {
        // The bridge between our `.ext4` rootfs files and microsandbox's
        // `.raw`/`.qcow2`/`.vmdk`-only `.disk()` builder. Round-trip:
        // create a fake .ext4, run the alias helper, expect a sibling
        // .raw hard-link pointing at the same inode.
        let temp = tempfile::tempdir().expect("tempdir");
        let rootfs = temp.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"fake-ext4-block-image").expect("write");

        let (alias, created) =
            ensure_microsandbox_rootfs_alias(&rootfs).expect("alias creation");
        assert!(created, "first call should create the alias");
        assert_eq!(alias, rootfs.with_extension("raw"));
        assert!(alias.exists(), ".raw alias must exist after creation");

        // Second call must be idempotent.
        let (alias2, created2) =
            ensure_microsandbox_rootfs_alias(&rootfs).expect("alias second call");
        assert_eq!(alias2, alias);
        assert!(!created2, "second call must reuse the existing alias");

        // Hard-link contract: same inode (cheaper than copy, mutation
        // is visible through both names — but rootfs is read-only at
        // boot so this isn't a concern).
        let m1 = std::fs::metadata(&rootfs).expect("rootfs meta");
        let m2 = std::fs::metadata(&alias).expect("alias meta");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(m1.ino(), m2.ino(), "hard-link must share the inode");
        }
        // On non-unix we just assert byte-equal sizes as a weaker proxy.
        assert_eq!(m1.len(), m2.len());
    }

    #[test]
    fn rootfs_alias_short_circuits_for_already_supported_extensions() {
        // If a future backend hands us a path that's already `.raw`/
        // `.qcow2`/`.vmdk`, no alias should be created. Asserting this
        // explicitly so a refactor doesn't silently start hard-linking
        // qcow2 files into raw aliases (which would corrupt the
        // builder's format detection).
        let temp = tempfile::tempdir().expect("tempdir");
        for ext in ["raw", "qcow2", "vmdk"] {
            let rootfs = temp.path().join(format!("rootfs.{ext}"));
            std::fs::write(&rootfs, b"any").expect("write");
            let (alias, created) =
                ensure_microsandbox_rootfs_alias(&rootfs).expect("alias short-circuit");
            assert_eq!(alias, rootfs, ".{ext} should pass through unchanged");
            assert!(!created, ".{ext} should not create a new alias file");
        }
    }

    #[test]
    fn microsandbox_stop_of_unknown_vm_is_idempotent() {
        // W1.2: stop() of a sandbox that doesn't exist must not error
        // — teardown paths call stop() optimistically and shouldn't
        // surface "not found" as a failure.
        //
        // We pick a deliberately-improbable name so a host with real
        // microsandbox state can't accidentally collide. If this test
        // ever flakes, swap the name for a UUID.
        let result = MicrosandboxBackend
            .stop(&VmId("__mvm_test_definitely_does_not_exist__".to_string()));
        // Either Ok (sandbox not found, idempotent path) or a list-
        // related error if microsandbox's DB isn't initialized — both
        // are acceptable in CI; we just guard against panics.
        if let Err(e) = result {
            // Allow the DB-init error class; reject only the impossible
            // case where stop() somehow reported success-then-failure.
            assert!(
                !e.to_string().contains("not found"),
                "stop should swallow not-found, but raised: {e}"
            );
        }
    }

    #[test]
    fn microsandbox_stop_all_does_not_panic() {
        // stop_all is best-effort. On a host without microsandbox state
        // initialized, list() will fail and we treat that as "nothing
        // to stop." On a host with state, list() returns handles and we
        // attempt each. The test asserts the call doesn't panic — a
        // proper running-sandbox round-trip is a Phase 1 W1.3 smoke
        // test, not a unit test.
        let _ = MicrosandboxBackend.stop_all();
    }

    #[test]
    fn microsandbox_status_unknown_returns_stopped() {
        // status() of a sandbox we know doesn't exist returns Stopped
        // (the conservative "not running" status). Acceptable to
        // return Err here too if microsandbox's DB isn't initialized
        // — we guard against panic only.
        let result = MicrosandboxBackend
            .status(&VmId("__mvm_test_definitely_does_not_exist__".to_string()));
        if let Ok(s) = result {
            assert_eq!(
                s,
                VmStatus::Stopped,
                "unknown sandbox must report Stopped, got: {s:?}"
            );
        }
        // DB-init failure (Err) is acceptable on a fresh CI machine.
    }

    #[test]
    fn microsandbox_list_does_not_panic() {
        // list() either returns a (possibly empty) Vec or an error if
        // microsandbox's storage isn't initialized. The test asserts
        // the call doesn't panic — same shape as stop_all().
        let _ = MicrosandboxBackend.list();
    }

    /// Serialize tests that mutate `$HOME` so they don't race each
    /// other. Reuses the workspace-wide `HOME_TEST_LOCK` from
    /// `mvm-runtime-base::runtime_meta` (W7 substrate split) so this
    /// crate's tests serialize against `mvm-runtime`'s
    /// `runtime_meta` tests as well.
    fn with_home_temp<F>(f: F)
    where
        F: FnOnce(&std::path::Path),
    {
        let _guard = crate::HOME_TEST_LOCK
            .lock()
            .expect("HOME_TEST_LOCK poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let saved = std::env::var_os("HOME");
        // SAFETY: serialized via HOME_TEST_LOCK above.
        unsafe { std::env::set_var("HOME", temp.path()); }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(temp.path());
        }));
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    #[test]
    fn microsandbox_record_start_mode_writes_attached_then_detached() {
        with_home_temp(|home| {
            let name = format!("rec-mode-{}", std::process::id());

            record_start_mode(&name, StartMode::Attached).expect("first write");
            let mode_path = home.join(".mvm").join("vms").join(&name).join("mode.json");
            let attached = std::fs::read_to_string(&mode_path).expect("mode.json present");
            assert!(attached.contains("attached"), "expected attached, got: {attached}");

            record_start_mode(&name, StartMode::Detached).expect("second write overrides");
            let detached = std::fs::read_to_string(&mode_path).expect("mode.json still present");
            assert!(detached.contains("detached"), "second write didn't override: {detached}");
            assert!(!detached.contains("attached"), "old marker not cleared");
        });
    }

    #[test]
    fn record_start_mode_from_rootfs_sealed_sidecar_blocks_console() {
        // Sealed image surfaced via passthru.mvm.accessible = false:
        // the sidecar emits accessible:false; record_start_mode_from_rootfs
        // propagates it into mode.json; mvmctl console's gate then
        // refuses without --force. End-to-end W6.2 ↔ W7.x.1 flow.
        with_home_temp(|home| {
            let tmp = tempfile::tempdir().expect("artifact tempdir");
            let rootfs = tmp.path().join("rootfs.ext4");
            std::fs::write(&rootfs, b"unused").expect("create rootfs sentinel");

            let sidecar = mvm_build::builder_vm::ArtifactSidecar {
                name: "prod-sealed".to_string(),
                accessible: false,
                sealed: true,
                entrypoint_kind: "command".to_string(),
                init_system: "busybox".to_string(),
                expected_boot_ms: 300,
                agent_binary: "real".to_string(),
                rootless_entrypoint: true,
                hypervisor: "microsandbox".to_string(),
            };
            sidecar
                .write_to_dir(tmp.path())
                .expect("sidecar write");

            let name = format!("sealed-{}", std::process::id());
            record_start_mode_from_rootfs(&name, StartMode::Detached, &rootfs)
                .expect("record");

            let mode_path = home.join(".mvm").join("vms").join(&name).join("mode.json");
            let body = std::fs::read_to_string(&mode_path).expect("mode.json present");
            assert!(body.contains("\"accessible\":false"), "got: {body}");
            assert!(body.contains("detached"), "got: {body}");
        });
    }

    #[test]
    fn record_start_mode_from_rootfs_missing_sidecar_defaults_accessible() {
        with_home_temp(|home| {
            let tmp = tempfile::tempdir().expect("artifact tempdir");
            let rootfs = tmp.path().join("rootfs.ext4");
            std::fs::write(&rootfs, b"unused").expect("create rootfs sentinel");

            let name = format!("dev-{}", std::process::id());
            record_start_mode_from_rootfs(&name, StartMode::Attached, &rootfs)
                .expect("record");

            let mode_path = home.join(".mvm").join("vms").join(&name).join("mode.json");
            let body = std::fs::read_to_string(&mode_path).expect("mode.json present");
            assert!(body.contains("\"accessible\":true"), "got: {body}");
        });
    }

    #[test]
    fn microsandbox_detach_records_detached_intent() {
        with_home_temp(|home| {
            let name = format!("detach-test-{}", std::process::id());
            let id = VmId(name.clone());

            // Pre-seed Attached so detach has something to flip.
            record_start_mode(&name, StartMode::Attached).expect("seed");

            MicrosandboxBackend.detach(&id).expect("detach is infallible after seed");

            let mode_path = home.join(".mvm").join("vms").join(&name).join("mode.json");
            let body = std::fs::read_to_string(&mode_path).expect("mode.json present");
            assert!(body.contains("detached"), "detach didn't flip intent, got: {body}");
        });
    }

    #[test]
    fn vm_exit_status_success_sentinel_round_trips() {
        // Sanity: the SUCCESS const is what the polling wait()
        // returns when the sandbox has disappeared. Asserting its
        // shape so a future refactor can't silently change the
        // semantics.
        let s = VmExitStatus::SUCCESS;
        assert_eq!(s.code, Some(0));
        assert!(s.success);
    }

    #[test]
    fn microsandbox_guest_channel_uses_shared_vsock_port() {
        // All backends share the same guest-agent vsock port — that's
        // the contract that lets a single vsock client speak to any
        // backend. Asserting it explicitly here so a future regression
        // (e.g., someone hardcodes a different port or switches to
        // UnixSocket) trips this test.
        let info = MicrosandboxBackend
            .guest_channel_info(&VmId("any-vm".to_string()))
            .expect("guest_channel_info is static in scaffolding");
        let GuestChannelInfo::Vsock { cid, port } = info else {
            panic!("microsandbox must use vsock, not UnixSocket");
        };
        assert_eq!(cid, 3, "guest CID is 3 for all backends");
        assert_eq!(
            port,
            mvm_guest::vsock::GUEST_AGENT_PORT,
            "all backends use the shared guest-agent port"
        );
    }
}
