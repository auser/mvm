//! Apple Container dev environment + bundled image fetching.
//!
//! Extracted from `commands/mod.rs` as a pure mechanical refactor —
//! no behavior changes.

use anyhow::{Context, Result};

use mvm::vsock_transport::{VsockProxyTransport, VsockTransport};

use super::super::vm::console::console_interactive;
use crate::ui;

// ============================================================================
// Apple Container dev environment
// ============================================================================

pub(super) const DEV_VM_NAME: &str = "mvm-dev";

/// Check if the Apple Container dev VM is running *and* reachable
/// cross-process via the vsock proxy socket.
///
/// A live PID file alone is not enough — the daemon may have started but
/// failed to materialize the proxy socket, in which case `run_in_vm` calls
/// from other processes will fail. Treating that state as "not running"
/// keeps `dev status` honest with what `shell::run_in_vm` actually sees.
pub(in crate::commands) fn is_apple_container_dev_running() -> bool {
    let pid_running = mvm_providers::apple_container::list_ids()
        .iter()
        .any(|id| id == DEV_VM_NAME);
    if !pid_running {
        return false;
    }
    let proxy = mvm_providers::apple_container::vsock_proxy_path(DEV_VM_NAME);
    proxy.exists()
}

/// Boot the Apple Container dev VM, optionally opening an interactive console.
pub(super) fn cmd_dev_apple_container(cpus: u32, memory_gib: u32, open_shell: bool) -> Result<()> {
    let is_daemon = std::env::var("MVM_DEV_DAEMON").as_deref() == Ok("1");

    // When running as the daemon process, do the actual VM boot.
    if is_daemon {
        return cmd_dev_apple_container_daemon(cpus, memory_gib);
    }

    ui::progress("Starting dev environment via Apple Container...");

    if is_apple_container_dev_running() {
        if open_shell {
            ui::progress("Dev VM already running. Opening shell...");
            return console_interactive(DEV_VM_NAME);
        }
        ui::progress("Dev VM already running.");
        return Ok(());
    }

    // Clean up stale state from a previous process that died.
    cleanup_stale_dev_vm();

    // Ensure dev image exists (build if needed — this runs in the CLI process)
    let (kernel, rootfs) = ensure_dev_image()?;

    // ADR-002 W1.5: lock ~/.mvm and ~/.cache/mvm to 0700 on every
    // `dev up`. Idempotent — a fresh install creates them locked-
    // down, and a host that pre-dates this change gets chmod'd on
    // the first `dev up` after the upgrade.
    mvm_core::config::ensure_data_dir().with_context(|| "locking down data dir to mode 0700")?;
    mvm_core::config::ensure_cache_dir().with_context(|| "locking down cache dir to mode 0700")?;

    // Launch a background daemon process that keeps the VM alive.
    let exe = std::env::current_exe().context("cannot find current executable")?;
    let log_dir = format!("{}/dev", mvm_core::config::mvm_cache_dir());
    std::fs::create_dir_all(&log_dir)?;

    // Truncate previous-run daemon logs. launchd doesn't rotate, and
    // the daemon writes every guest-agent stdout/stderr there, so
    // these grow without bound. Each `dev up` is a logical session
    // boundary — losing prior logs is fine; preserving them forever
    // is the wrong default.
    //
    // ADR-002 W1.4: the daemon logs capture guest output the same way
    // console.log does — they are mode 0600 so a same-host other user
    // can't tail them. The truncate-on-each-up cadence is unchanged.
    use std::os::unix::fs::OpenOptionsExt as _;
    for name in ["daemon-stdout.log", "daemon-stderr.log"] {
        let path = format!("{log_dir}/{name}");
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .mode(0o600)
            .open(&path);
    }

    // Sign the binary BEFORE launching via launchd. The daemon runs with
    // MVM_SIGNED=1 so it won't re-exec (which would lose launchd context).
    mvm_providers::apple_container::ensure_signed();

    // The host-backed Nix store is a sparse ext4 file at a stable
    // path. Apple Container attaches it as /dev/vdb; the guest's init
    // mkfs's it once and uses it as overlayfs upper over the rootfs's
    // /nix. Persisted under the data dir (not the cache dir) so
    // `dev down --reset` doesn't wipe it — populated build cache
    // survives image rebuilds, since image staleness and store
    // staleness are independent concerns.
    //
    // The parent process only ensures the parent dir exists; the
    // sparse file itself is created in start_vm if missing.
    let nix_store_disk = nix_store_disk_path();
    if let Some(parent) = std::path::Path::new(&nix_store_disk).parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating host-backed Nix store parent {}", parent.display())
        })?;
    }
    maybe_gc_host_nix_disk(&nix_store_disk);

    ui::info(&format!(
        "Booting dev VM ({} vCPUs, {} GiB memory)...",
        cpus, memory_gib
    ));

    // Install a launchd agent to run the daemon. This is a proper macOS
    // service that is cleanly unloaded by `dev down`.
    install_dev_launchd_agent(&exe, &kernel, &rootfs, cpus, memory_gib, &log_dir)?;

    // Wait for the VM to become ready (vsock proxy socket + guest agent reachable)
    let proxy_path = dev_vsock_proxy_path();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "Dev VM did not start within 60 seconds.\n\
                           Check logs: {log_dir}/daemon-stderr.log"
            );
        }
        if std::path::Path::new(&proxy_path).exists()
            && VsockProxyTransport::new(proxy_path.clone())
                .connect(mvm_guest::vsock::GUEST_AGENT_PORT)
                .is_ok()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    ui::success("Dev VM ready.");
    ui::info("  Shell:      mvmctl dev shell");
    ui::info("  Stop VM:    mvmctl dev down");

    if open_shell {
        ui::info("");
        let _ = console_interactive(DEV_VM_NAME);
    }

    Ok(())
}

/// Path for the vsock proxy Unix socket.
pub(in crate::commands) fn dev_vsock_proxy_path() -> String {
    mvm_providers::apple_container::vsock_proxy_path(DEV_VM_NAME)
        .to_string_lossy()
        .into_owned()
}

/// Daemon mode: boot the VM (which also publishes the vsock proxy socket)
/// and block forever so the in-process VZVirtualMachine stays alive.
fn cmd_dev_apple_container_daemon(cpus: u32, memory_gib: u32) -> Result<()> {
    let kernel = std::env::var("MVM_DEV_KERNEL")
        .unwrap_or_else(|_| format!("{}/dev/vmlinux", mvm_core::config::mvm_cache_dir()));
    let rootfs = std::env::var("MVM_DEV_ROOTFS")
        .unwrap_or_else(|_| format!("{}/dev/rootfs.ext4", mvm_core::config::mvm_cache_dir()));

    let memory_mib = (memory_gib as u64) * 1024;
    mvm_providers::apple_container::start(DEV_VM_NAME, &kernel, &rootfs, cpus, memory_mib)
        .map_err(|e| anyhow::anyhow!("Failed to start dev VM: {e}"))?;

    // Block forever — the VM lives in this process.
    loop {
        std::thread::park();
    }
}

/// Path to the sparse ext4 file that backs the dev VM's Nix store
/// upper layer. Lives outside the cache dir so `dev down --reset`
/// doesn't churn it.
fn nix_store_disk_path() -> String {
    format!("{}/dev/nix-store.img", mvm_core::config::mvm_data_dir())
}

/// Threshold above which `dev up` invokes the in-VM GC before booting.
/// We compare against the sparse file's *materialised* (allocated) size
/// on the host, not its logical size — the file is provisioned at 64
/// GiB but only consumes blocks for actual writes. 20 GiB allocated is
/// comfortably above a typical Rust/Python toolchain closure (~3-6 GiB)
/// and well below the point where the host disk feels strained.
const NIX_STORE_GC_THRESHOLD_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Run `nix-collect-garbage --delete-older-than 14d` *inside* the dev
/// VM when the backing sparse file's allocated size crosses the
/// threshold. Running the GC inside the VM matters: the in-VM nix
/// owns the database and knows the GC roots; running on the host with
/// `NIX_STORE_DIR` pointed at the upper layer would skip locks and
/// could corrupt the store mid-build. Best-effort — failure is logged
/// and the boot proceeds.
fn maybe_gc_host_nix_disk(disk_path: &str) {
    let Ok(meta) = std::fs::metadata(disk_path) else {
        return;
    };
    let allocated = file_allocated_bytes(&meta);
    if allocated < NIX_STORE_GC_THRESHOLD_BYTES {
        return;
    }
    let gib = allocated as f64 / (1024.0 * 1024.0 * 1024.0);
    ui::info(&format!(
        "Host-backed Nix store ({disk_path}) using {gib:.1} GiB; \
         next dev VM boot will run nix-collect-garbage."
    ));
    // Drop a sentinel the daemon's first-build hook can spot. The
    // actual GC runs inside the VM via the dev_build pipeline; we
    // can't run it from the host (would race the in-VM nix daemon
    // and skip locks). The sentinel approach keeps the host side
    // declarative and pushes the work to where it can be done safely.
    let sentinel = format!(
        "{}/dev/nix-store-needs-gc",
        mvm_core::config::mvm_data_dir()
    );
    let _ = std::fs::write(&sentinel, "");
}

/// Allocated (st_blocks * 512) bytes of a file, which for a sparse
/// file is the *materialised* size — much smaller than the logical
/// length until the file gets written into. Falls back to logical
/// length on platforms without st_blocks.
#[cfg(unix)]
fn file_allocated_bytes(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn file_allocated_bytes(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

const DEV_LAUNCHD_LABEL: &str = "com.mvm.dev";

fn dev_launchd_plist_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(format!(
        "{home}/Library/LaunchAgents/{DEV_LAUNCHD_LABEL}.plist"
    ))
}

fn install_dev_launchd_agent(
    exe: &std::path::Path,
    kernel: &str,
    rootfs: &str,
    cpus: u32,
    memory_gib: u32,
    log_dir: &str,
) -> Result<()> {
    // Unload any previous agent first
    unload_dev_launchd_agent();

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{DEV_LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>dev</string>
        <string>up</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>MVM_DEV_DAEMON</key>
        <string>1</string>
        <key>MVM_DEV_KERNEL</key>
        <string>{kernel}</string>
        <key>MVM_DEV_ROOTFS</key>
        <string>{rootfs}</string>
        <key>MVM_DEV_CPUS</key>
        <string>{cpus}</string>
        <key>MVM_DEV_MEM_GIB</key>
        <string>{memory_gib}</string>
        <key>MVM_HOST_WORKDIR</key>
        <string>{host_workdir}</string>
        <key>MVM_HOST_DATADIR</key>
        <string>{host_datadir}</string>
        <key>MVM_NIX_STORE_DISK</key>
        <string>{nix_store_disk}</string>
        <key>MVM_SIGNED</key>
        <string>0</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{log_dir}/daemon-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/daemon-stderr.log</string>
</dict>
</plist>"#,
        exe = exe.display(),
        // Capture the user's CWD here (parent CLI process). The daemon
        // is spawned by launchd with `current_dir() == /`, so it can't
        // recover this on its own — `start_vm()` reads this env var to
        // decide where to bind-mount the virtiofs share inside the VM.
        host_workdir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        // Persistent host-backed Nix store, sparse ext4 file. Lives
        // outside the cache dir for the dev image (which `dev down
        // --reset` blows away) so populated build cache survives image
        // rebuilds. The file is created on first VM start; the guest
        // mkfs's it the first time it sees /dev/vdb.
        nix_store_disk = nix_store_disk_path(),
        // The mvm data dir on the host ($HOME/.mvm/...). The VM
        // mounts it at the same absolute path so paths the dev_build
        // pipeline emits (e.g. ~/.mvm/dev/builds/<hash>/) resolve to
        // the same files on both sides of the VM boundary.
        host_datadir = mvm_core::config::mvm_data_dir(),
    );

    let plist_path = dev_launchd_plist_path();
    let agents_dir = plist_path.parent().expect("plist path must have parent");
    std::fs::create_dir_all(agents_dir)?;
    std::fs::write(&plist_path, &plist)?;

    let output = std::process::Command::new("launchctl")
        .args(["load", plist_path.to_str().unwrap_or("")])
        .output()
        .context("Failed to run launchctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("launchctl load failed: {stderr}");
    }

    Ok(())
}

fn unload_dev_launchd_agent() {
    let plist_path = dev_launchd_plist_path();
    if plist_path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", plist_path.to_str().unwrap_or("")])
            .output();
        let _ = std::fs::remove_file(&plist_path);
    }
}

/// Kill the process that owns the dev VM and clean up its state.
fn stop_dev_vm_owner() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let vm_dir = std::path::PathBuf::from(format!("{home}/.mvm/vms/{DEV_VM_NAME}"));
    let pid_file = vm_dir.join("pid");

    if let Ok(pid_str) = std::fs::read_to_string(&pid_file)
        && let Ok(pid) = pid_str.trim().parse::<i32>()
    {
        // Don't kill ourselves
        if pid as u32 != std::process::id() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            // Wait briefly for it to exit
            for _ in 0..20 {
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    let _ = std::fs::remove_dir_all(&vm_dir);
}

/// Clean up stale persisted state from a dead dev VM process.
fn cleanup_stale_dev_vm() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let vm_dir = std::path::PathBuf::from(format!("{home}/.mvm/vms/{DEV_VM_NAME}"));
    let pid_file = vm_dir.join("pid");

    if !pid_file.exists() {
        return;
    }

    let pid_str = match std::fs::read_to_string(&pid_file) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };
    let pid: i32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => return,
    };

    // Check if the process is still alive (signal 0 = existence check)
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        return; // process still running, not stale
    }

    ui::info("Cleaning up stale dev VM state from a previous session...");
    let _ = std::fs::remove_dir_all(&vm_dir);
}

/// Stop the Apple Container dev VM.
pub(super) fn cmd_dev_apple_container_down() -> Result<()> {
    let was_running = is_apple_container_dev_running() || dev_launchd_plist_path().exists();

    // Unload the launchd agent (stops the daemon process)
    unload_dev_launchd_agent();
    // Kill any lingering daemon process
    stop_dev_vm_owner();
    // Clean up state files
    cleanup_stale_dev_vm();
    let _ = std::fs::remove_file(dev_vsock_proxy_path());

    if was_running {
        ui::success("Dev VM stopped.");
    } else {
        ui::info("Dev VM is not running.");
    }
    Ok(())
}

/// Show Apple Container dev VM status.
pub(super) fn cmd_dev_apple_container_status() -> Result<()> {
    let running = is_apple_container_dev_running();
    ui::info("Backend:  Apple Container (Virtualization.framework)");
    ui::info(&format!("Dev VM:   {DEV_VM_NAME}"));
    ui::info(&format!(
        "Status:   {}",
        if running { "running" } else { "stopped" }
    ));

    if running
        && let Ok(mut stream) = mvm_providers::apple_container::vsock_connect_any(
            DEV_VM_NAME,
            mvm_guest::vsock::GUEST_AGENT_PORT,
        )
        && let Ok(mvm_guest::vsock::GuestResponse::ExecResult { stdout, .. }) =
            mvm_guest::vsock::send_request(
                &mut stream,
                &mvm_guest::vsock::GuestRequest::Exec {
                    command: "uname -r".to_string(),
                    stdin: None,
                    timeout_secs: Some(5),
                },
            )
    {
        ui::info(&format!("  Kernel:  {}", stdout.trim()));
    }

    if let Some(image) = resolve_dev_status_image() {
        ui::info("  Image:   cached");
        if let Some(kernel_path) = image.kernel_path {
            ui::info(&format!("  Kernel:  {kernel_path}"));
        }
        ui::info(&format!("  Rootfs:  {}", image.rootfs_path));
    } else {
        ui::info("  Image:   not built");
    }

    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct DevStatusImage {
    kernel_path: Option<String>,
    rootfs_path: String,
}

fn resolve_dev_status_image() -> Option<DevStatusImage> {
    if let Some(image) = dev_launchd_image_paths()
        && std::path::Path::new(&image.rootfs_path).exists()
    {
        return Some(image);
    }

    let version = env!("CARGO_PKG_VERSION");
    for dir in [
        format!("{}/dev/current", mvm_core::config::mvm_data_dir()),
        format!(
            "{}/dev/prebuilt/v{version}",
            mvm_core::config::mvm_data_dir()
        ),
        format!("{}/dev", mvm_core::config::mvm_cache_dir()),
    ] {
        let rootfs_path = format!("{dir}/rootfs.ext4");
        if !std::path::Path::new(&rootfs_path).exists() {
            continue;
        }
        let kernel_path = format!("{dir}/vmlinux");
        return Some(DevStatusImage {
            kernel_path: std::path::Path::new(&kernel_path)
                .exists()
                .then_some(kernel_path),
            rootfs_path,
        });
    }

    None
}

fn dev_launchd_image_paths() -> Option<DevStatusImage> {
    let plist = std::fs::read_to_string(dev_launchd_plist_path()).ok()?;
    Some(DevStatusImage {
        kernel_path: plist_env_string_value(&plist, "MVM_DEV_KERNEL"),
        rootfs_path: plist_env_string_value(&plist, "MVM_DEV_ROOTFS")?,
    })
}

fn plist_env_string_value(plist: &str, key: &str) -> Option<String> {
    let expected_key = format!("<key>{key}</key>");
    let mut lines = plist.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if line != expected_key {
            continue;
        }
        let value = lines.next()?.trim();
        return value
            .strip_prefix("<string>")?
            .strip_suffix("</string>")
            .map(str::to_string);
    }
    None
}

/// Prepare `~/.mvm/dev/current/` for a fresh dev-image build.
///
/// Replaces a stale symlink (the nix-darwin `linux-builder` legacy
/// pointed `current` at a root-owned `/nix/store/…-mvm-dev` path)
/// with a real, writable directory. `create_dir_all` is a no-op
/// against an existing symlink, so without this the libkrun
/// virtio-fs `/out` mount lands on the read-only Nix store path
/// and Apple Container fails with EACCES.
///
/// `allow(dead_code)`: only reachable under the libkrun-dispatch
/// branch of `ensure_dev_image`, which itself is gated on
/// `backends-builder-vm-libkrun`. Default-features-off builds
/// don't reach this helper.
#[allow(dead_code)]
fn prepare_dev_image_out_dir(out_dir: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(out_dir).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dev-image out parent {}", parent.display()))?;
    }
    if std::path::Path::new(out_dir)
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        std::fs::remove_file(out_dir)
            .with_context(|| format!("removing stale dev-image symlink at {out_dir}"))?;
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating dev-image out dir {out_dir}"))?;
    Ok(())
}

#[cfg(not(feature = "contributor-bootstrap"))]
fn source_checkout_requires_contributor_bootstrap(flake_dir: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Local dev-image flake found at {flake_dir}, but this mvmctl binary was built without \
         the `contributor-bootstrap` feature.\n\n\
         Refusing to download the published prebuilt because `mvmctl dev up` from a source \
         checkout must reflect local flakes and rootfs changes.\n\n\
         Re-run with:\n\
           cargo run --features contributor-bootstrap -- dev up\n\n\
         Or install a contributor binary with:\n\
           cargo install --path . --features contributor-bootstrap"
    )
}

/// Resolve the dev image (kernel + rootfs) to absolute paths.
///
/// In a source checkout: uses the libkrun-backed builder VM
/// (Plan 72 W4/W5 — `LibkrunBuilderVm` runs `nix build` against the
/// dev-shell flake from inside a microVM with a persistent 64 GiB
/// `/nix` store). Libkrun isn't installed → loud error pointing at
/// the install command; **no microsandbox fallback for the dev-shell
/// image** (Plan 72 W5.C — the dev-shell rustc closure overflows
/// microsandbox's 4 GiB overlay anyway, so a fallback that would
/// just disk-out is worse than an actionable error).
///
/// Outside a source checkout: falls back to the GitHub-release
/// download of a pre-built image.
///
/// Failures of the local build are surfaced loudly — never silently
/// substituted with the prebuilt, since the prebuilt would mask local
/// rootfs changes.
fn ensure_dev_image() -> Result<(String, String)> {
    let local_flake = find_dev_image_flake().ok();

    #[cfg(not(feature = "contributor-bootstrap"))]
    if let Some(flake_dir) = &local_flake {
        return Err(source_checkout_requires_contributor_bootstrap(flake_dir));
    }

    // Plan 72 W5.B + W5.C — source-checkout dispatch.
    //
    // libkrun is the only supported builder for the dev-shell flake.
    // Plan 72 W5.C removed the legacy direct-microsandbox fallback
    // because:
    //
    //   1. The dev-shell rustc + cargo closure overflows microsandbox's
    //      hardcoded 4 GiB writable overlay (the load-bearing reason
    //      ADR-046 / Plan 72 exists). A fallback that would fail with
    //      "No space left on device" is worse than a clear install
    //      hint.
    //
    //   2. libkrun is now a documented prerequisite for the source-
    //      checkout dev loop on macOS Apple Silicon / Linux KVM hosts.
    //      `mvmctl doctor` reports its absence; `brew install libkrun`
    //      (macOS) / distro package (Linux) is the install path.
    //
    // microsandbox is still used INSIDE `build_image_via_libkrun` for
    // Stage 0 (building the small Layer 1 builder VM image from the
    // in-repo W2 flake — a closure that fits the 4 GiB overlay). That
    // path is gated by `contributor-bootstrap` and lives in
    // `bootstrap_builder_vm_image`. It's not a runtime fallback.
    //
    // Failures are loud and refuse silent fallback to the prebuilt,
    // since the typical failure mode (libkrun runtime mismatch, builder-
    // vm image cache missing) is a config error that hiding behind the
    // prebuilt would mask.
    // Gate the dispatch itself on `backends-builder-vm-libkrun`. The
    // earlier `contributor-bootstrap` guard is intentionally stricter
    // for source checkouts: contributors must build Layer 1 from the
    // in-repo W2 flake instead of downloading the published builder-VM
    // artifacts.
    #[cfg(feature = "backends-builder-vm-libkrun")]
    if let Some(flake_dir) = &local_flake
        && find_builder_vm_flake().is_ok()
    {
        let out_dir = format!("{}/dev/current", mvm_core::config::mvm_data_dir());
        prepare_dev_image_out_dir(&out_dir)?;

        if !mvm_libkrun::is_available() {
            anyhow::bail!(
                "libkrun is required to build the dev image from source (Plan 72 W5.C).\n\
                 {}\n\n\
                 Once installed, retry `mvmctl dev up`. If you intend to use the published\n\
                 prebuilt instead (no local builds), move or delete\n\
                 nix/images/builder/flake.nix so the source-checkout heuristic stops matching.",
                mvm_libkrun::install_hint(),
            );
        }

        ui::info(&format!(
            "Building dev image via libkrun builder VM (Plan 72 W5) from: {flake_dir}"
        ));
        match build_image_via_libkrun(&out_dir) {
            Ok((kernel, rootfs)) => {
                ui::success(&format!("Dev image ready at {out_dir}."));
                return Ok((kernel, rootfs));
            }
            Err(e) => {
                anyhow::bail!(
                    "libkrun builder VM build failed (source checkout: {flake_dir}).\n{e:#}\n\n\
                     Refusing to fall back to the published prebuilt because it would mask\n\
                     local rootfs changes. To force the prebuilt anyway, move or delete\n\
                     nix/images/builder/flake.nix so the source-checkout heuristic stops matching."
                );
            }
        }
    }

    // No local source checkout — download the published prebuilt.
    // Cache key = mvmctl's version: each version owns a sibling
    // directory under .../dev/prebuilt/, and bumping the binary
    // automatically invalidates older caches. We sweep older version
    // dirs on every miss so disk usage tracks the *current* version,
    // not the union of every version ever installed.
    ui::info("No local dev-image flake found; downloading published prebuilt.");
    let version = env!("CARGO_PKG_VERSION");
    let prebuilt_root = format!("{}/dev/prebuilt", mvm_core::config::mvm_data_dir());
    let prebuilt_dir = format!("{prebuilt_root}/v{version}");
    std::fs::create_dir_all(&prebuilt_dir)
        .with_context(|| format!("creating prebuilt dir {prebuilt_dir}"))?;
    let kernel_path = format!("{prebuilt_dir}/vmlinux");
    let rootfs_path = format!("{prebuilt_dir}/rootfs.ext4");
    // Cache hit on the current version's dir — fast path. Validate
    // first; if either file is below the size floor or the rootfs
    // lacks the ext4 magic, treat the cache as poisoned and delete it
    // so the cascade below can re-populate from a healthy source. The
    // typical poisoning vector is an earlier copy from a stub or
    // half-downloaded source — the size floor catches the stub case
    // (~12 B vs. ~16 MiB minimum), and the magic check catches a
    // wrong-format file at the right size.
    if std::path::Path::new(&kernel_path).exists() && std::path::Path::new(&rootfs_path).exists() {
        match validate_dev_image_artifacts(&kernel_path, &rootfs_path) {
            Ok(()) => {
                prune_old_prebuilts(&prebuilt_root, version);
                return Ok((kernel_path, rootfs_path));
            }
            Err(e) => {
                ui::warn(&format!(
                    "Cached dev image at {prebuilt_dir} failed sanity check ({e}); \
                     deleting and rebuilding."
                ));
                let _ = std::fs::remove_file(&kernel_path);
                let _ = std::fs::remove_file(&rootfs_path);
            }
        }
    }
    // Source-checkout-first. When the binary was compiled out of an
    // mvm source tree that has `nix/images/dev-prebuilt/<arch>/`
    // populated, that's the authoritative dev image for this build —
    // skip GitHub entirely. The helper returns `None` for installed
    // binaries (their `CARGO_MANIFEST_DIR` resolves into
    // `~/.cargo/registry/` where the directory will never exist) and
    // for source checkouts that haven't vendored anything yet, in
    // which case we fall through to the published prebuilt as before.
    if let Some((src_kernel, src_rootfs, source_label)) = find_vendored_dev_image() {
        validate_dev_image_artifacts(&src_kernel, &src_rootfs).with_context(|| {
            format!(
                "vendored dev image at {source_label} failed sanity check — \
                 refusing to copy garbage into the prebuilt cache"
            )
        })?;
        ui::info(&format!(
            "Using vendored dev image from source checkout ({source_label})."
        ));
        std::fs::copy(&src_kernel, &kernel_path)
            .with_context(|| format!("copying vendored kernel {src_kernel:?} → {kernel_path}"))?;
        std::fs::copy(&src_rootfs, &rootfs_path)
            .with_context(|| format!("copying vendored rootfs {src_rootfs:?} → {rootfs_path}"))?;
        // No prune — vendored is the source of truth for this binary,
        // not a download; leaving older `v*/` dirs around lets
        // installed-binary users keep their offline-fallback cache.
        return Ok((kernel_path, rootfs_path));
    }
    // Try the published prebuilt. Defer the prune until *after* a
    // successful download — old `~/.mvm/dev/prebuilt/v*/` dirs and
    // historical `~/.mvm/dev/builds/<hash>/` artifacts are our last-
    // resort fallback when the release page lacks v{version} assets.
    match download_dev_image(&kernel_path, &rootfs_path) {
        Ok(result) => {
            prune_old_prebuilts(&prebuilt_root, version);
            Ok(result)
        }
        Err(download_err) => {
            ui::warn(&format!(
                "Could not download dev image for v{version}: {download_err}\n\
                 Searching for a local fallback under ~/.mvm/dev/."
            ));
            if let Some((src_kernel, src_rootfs, source_label)) = find_local_fallback_image() {
                ui::warn(&format!(
                    "Using local fallback from {source_label}. \
                     This is not the published v{version} image — boot it knowing the \
                     versions differ. Publish v{version} assets or restore the local \
                     builder flake to make this go away."
                ));
                std::fs::copy(&src_kernel, &kernel_path).with_context(|| {
                    format!("copying fallback kernel {src_kernel:?} → {kernel_path}")
                })?;
                std::fs::copy(&src_rootfs, &rootfs_path).with_context(|| {
                    format!("copying fallback rootfs {src_rootfs:?} → {rootfs_path}")
                })?;
                Ok((kernel_path, rootfs_path))
            } else {
                Err(download_err.context(
                    "no local fallback found under ~/.mvm/dev/prebuilt/v*/ \
                     or ~/.mvm/dev/builds/*/",
                ))
            }
        }
    }
}

/// Search for any locally-cached dev image as a fallback when the
/// published-prebuilt download fails. Looks under:
///
/// - `~/.mvm/dev/prebuilt/v*/{vmlinux,rootfs.ext4}` — previously
///   downloaded prebuilts for earlier versions.
/// - `~/.mvm/dev/builds/<hash>/{vmlinux,rootfs.ext4}` — historical
///   nix-darwin `linux-builder` outputs from the pre-microsandbox era.
///
/// Returns the most-recently-modified pair so a user with a recent
/// successful build/download keeps booting, with a short label
/// (e.g. `v0.13.0` or `builds/abcdef…`) for the warning surface.
/// `None` means nothing usable was found.
fn find_local_fallback_image() -> Option<(std::path::PathBuf, std::path::PathBuf, String)> {
    let dev_root = format!("{}/dev", mvm_core::config::mvm_data_dir());

    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf, String)> = Vec::new();
    for sub in ["prebuilt", "builds"] {
        let parent = std::path::Path::new(&dev_root).join(sub);
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let kernel = dir.join("vmlinux");
            let rootfs = dir.join("rootfs.ext4");
            if !kernel.is_file() || !rootfs.is_file() {
                continue;
            }
            // Silently skip cache entries that look corrupt (stub
            // bytes from a botched earlier copy, half-written
            // downloads, etc.). The auto-discover path is best-effort
            // — surfacing every bad candidate as a warning would
            // spam the boot path; the cascade just falls through to
            // a healthier candidate or to the next layer.
            if validate_dev_image_artifacts(&kernel, &rootfs).is_err() {
                continue;
            }
            let mtime = std::fs::metadata(&rootfs)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            let label = format!("{sub}/{}", entry.file_name().to_string_lossy());
            candidates.push((mtime, dir, label));
        }
    }

    candidates.sort_by_key(|(mtime, ..)| *mtime);
    let (_, dir, label) = candidates.into_iter().next_back()?;
    Some((dir.join("vmlinux"), dir.join("rootfs.ext4"), label))
}

/// Sanity-check that a `(vmlinux, rootfs.ext4)` pair looks like a real
/// dev image. Fast-fails before copying or returning the artifacts as
/// usable, so a stub or truncated file can't poison the prebuilt cache.
///
/// Two checks per file:
///
/// - **Size floor.** A real `vmlinux` is several MiB (typical ARM64
///   Image is 15–20 MiB); a real `rootfs.ext4` is ~700 MiB. Reject
///   anything under a conservative floor (1 MiB / 4 MiB respectively)
///   to catch the stub-file case (~12 B from a botched test, ~0 B
///   from a torn-down download).
/// - **Ext4 magic.** The ext4 superblock starts at byte 1024; the
///   `s_magic` field is at byte 1080 (offset 56 inside the
///   superblock) and equals `0xEF53` little-endian. Only the rootfs
///   gets this check — `vmlinux` formats vary by arch (ARM64
///   `Image`, x86 bzImage, etc.) so there's no portable magic to
///   match.
fn validate_dev_image_artifacts(
    kernel: impl AsRef<std::path::Path>,
    rootfs: impl AsRef<std::path::Path>,
) -> Result<()> {
    const KERNEL_MIN_BYTES: u64 = 1024 * 1024;
    const ROOTFS_MIN_BYTES: u64 = 4 * 1024 * 1024;
    const EXT4_MAGIC_OFFSET: u64 = 1024 + 56;
    const EXT4_MAGIC: [u8; 2] = [0x53, 0xEF];

    let kernel = kernel.as_ref();
    let rootfs = rootfs.as_ref();

    let kernel_size = std::fs::metadata(kernel)
        .with_context(|| format!("stat {}", kernel.display()))?
        .len();
    if kernel_size < KERNEL_MIN_BYTES {
        anyhow::bail!(
            "kernel at {} is only {} bytes (expected ≥ {})",
            kernel.display(),
            kernel_size,
            KERNEL_MIN_BYTES,
        );
    }

    let rootfs_size = std::fs::metadata(rootfs)
        .with_context(|| format!("stat {}", rootfs.display()))?
        .len();
    if rootfs_size < ROOTFS_MIN_BYTES {
        anyhow::bail!(
            "rootfs at {} is only {} bytes (expected ≥ {})",
            rootfs.display(),
            rootfs_size,
            ROOTFS_MIN_BYTES,
        );
    }

    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(rootfs).with_context(|| format!("open {}", rootfs.display()))?;
    f.seek(SeekFrom::Start(EXT4_MAGIC_OFFSET))
        .with_context(|| format!("seek to ext4 magic in {}", rootfs.display()))?;
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic)
        .with_context(|| format!("read ext4 magic from {}", rootfs.display()))?;
    if magic != EXT4_MAGIC {
        anyhow::bail!(
            "rootfs at {} does not have ext4 magic at offset {} (got {magic:02x?})",
            rootfs.display(),
            EXT4_MAGIC_OFFSET,
        );
    }

    Ok(())
}

/// Look for a vendored dev image inside the source checkout the mvmctl
/// binary was compiled from: `{workspace_root}/nix/images/dev-prebuilt/
/// <arch>/{vmlinux, rootfs.ext4}`. The path is checked last in the
/// fallback cascade — it's the most predictable source ("what the
/// repo ships") but only useful when `mvmctl` runs out of its source
/// checkout: `CARGO_MANIFEST_DIR` is baked at compile time and points
/// into `~/.cargo/registry/` for `cargo install`-ed builds, where the
/// directory will reliably be missing. That's fine — for installed
/// binaries the `~/.mvm/dev/` auto-discover path covers the offline
/// case.
///
/// `arch` mirrors the matrix used by `download_dev_image`: `aarch64`
/// on Apple Silicon / aarch64-linux, `x86_64` everywhere else.
fn find_vendored_dev_image() -> Option<(std::path::PathBuf, std::path::PathBuf, String)> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir).parent()?.parent()?;
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let dir = workspace_root
        .join("nix")
        .join("images")
        .join("dev-prebuilt")
        .join(arch);
    let kernel = dir.join("vmlinux");
    let rootfs = dir.join("rootfs.ext4");
    if !kernel.is_file() || !rootfs.is_file() {
        return None;
    }
    let label = format!("vendored {}", dir.display());
    Some((kernel, rootfs, label))
}

/// Drop every direct child of `prebuilt_root` except the one for the
/// currently-running version. Best-effort — failure is logged.
fn prune_old_prebuilts(prebuilt_root: &str, current_version: &str) {
    let current = format!("v{current_version}");
    let Ok(entries) = std::fs::read_dir(prebuilt_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == current {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => ui::info(&format!("Pruned stale prebuilt cache: {name_str}")),
            Err(e) => tracing::warn!("Could not prune {}: {e}", path.display()),
        }
    }
}

/// Download a pre-built dev image (kernel + rootfs) from GitHub releases.
///
/// Plan 36 / ADR 005 trust chain:
///
/// 1. Try the cosign-keyless-signed manifest first
///    (`dev-image-{arch}.manifest.json` + `.bundle`). If present,
///    `mvm-security::image_verify::verify_manifest` validates the
///    Sigstore bundle against the project's release-workflow OIDC
///    identity, parses the manifest, and we use *its* artifact
///    digests as the source of truth.
///
/// 2. If the manifest is 404 (older release predating plan 36) or
///    its companion bundle is missing, fall back to the W5.1
///    unsigned-checksum path with a loud deprecation warning. This
///    keeps mvmctl pointing at older releases working through the
///    rollout, and the deprecation banner sets the stage for making
///    the manifest mandatory in a future major version.
///
/// 3. Either way, every downloaded artifact gets streaming SHA-256
///    verification (W5.1) against the expected digest.
///
/// Escape hatches (both print loud warnings):
///   - `MVM_SKIP_HASH_VERIFY=1` — skip SHA-256 step (existing W5.1).
///   - `MVM_SKIP_COSIGN_VERIFY=1` — skip cosign signature check on
///     the manifest body but still parse and use it. Only for
///     emergency Sigstore-side rotation; SHA-256 still applies.
fn download_dev_image(kernel_path: &str, rootfs_path: &str) -> Result<(String, String)> {
    // Wrap the verification pipeline so every exit path — success or
    // failure — emits the verify_duration gauge and bumps the
    // appropriate outcome counter. Plan 36 §Layer 4 step 11.
    let verify_start = std::time::Instant::now();
    let result = download_dev_image_inner(kernel_path, rootfs_path);
    let elapsed_ms = verify_start.elapsed().as_millis() as u64;
    let metrics = mvm_core::observability::metrics::global();
    metrics
        .dev_image_verify_duration_ms
        .store(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
    if result.is_ok() {
        metrics
            .dev_image_verify_ok
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    result
}

fn download_dev_image_inner(kernel_path: &str, rootfs_path: &str) -> Result<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    let base_url = format!("https://github.com/tinylabscom/mvm/releases/download/v{version}");
    // Detect host arch to download the right image.
    // Apple Silicon (aarch64-darwin) needs aarch64-linux image.
    // Intel Mac (x86_64-darwin) needs x86_64-linux image.
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let kernel_name = format!("dev-vmlinux-{arch}");
    let rootfs_name = format!("dev-rootfs-{arch}.ext4");
    let kernel_url = format!("{base_url}/{kernel_name}");
    let rootfs_url = format!("{base_url}/{rootfs_name}");

    ui::info(&format!("Downloading dev image (v{version})..."));

    // Plan 36 PR-C.2: prefer the cosign-signed manifest. Falls back
    // to the W5.1 unsigned checksum file when the manifest is 404
    // (older release).
    let expected = match try_fetch_signed_manifest(&base_url, version, arch, "dev")? {
        Some(manifest) => {
            ui::success(&format!(
                "  ✓ cosign-verified manifest for v{} (built {} UTC, valid until {} UTC)",
                manifest.version,
                manifest.built_at.format("%Y-%m-%d"),
                manifest.not_after.format("%Y-%m-%d"),
            ));
            manifest
                .artifacts
                .iter()
                .map(|a| (a.name.clone(), a.sha256.to_ascii_lowercase()))
                .collect::<std::collections::HashMap<_, _>>()
        }
        None => {
            ui::warn(&format!(
                "No cosign-signed manifest found for v{version}. Falling back to \
                 unsigned checksum file (legacy path predating plan 36 / ADR 005). \
                 Future releases will require the signed manifest."
            ));
            let checksums_name = format!("dev-image-{arch}-checksums-sha256.txt");
            let checksums_url = format!("{base_url}/{checksums_name}");
            fetch_expected_hashes(&checksums_url, &[&kernel_name, &rootfs_name])?
        }
    };

    ui::info("  Fetching kernel...");
    download_file(&kernel_url, kernel_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!("Failed to download kernel from {kernel_url}"))
    })?;
    verify_artifact_hash(
        kernel_path,
        &kernel_name,
        expected.get(kernel_name.as_str()),
    )?;

    ui::info("  Fetching rootfs...");
    download_file(&rootfs_url, rootfs_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!("Failed to download rootfs from {rootfs_url}"))
    })?;
    verify_artifact_hash(
        rootfs_path,
        &rootfs_name,
        expected.get(rootfs_name.as_str()),
    )?;

    ui::success("Dev image downloaded, hash-verified, and cached.");
    Ok((kernel_path.to_string(), rootfs_path.to_string()))
}

/// Probe for and verify the cosign-signed manifest at
/// `{base_url}/{variant}-image-{arch}.manifest.json{,.bundle}`.
///
/// Returns:
/// - `Ok(Some(manifest))` — manifest + bundle present, signature verified,
///   version pinned to runtime, max-age window not yet exceeded.
/// - `Ok(None)` — manifest URL 404. This is the legacy fallback for
///   older releases that predate plan 36; caller can fall back to the
///   W5.1 unsigned-checksum path with a deprecation warning.
/// - `Err(_)` — manifest fetched but verification or parsing failed.
///   Hard error; never silently fall through. `MVM_SKIP_COSIGN_VERIFY=1`
///   downgrades signature failures to a parse-only path.
fn try_fetch_signed_manifest(
    base_url: &str,
    version: &str,
    arch: &str,
    variant: &str,
) -> Result<Option<mvm_security::image_verify::SignedManifest>> {
    use mvm_security::image_verify;

    let manifest_name = format!("{variant}-image-{arch}.manifest.json");
    let manifest_url = format!("{base_url}/{manifest_name}");
    let bundle_url = format!("{manifest_url}.bundle");

    // HEAD-probe the manifest URL. If absent (older release without
    // plan-36 signing), fall back gracefully.
    if !url_exists(&manifest_url)? {
        return Ok(None);
    }

    let manifest_tmp = tempfile::NamedTempFile::new().context("creating manifest tempfile")?;
    let bundle_tmp = tempfile::NamedTempFile::new().context("creating bundle tempfile")?;
    let manifest_path = manifest_tmp.path().to_string_lossy().into_owned();
    let bundle_path = bundle_tmp.path().to_string_lossy().into_owned();

    download_file(&manifest_url, &manifest_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!(
            "Failed to download signed manifest from {manifest_url}"
        ))
    })?;
    download_file(&bundle_url, &bundle_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!(
            "Failed to download cosign bundle from {bundle_url}. Plan 36 \
             requires a manifest's signature to be present alongside the \
             manifest body — refusing to trust an unsigned manifest."
        ))
    })?;

    let manifest_bytes =
        std::fs::read(&manifest_path).context("reading downloaded manifest body")?;
    let bundle_bytes = std::fs::read(&bundle_path).context("reading downloaded cosign bundle")?;

    // GitHub Actions keyless OIDC: the SAN encodes the workflow URL
    // bound to the tag, and the issuer is GitHub's token endpoint.
    let expected_identity = format!(
        "https://github.com/tinylabscom/mvm/.github/workflows/release.yml@refs/tags/v{version}"
    );
    let expected_issuer = "https://token.actions.githubusercontent.com";

    let manifest = if std::env::var_os("MVM_SKIP_COSIGN_VERIFY").is_some() {
        tracing::warn!(
            "MVM_SKIP_COSIGN_VERIFY set — accepting unverified manifest body. \
             Plan 36 documents this as an emergency-rotation escape hatch only."
        );
        image_verify::parse_manifest(&manifest_bytes)
            .map_err(|e| anyhow::anyhow!("manifest parse failed: {e}"))?
    } else {
        image_verify::verify_manifest(
            &manifest_bytes,
            &bundle_bytes,
            &expected_identity,
            expected_issuer,
        )
        .map_err(|e| {
            bump_verify_outcome("sig_invalid");
            anyhow::anyhow!(
                "Cosign verification failed for {manifest_name}: {e}\n\
                 \n\
                 Plan 36 / ADR 005 requires every dev image manifest to be cosign-keyless\n\
                 signed against the release workflow's OIDC identity. Refusing to use this\n\
                 image. Possible causes:\n\
                 - account/CDN compromise (open a security issue);\n\
                 - the release was published without going through the signing job;\n\
                 - clock skew (manifest expired); check `date -u`.\n\
                 \n\
                 Emergency rotation: set MVM_SKIP_COSIGN_VERIFY=1 to bypass the signature\n\
                 check while keeping SHA-256 verification active."
            )
        })?
    };

    // Pin the manifest's claimed version to mvmctl's own version. A
    // mismatch means someone is feeding us a different release's
    // manifest — refuse.
    image_verify::check_version_pin(&manifest, version).map_err(|e| {
        bump_verify_outcome("version_skew");
        anyhow::anyhow!("manifest version pin failed: {e}")
    })?;

    // Enforce max-age (default 90d). mvmctl warns and proceeds; mvmd
    // refuses (different risk tolerance — handled in mvmd plan 23).
    let now = chrono::Utc::now();
    if let Err(e) = image_verify::check_not_after(&manifest, now) {
        bump_verify_outcome("expired");
        ui::warn(&format!(
            "Dev image manifest is past its max-age ({e}). Consider upgrading \
             mvmctl — older signed images are still cryptographically valid but \
             may carry unpatched vulnerabilities."
        ));
    }

    // Plan 36 §Layer 4 step 4: consult the cosign-signed revocation
    // list. Cached up to 24h; tolerated up to 7d offline. A signed
    // image whose version is on the list hard-fails — recall is the
    // primary mechanism for "we know this build is bad."
    if let Some(revocations) = try_fetch_revocation_list()? {
        image_verify::check_revocation(&manifest, &revocations).map_err(|e| {
            bump_verify_outcome("revoked");
            anyhow::anyhow!(
                "Dev image manifest is on the project's revocation list: {e}\n\
                 \n\
                 Plan 36 / ADR 005: a published `revocations` release entry has\n\
                 marked v{version} unsafe to run. Refusing to use this image.\n\
                 Upgrade mvmctl to a non-revoked release."
            )
        })?;
    }

    Ok(Some(manifest))
}

/// Fetch + verify the project's signed revocation list, caching it
/// under `~/.cache/mvm/revocations/`.
///
/// Plan 36 §Layer 4 step 4. The revocation list lives at a stable
/// `revocations` release tag whose only assets are
/// `revoked-versions.json` and its cosign bundle. Append-only across
/// releases; updated by cutting a new entry on that tag.
///
/// Cache policy:
///   - Refresh from upstream if the cached file is >24h old.
///   - Tolerate up to 7d of cached staleness when the network is
///     unavailable; surface a warning rather than blocking.
///   - 404 on the upstream URL is treated as "no recalls today" —
///     bootstrap state until the project publishes its first
///     revocations entry. Returns Ok(None).
///
/// Returns Ok(None) when the list isn't available *and* we have no
/// cached copy — caller proceeds without revocation enforcement (with
/// a warning). Returns Err on signature verification failure.
fn try_fetch_revocation_list() -> Result<Option<mvm_security::image_verify::RevocationList>> {
    use mvm_security::image_verify;
    use std::time::{Duration, SystemTime};

    let cache_dir = format!("{}/revocations", mvm_core::config::mvm_cache_dir());
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating revocations cache dir {cache_dir}"))?;
    let cache_json = format!("{cache_dir}/revoked-versions.json");
    let cache_bundle = format!("{cache_dir}/revoked-versions.json.bundle");

    let cache_age = std::fs::metadata(&cache_json)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or(Duration::from_secs(u64::MAX));

    let twenty_four_hours = Duration::from_secs(24 * 60 * 60);
    let seven_days = Duration::from_secs(7 * 24 * 60 * 60);

    // Refresh if cache is stale (or absent).
    if cache_age > twenty_four_hours {
        let base = "https://github.com/tinylabscom/mvm/releases/download/revocations";
        let json_url = format!("{base}/revoked-versions.json");
        let bundle_url = format!("{base}/revoked-versions.json.bundle");

        match url_exists(&json_url) {
            Ok(true) => {
                let tmp_json =
                    tempfile::NamedTempFile::new().context("creating revocations tempfile")?;
                let tmp_bundle = tempfile::NamedTempFile::new()
                    .context("creating revocations bundle tempfile")?;
                let tmp_json_path = tmp_json.path().to_string_lossy().into_owned();
                let tmp_bundle_path = tmp_bundle.path().to_string_lossy().into_owned();
                let download_result = download_file(&json_url, &tmp_json_path)
                    .and_then(|()| download_file(&bundle_url, &tmp_bundle_path));
                match download_result {
                    Ok(()) => {
                        std::fs::copy(&tmp_json_path, &cache_json)
                            .context("caching revoked-versions.json")?;
                        std::fs::copy(&tmp_bundle_path, &cache_bundle)
                            .context("caching revoked-versions.json.bundle")?;
                    }
                    Err(e) if cache_age <= seven_days => {
                        ui::warn(&format!(
                            "Could not refresh revocation list ({e}); using cached copy \
                             (last refreshed {} hours ago).",
                            cache_age.as_secs() / 3600
                        ));
                    }
                    Err(e) => {
                        ui::warn(&format!(
                            "Could not refresh revocation list ({e}) and no fresh cache \
                             is available; proceeding without recall enforcement. \
                             Plan 36 §Layer 4."
                        ));
                        return Ok(None);
                    }
                }
            }
            Ok(false) => {
                // 404: the project hasn't published a revocations
                // release yet. Bootstrap state — no recalls means
                // nothing to enforce. Don't cache this; a future
                // refresh should pick up the first published list.
                return Ok(None);
            }
            Err(e) if cache_age <= seven_days => {
                ui::warn(&format!(
                    "Could not probe revocation list ({e}); using cached copy."
                ));
            }
            Err(e) => {
                ui::warn(&format!(
                    "Could not probe revocation list ({e}) and no fresh cache \
                     is available; proceeding without recall enforcement."
                ));
                return Ok(None);
            }
        }
    }

    // No cached file → nothing to enforce.
    if !std::path::Path::new(&cache_json).exists() {
        return Ok(None);
    }

    let json_bytes = std::fs::read(&cache_json).context("reading cached revocations.json")?;
    let bundle_bytes =
        std::fs::read(&cache_bundle).context("reading cached revocations.json.bundle")?;

    // The revocations tag is signed by a dedicated revocations
    // workflow's OIDC identity, not the per-release workflow. A
    // separate identity ensures a leaked image-signing cert can't
    // fabricate a permissive revocation list (and vice versa).
    let expected_identity = "https://github.com/tinylabscom/mvm/.github/workflows/revocations.yml@refs/tags/revocations";
    let expected_issuer = "https://token.actions.githubusercontent.com";

    if std::env::var_os("MVM_SKIP_COSIGN_VERIFY").is_some() {
        // The same MVM_SKIP_COSIGN_VERIFY emergency-rotation escape
        // hatch covers both the manifest and the revocation list.
        // SHA-256 of artifacts still applies separately at the
        // verify_artifact_hash callsite.
        let list: image_verify::RevocationList = serde_json::from_slice(&json_bytes)
            .context("parsing revocations JSON without signature verification")?;
        return Ok(Some(list));
    }

    image_verify::verify_signed_payload(
        &json_bytes,
        &bundle_bytes,
        expected_identity,
        expected_issuer,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "Revocation list signature verification failed: {e}. Refusing to \
             trust an unverified recall. Plan 36 §Layer 4."
        )
    })?;
    let list: image_verify::RevocationList =
        serde_json::from_slice(&json_bytes).context("parsing verified revocations JSON")?;
    Ok(Some(list))
}

/// `mvmctl dev import-image` — sideload a verified dev image from local files.
///
/// Plan 36 PR-D.2 / §"Air-gapped install path". Runs the same
/// cosign + SHA-256 + version-pin + max-age + revocation pipeline
/// as `download_dev_image`, but against operator-provided local
/// files instead of the GitHub Releases URL. On success the verified
/// artifacts are copied into the version-namespaced cache so the next
/// `mvmctl dev up` boots from them with no further verification or
/// network round-trip.
///
/// The intended user is anyone running mvmctl in a regulated /
/// gov / air-gapped environment that can't reach github.com but
/// that legitimately wants the supply-chain check. Without this
/// path the only option for these users was MVM_SKIP_HASH_VERIFY=1,
/// which disables verification entirely — exactly the unsafe escape
/// plan 36 exists to discourage.
pub fn cmd_dev_import_image(
    manifest_path: &str,
    bundle_path: &str,
    vmlinux_path: &str,
    rootfs_path: &str,
) -> Result<()> {
    use mvm_security::image_verify;

    let version = env!("CARGO_PKG_VERSION");
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };

    ui::info(&format!(
        "Importing dev image (v{version}, {arch}) from local files..."
    ));

    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("reading manifest file at {manifest_path}"))?;
    let bundle_bytes = std::fs::read(bundle_path)
        .with_context(|| format!("reading cosign bundle at {bundle_path}"))?;

    let expected_identity = format!(
        "https://github.com/tinylabscom/mvm/.github/workflows/release.yml@refs/tags/v{version}"
    );
    let expected_issuer = "https://token.actions.githubusercontent.com";

    let manifest = if std::env::var_os("MVM_SKIP_COSIGN_VERIFY").is_some() {
        ui::warn(
            "MVM_SKIP_COSIGN_VERIFY set — accepting unverified manifest. \
             Plan 36 documents this as an emergency-rotation escape only.",
        );
        image_verify::parse_manifest(&manifest_bytes)
            .map_err(|e| anyhow::anyhow!("manifest parse failed: {e}"))?
    } else {
        image_verify::verify_manifest(
            &manifest_bytes,
            &bundle_bytes,
            &expected_identity,
            expected_issuer,
        )
        .map_err(|e| {
            bump_verify_outcome("sig_invalid");
            anyhow::anyhow!(
                "Cosign verification failed for the imported manifest: {e}\n\
                 \n\
                 Plan 36 / ADR 005: a sideloaded manifest must carry the\n\
                 same release-workflow OIDC signature as the network path.\n\
                 \n\
                 Common causes:\n\
                 - mismatched manifest + bundle pair (re-export both as a set);\n\
                 - manifest belongs to a different mvmctl version (check `mvmctl --version`);\n\
                 - clock skew (signature time-window).\n\
                 \n\
                 Emergency rotation: MVM_SKIP_COSIGN_VERIFY=1 keeps SHA-256\n\
                 verification active while bypassing the signature step."
            )
        })?
    };

    image_verify::check_version_pin(&manifest, version).map_err(|e| {
        bump_verify_outcome("version_skew");
        anyhow::anyhow!(
            "Imported manifest is for a different mvmctl version: {e}\n\
             \n\
             Plan 36 pins manifest.version == mvmctl version exactly. Re-export\n\
             the manifest from a release matching v{version}, or upgrade mvmctl."
        )
    })?;

    let now = chrono::Utc::now();
    if let Err(e) = image_verify::check_not_after(&manifest, now) {
        bump_verify_outcome("expired");
        ui::warn(&format!(
            "Imported manifest is past its max-age ({e}). Sideloaded images \
             from older releases remain cryptographically valid but may \
             carry unpatched vulnerabilities."
        ));
    }

    if let Some(revocations) = try_fetch_revocation_list()? {
        image_verify::check_revocation(&manifest, &revocations).map_err(|e| {
            bump_verify_outcome("revoked");
            anyhow::anyhow!(
                "Imported manifest is on the project's revocation list: {e}\n\
                 \n\
                 Plan 36: a `revocations` release entry has marked v{version} \
                 unsafe to run. Refusing to import."
            )
        })?;
    }

    if manifest.arch != arch {
        anyhow::bail!(
            "Manifest is for arch {} but this host is {arch}. Wrong-arch image \
             would not boot. Re-export the manifest for the correct arch.",
            manifest.arch
        );
    }

    let kernel_name = format!("dev-vmlinux-{arch}");
    let rootfs_name = format!("dev-rootfs-{arch}.{}", manifest.rootfs_format);

    let kernel_digest = manifest
        .artifact(&kernel_name)
        .ok_or_else(|| anyhow::anyhow!("manifest does not list {kernel_name}"))?;
    let rootfs_digest = manifest
        .artifact(&rootfs_name)
        .ok_or_else(|| anyhow::anyhow!("manifest does not list {rootfs_name}"))?;

    image_verify::verify_artifact(std::path::Path::new(vmlinux_path), kernel_digest).map_err(
        |e| {
            bump_verify_outcome("digest_mismatch");
            anyhow::anyhow!("kernel SHA-256 mismatch: {e}")
        },
    )?;
    image_verify::verify_artifact(std::path::Path::new(rootfs_path), rootfs_digest).map_err(
        |e| {
            bump_verify_outcome("digest_mismatch");
            anyhow::anyhow!("rootfs SHA-256 mismatch: {e}")
        },
    )?;

    // Copy the verified artifacts into the version-namespaced cache.
    // The next `mvmctl dev up` picks them up without re-running
    // verification (the cache hit precedes download_dev_image).
    let prebuilt_dir = format!(
        "{}/dev/prebuilt/v{version}",
        mvm_core::config::mvm_data_dir()
    );
    std::fs::create_dir_all(&prebuilt_dir)
        .with_context(|| format!("creating prebuilt dir {prebuilt_dir}"))?;
    let target_kernel = format!("{prebuilt_dir}/vmlinux");
    let target_rootfs = format!("{prebuilt_dir}/rootfs.ext4");
    std::fs::copy(vmlinux_path, &target_kernel)
        .with_context(|| format!("copying kernel to {target_kernel}"))?;
    std::fs::copy(rootfs_path, &target_rootfs)
        .with_context(|| format!("copying rootfs to {target_rootfs}"))?;

    mvm_core::observability::metrics::global()
        .dev_image_verify_ok
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    ui::success(&format!(
        "Imported and verified dev image v{version} into {prebuilt_dir}. \
         Run `mvmctl dev up` to boot the dev VM from the cached artifacts."
    ));
    Ok(())
}

/// Bump the dev_image_verify_<outcome> counter. Plan 36 §Layer 4 step 11.
///
/// Caller passes the outcome name; centralising the lookup keeps the
/// counter set discoverable in one place. mvmd plan 23's
/// reconciliation loop will alert on attack-shaped spikes
/// (sig_invalid, digest_mismatch, revoked).
///
/// Security-relevant outcomes (everything except `network`, which is
/// operational) also emit a `LocalAuditKind::ImageVerifyFailed` event
/// so `mvmctl audit tail` shows the rejection. The counter is the
/// alerting channel; the audit line is the forensics channel.
fn bump_verify_outcome(outcome: &str) {
    let m = mvm_core::observability::metrics::global();
    let counter = match outcome {
        "sig_invalid" => &m.dev_image_verify_sig_invalid,
        "digest_mismatch" => &m.dev_image_verify_digest_mismatch,
        "version_skew" => &m.dev_image_verify_version_skew,
        "revoked" => &m.dev_image_verify_revoked,
        "expired" => &m.dev_image_verify_expired,
        "network" => &m.dev_image_verify_network,
        // Defensive: an unknown outcome is itself a bug worth surfacing
        // — log a warning rather than silently swallowing the metric.
        _ => {
            tracing::warn!("bump_verify_outcome: unknown outcome '{outcome}'");
            return;
        }
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if outcome != "network" {
        let mvmctl_version = env!("CARGO_PKG_VERSION");
        mvm_core::audit_emit!(
            ImageVerifyFailed,
            "outcome={outcome} mvmctl_version={mvmctl_version}"
        );
    }
}

/// HEAD-probe a URL. Returns Ok(true) when the resource is reachable
/// (HTTP 2xx), Ok(false) on 404, Err for transient failures.
fn url_exists(url: &str) -> Result<bool> {
    let output = std::process::Command::new("curl")
        .args(["-fSI", "-o", "/dev/null", "-w", "%{http_code}", url])
        .output()
        .context("Failed to run curl HEAD probe")?;
    let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match code.as_str() {
        "200" | "302" => Ok(true),
        "404" => Ok(false),
        _ => {
            // Other status (5xx, network error, redirect chain failure)
            // — don't silently fall through to the unsigned path.
            anyhow::bail!(
                "HEAD probe of {url} returned status {code}; refusing to guess \
                 whether the signed manifest is missing or transiently unavailable. \
                 Retry, or investigate."
            )
        }
    }
}

/// Download the per-release `sha256sum`-format checksum file and parse it
/// into a `name -> hex-digest` map for the artifacts we plan to download.
///
/// The checksum file is the trust anchor for ADR-002 §W5.1. It is fetched
/// from the same GitHub release URL as the artifacts, over TLS. Anyone
/// who can swap the artifact can also swap the checksum file, so the
/// real defence is end-to-end signing (cosign on the .tar.gz / SBOM
/// today, on the checksum file itself in a future iteration). What we
/// gain *now* is detection of mid-flight corruption and operator-error
/// substitution at the URL level — both of which are ruled out by a
/// matching hash.
///
/// Returns only entries for the artifacts in `wanted`; missing names
/// short-circuit to a clear error.
fn fetch_expected_hashes(
    checksums_url: &str,
    wanted: &[&str],
) -> Result<std::collections::HashMap<String, String>> {
    let tmp = tempfile::NamedTempFile::new().context("Failed to create temp file")?;
    let tmp_path = tmp.path().to_string_lossy().to_string();
    download_file(checksums_url, &tmp_path).with_context(|| {
        format!(
            "Failed to download checksum manifest from {checksums_url}.\n\
             ADR-002 §W5.1 requires a hash-verified download; refusing to\n\
             proceed without the checksum file. To bypass for an emergency\n\
             rotation, set MVM_SKIP_HASH_VERIFY=1."
        )
    })?;
    let body = std::fs::read_to_string(&tmp_path)
        .with_context(|| format!("Failed to read checksum file at {tmp_path}"))?;

    let mut map = std::collections::HashMap::new();
    for line in body.lines() {
        // `sha256sum` output: `<64-hex>  <filename>`. Two-space gap is
        // canonical; a single space marks "text mode" but we accept
        // either rather than be picky about emitter conventions.
        let mut iter = line.splitn(2, char::is_whitespace);
        let Some(hash) = iter.next() else { continue };
        let Some(rest) = iter.next() else { continue };
        let name = rest.trim().trim_start_matches('*').to_string();
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            map.insert(name, hash.to_ascii_lowercase());
        }
    }

    for w in wanted {
        if !map.contains_key(*w) {
            anyhow::bail!(
                "Checksum manifest at {checksums_url} did not include\n\
                 an entry for '{w}'. Refusing to download an unverifiable\n\
                 artifact. ADR-002 §W5.1."
            );
        }
    }
    Ok(map)
}

/// Stream `path` through SHA-256 and compare to `expected` (lowercase
/// hex). On mismatch, delete the file and bail with a clear message.
/// On `MVM_SKIP_HASH_VERIFY=1`, log a warning and accept — the env-var
/// is the documented escape hatch for emergency-rotation scenarios per
/// plan 29.
fn verify_artifact_hash(path: &str, name: &str, expected: Option<&String>) -> Result<()> {
    if std::env::var_os("MVM_SKIP_HASH_VERIFY").is_some() {
        tracing::warn!(
            "MVM_SKIP_HASH_VERIFY set — skipping integrity check on {name}. \
             ADR-002 §W5.1 documents this as an emergency-rotation escape hatch."
        );
        return Ok(());
    }
    let Some(expected) = expected else {
        // fetch_expected_hashes already enforced presence, but defend
        // against a refactor that decouples the steps.
        anyhow::bail!("internal: no expected hash recorded for {name}");
    };

    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open downloaded artifact at {path}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("Failed to hash downloaded artifact at {path}"))?;
    let actual = format!("{:x}", hasher.finalize());

    if actual != *expected {
        let _ = std::fs::remove_file(path);
        bump_verify_outcome("digest_mismatch");
        anyhow::bail!(
            "Integrity check failed for {name}.\n\
             expected sha256: {expected}\n\
             actual   sha256: {actual}\n\
             \n\
             The downloaded artifact does not match the published checksum.\n\
             Refusing to use it. Possible causes:\n\
             - mid-flight corruption (retry the download);\n\
             - mirror/CDN cache poisoning (open an issue);\n\
             - the release was re-uploaded and the manifest is stale.\n\
             ADR-002 §W5.1."
        );
    }
    ui::info(&format!("  ✓ verified {name} sha256={}", &actual[..12]));
    Ok(())
}

/// Download a file from a URL using curl.
fn download_file(url: &str, dest: &str) -> Result<()> {
    let status = std::process::Command::new("curl")
        .args(["-fSL", "--progress-bar", "-o", dest, url])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("Failed to run curl")?;

    if !status.success() {
        // Clean up partial download
        let _ = std::fs::remove_file(dest);
        anyhow::bail!(
            "Download failed. Pre-built images for v{version} may not yet be\n\
             published — release tags are pushed before the artifact-build\n\
             matrix completes, so a 404 here often just means the build is\n\
             still in flight. Check the release page or retry in a few\n\
             minutes:\n\
             \n\
             \x20   https://github.com/tinylabscom/mvm/releases/tag/v{version}\n\
             \n\
             To build locally instead, set up a Nix Linux builder:\n\
             \n\
             \x20 Option 1 — Temporary (run in another terminal):\n\
             \x20   nix run 'nixpkgs#darwin.linux-builder'\n\
             \n\
             \x20 Option 2 — Permanent (add to /etc/nix/nix.conf):\n\
             \x20   builders = ssh-ng://builder@linux-builder aarch64-linux /etc/nix/builder_ed25519 4 1 kvm,big-parallel - -\n\
             \x20   builders-use-substitutes = true",
            version = env!("CARGO_PKG_VERSION")
        );
    }
    Ok(())
}

/// Build a microVM image (kernel + rootfs) by spawning the mvm-owned
/// microsandbox builder VM and running `nix build` inside it.
///
/// `flake_dir` is bind-mounted into the builder as `/work`; the builder
/// runs `nix build /work#packages.<linux_system>.default` and copies
/// the resulting artifacts to `out_dir`. The host's `/nix/store` is
/// bind-mounted opportunistically (when present) for cache reuse — its
/// absence is fine, the builder will fetch from substituters.
///
/// ADR-013 §"Linux builder via microsandbox (no Lima)" — this replaces
/// the previous host-nix + `nix-darwin` `linux-builder` path so mvm
/// owns the builder VM end-to-end and the user needs no external Nix
/// configuration on the host.
#[cfg(feature = "contributor-bootstrap")]
fn build_image_via_microsandbox(flake_dir: &str, out_dir: &str) -> Result<(String, String)> {
    use mvm_build::builder_vm::{
        BUILDER_GUEST_WORK_DIR, BuilderJob, BuilderMounts, BuilderVm, MicrosandboxBuilderVm,
        host_system_linux,
    };

    let flake_src = std::path::PathBuf::from(flake_dir);
    if !flake_src.exists() {
        anyhow::bail!("flake dir does not exist: {flake_dir}");
    }

    // The bundled flakes live at `<workspace>/nix/images/<name>/` and
    // use `builtins.path { path = ../../..; }` to capture the workspace
    // root (for `mkGuest` lib + `Cargo.lock`). To make that resolve
    // correctly inside the sandbox, mount the whole workspace at /work
    // and point `flake_ref` at the subdir. Mounting only the flake dir
    // (the previous design) made `../../..` resolve to `/` of the
    // sandbox, which tripped over `/.msb/agent.sock` and other
    // microsandbox-internal files Nix can't import.
    let workspace_root = flake_src
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| {
            anyhow::anyhow!("flake dir is not three levels deep in workspace: {flake_dir}")
        })?
        .to_path_buf();
    let flake_rel = flake_src
        .strip_prefix(&workspace_root)
        .map_err(|_| anyhow::anyhow!("flake dir not under derived workspace root: {flake_dir}"))?;
    // `path:` URL forces Nix's filesystem flake fetcher rather than the
    // git fetcher. The git fetcher gets engaged automatically whenever
    // the flake path sits under a `.git` directory (the workspace
    // mount always does), and it fails on git worktrees — the
    // worktree's `.git` is a file whose `gitdir:` redirect points
    // outside the bind mount.
    let guest_flake_ref = format!(
        "path:{}/{}",
        BUILDER_GUEST_WORK_DIR,
        flake_rel
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("flake subpath has non-UTF-8 bytes: {flake_rel:?}"))?
    );

    // Bind-mount /nix opportunistically on Linux hosts that already
    // run native Nix — the builder reuses already-realized store paths,
    // and absence is fine because the in-sandbox store fetches from
    // substituters. Skip the bind on macOS: a multi-user Nix install
    // there leaves `/nix` owned by `root:wheel` (Apple Container's
    // bind-mounter fails with EACCES), *and* the store contains
    // Darwin-targeted closures that a Linux microVM can't execute
    // anyway. The microsandbox builder is on the macOS path precisely
    // because the host has no usable Linux Nix; reusing its Darwin
    // store is wrong regardless of permissions.
    let host_nix_store = if cfg!(target_os = "macos") {
        None
    } else {
        let host_nix = std::path::PathBuf::from("/nix");
        if host_nix.join("store").is_dir() {
            Some(host_nix)
        } else {
            None
        }
    };

    let job = BuilderJob::Flake {
        flake_ref: guest_flake_ref,
        attr_path: format!("packages.{}.default", host_system_linux()),
    };
    let mounts = BuilderMounts {
        flake_src: workspace_root,
        host_nix_store,
        artifact_out: std::path::PathBuf::from(out_dir),
    };

    MicrosandboxBuilderVm::default()
        .run_build(&job, &mounts)
        .map_err(|e| anyhow::anyhow!("microsandbox builder VM: {e}"))?;

    let kernel = format!("{out_dir}/vmlinux");
    let rootfs = format!("{out_dir}/rootfs.ext4");
    if !std::path::Path::new(&kernel).exists() {
        anyhow::bail!("builder VM did not produce vmlinux at {kernel}");
    }
    if !std::path::Path::new(&rootfs).exists() {
        anyhow::bail!("builder VM did not produce rootfs.ext4 at {rootfs}");
    }
    Ok((kernel, rootfs))
}

/// Find the dev-image Nix flake directory.
///
/// Returns `Ok(path)` only when `nix/images/builder/flake.nix` is present —
/// that flake is the only one whose `packages.<sys>.default` output
/// produces the vmlinux + rootfs.ext4 + sidecar shape that
/// `MicrosandboxBuilderVm::run_build` extracts from `/out`. The
/// parent `nix/flake.nix` exposes a library (`lib.mkGuest`) plus an
/// `internal-minimal-runner` test fixture, neither of which match
/// that contract — falling back to it earlier yielded a misleading
/// "double-prefix attribute" `nix build` failure inside the
/// sandbox. The bail signals `ensure_dev_image` to take the
/// published-prebuilt download path (W5.1 — hash-verified).
fn find_dev_image_flake() -> Result<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot find workspace root"))?;

    let candidate = workspace_root.join("nix").join("images").join("builder");
    if candidate.join("flake.nix").exists() {
        return Ok(candidate.to_str().unwrap_or(".").to_string());
    }

    anyhow::bail!("Dev image flake not found. Expected at nix/images/builder/flake.nix")
}

/// Plan 72 W5 sibling of [`find_dev_image_flake`] — locate the
/// **builder VM** flake at `nix/images/builder-vm/flake.nix`.
///
/// Distinct from the dev-shell image flake at `nix/images/builder/`:
/// the builder-vm flake produces a small (busybox + nix + tools)
/// rootfs whose closure fits in microsandbox's 4 GiB overlay, so
/// Stage 0 (`build_image_via_microsandbox`) can build it without
/// hitting the disk-full ceiling that motivated the whole Plan 72
/// migration. The dev-shell flake includes rustc + the workspace's
/// cargo closure, which does not fit and is what
/// `LibkrunBuilderVm` (Plan 72 W4) handles via virtio-blk-backed
/// `/nix` instead.
///
/// W5.B (this PR) wires this into `ensure_dev_image` as a precondition
/// for the libkrun dispatch path — when it returns `Ok`, the host has
/// the Layer 1 builder-VM flake it needs to bootstrap.
///
/// `allow(dead_code)`: the function is only called from the
/// libkrun-dispatch path inside `ensure_dev_image`, which itself only
/// compiles under `all(contributor-bootstrap, backends-builder-vm-libkrun)`.
/// Default-features and contributor-bootstrap-only builds compile the
/// function but never reach it; the lint silencing covers those.
#[allow(dead_code)]
fn find_builder_vm_flake() -> Result<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot find workspace root"))?;

    let candidate = workspace_root.join("nix").join("images").join("builder-vm");
    if candidate.join("flake.nix").exists() {
        return Ok(candidate.to_str().unwrap_or(".").to_string());
    }

    anyhow::bail!(
        "Builder VM flake not found. Expected at nix/images/builder-vm/flake.nix \
         (Plan 72 W2 artifact)."
    )
}

/// Plan 72 W5 — populate `~/.cache/mvm/builder-vm/<arch>/` with
/// `vmlinux` + `rootfs.ext4` + `cmdline.txt` + `manifest.json`
/// from the in-repo W2 flake, using Stage 0 (microsandbox +
/// `nixos/nix:2.24.10`) as the bootstrap.
///
/// `LibkrunBuilderVm::run_build` reads from this cache; this
/// function is what fills it. The two-layer artifact rule from
/// ADR-046 in action:
///
/// - Layer 1 (this function): build the **builder VM image**
///   via microsandbox (small closure, fits 4 GiB overlay).
/// - Layer 2 (a future `ensure_dev_image` rework): use the
///   Layer 1 image plus libkrun to build the **dev shell
///   image** with the large rustc closure.
///
/// Source-checkout-only by design — `find_builder_vm_flake()`
/// Two acquisition paths split on whether we have an in-repo flake:
///
/// 1. **Contributor path (source checkout)** — when `contributor-bootstrap`
///    is on and `find_builder_vm_flake()` returns Ok, Stage 0 microsandbox
///    builds the W2 flake locally **on every invocation**. No cache hit
///    fast path here — CLAUDE.md mandates that a contributor edit to
///    `nix/images/builder-vm/flake.nix` must show up in the very next
///    `mvmctl dev up`, and a cache-hit shortcut would mask local edits.
///    Microsandbox's own internal caching (OCI image + Nix store paths)
///    keeps the no-change rebuild fast, so this isn't expensive.
///
/// 2. **End-user download path (installed binary)** — when there's no
///    in-repo flake, fetch the per-arch artifacts the W2 release-workflow
///    job publishes (`builder-vm-vmlinux-<arch>`,
///    `builder-vm-rootfs-<arch>.ext4`, optional sidecars) from
///    `releases/download/v<version>/`. SHA-256 verifies per ADR-002 §W5.1.
///    A cache hit here IS the fast path — there's no upstream source
///    to be out of sync with; only the release tag matters and the
///    cache dir is keyed on it.
///
/// W5.B wired this into `ensure_dev_image`; Plan 72 W5's "Layer 1
/// outside source checkout — download the published prebuilt"
/// follow-up landed here.
///
/// `allow(dead_code)`: same justification as
/// [`find_builder_vm_flake`] — only called when
/// `backends-builder-vm-libkrun` is on.
#[allow(dead_code)]
fn bootstrap_builder_vm_image() -> Result<()> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let out_dir = format!("{}/builder-vm/{arch}", mvm_core::config::mvm_cache_dir());
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating builder-vm cache dir {out_dir}"))?;

    // Source-checkout path: always rebuild from the in-repo flake.
    // CLAUDE.md: "A contributor modifying `nix/images/builder-vm/flake.nix`
    // must see their change in the very next `mvmctl dev up` with no
    // release-pipeline round-trip." A cache-hit fast path would
    // silently mask local edits — the per-call microsandbox build is
    // the correctness gate. Microsandbox's own OCI + Nix-store caching
    // keeps the no-change rebuild fast.
    #[cfg(feature = "contributor-bootstrap")]
    if let Ok(flake_dir) = find_builder_vm_flake() {
        ui::info(&format!(
            "Bootstrapping builder VM image via Stage 0 (microsandbox + nixos/nix:2.24.10) \
             from: {flake_dir}"
        ));
        return match build_image_via_microsandbox(&flake_dir, &out_dir) {
            Ok((kernel, rootfs)) => {
                ui::success(&format!(
                    "Builder VM image ready at {out_dir} (kernel={kernel}, rootfs={rootfs})."
                ));
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "Stage 0 microsandbox build of the builder-vm flake at {flake_dir} failed: {e:#}\n\
                 The builder-vm rootfs closure is meant to fit in microsandbox's 4 GiB overlay \
                 (no rustc, no cargo crates — see Plan 72 §W2). If it doesn't, the package list \
                 in nix/images/builder-vm/flake.nix needs trimming."
            )),
        };
    }

    // Installed-binary path: no in-repo source to be out of sync with,
    // so a cache hit IS the fast path. Only download when the cache
    // is empty.
    let cached_kernel = format!("{out_dir}/vmlinux");
    let cached_rootfs = format!("{out_dir}/rootfs.ext4");
    if std::path::Path::new(&cached_kernel).is_file()
        && std::path::Path::new(&cached_rootfs).is_file()
    {
        ui::info(&format!("Builder VM image already cached at {out_dir}."));
        return Ok(());
    }

    ui::info(&format!(
        "Builder VM image not in cache; downloading published prebuilt for v{}...",
        env!("CARGO_PKG_VERSION")
    ));
    download_builder_vm_image(arch, &out_dir).with_context(|| {
        "downloading the builder VM image. The release-artifact path is the only \
         option for installed-binary users without contributor-bootstrap; rebuild \
         with `cargo install --path . --features contributor-bootstrap` to use the \
         in-repo Stage 0 microsandbox path instead."
    })?;
    Ok(())
}

/// Download the per-arch Layer 1 builder VM artifacts published by the
/// `builder-vm-image` release-workflow job into the local cache dir,
/// SHA-256-verified per ADR-002 §W5.1.
///
/// Mirrors `download_dev_image_inner` for the dev-shell image, minus
/// cosign signing (Plan 36 ADR-005 extends to builder-vm artifacts as
/// a follow-up). The required artifacts are `vmlinux` + `rootfs.ext4`;
/// `cmdline.txt` and `manifest.json` sidecars are best-effort
/// downloads with a fallback at the `mvm-build` consumer
/// (`ensure_builder_vm_image` uses the canonical Plan 72 §W2 cmdline
/// when `cmdline.txt` is missing).
#[allow(dead_code)]
fn download_builder_vm_image(arch: &str, cache_dir: &str) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let names = builder_vm_artifact_names(arch);
    let base_url = format!("https://github.com/tinylabscom/mvm/releases/download/v{version}");
    let kernel_url = format!("{base_url}/{}", names.kernel);
    let rootfs_url = format!("{base_url}/{}", names.rootfs);
    let cmdline_url = format!("{base_url}/{}", names.cmdline);
    let manifest_url = format!("{base_url}/{}", names.manifest);
    let checksums_url = format!("{base_url}/{}", names.checksums);

    // Required artifacts only; sidecars get best-effort treatment
    // below. `fetch_expected_hashes` enforces that the checksum file
    // contains entries for everything in `wanted` before any download
    // starts.
    let expected = fetch_expected_hashes(&checksums_url, &[&names.kernel, &names.rootfs])?;

    ui::info("  Fetching kernel...");
    let kernel_path = format!("{cache_dir}/vmlinux");
    download_file(&kernel_url, &kernel_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!(
            "Failed to download builder VM kernel from {kernel_url}"
        ))
    })?;
    verify_artifact_hash(&kernel_path, &names.kernel, expected.get(&names.kernel))?;

    ui::info("  Fetching rootfs...");
    let rootfs_path = format!("{cache_dir}/rootfs.ext4");
    download_file(&rootfs_url, &rootfs_path).map_err(|e| {
        bump_verify_outcome("network");
        e.context(format!(
            "Failed to download builder VM rootfs from {rootfs_url}"
        ))
    })?;
    verify_artifact_hash(&rootfs_path, &names.rootfs, expected.get(&names.rootfs))?;

    // Sidecars — best-effort. `cmdline.txt` has a documented fallback
    // in `mvm-build::libkrun_builder::ensure_builder_vm_image`;
    // `manifest.json` is informational. A 404 on either is fine; a
    // hash mismatch when the file IS present is still a hard fail.
    if let Some(expected_cmdline) = expected.get(&names.cmdline) {
        let cmdline_path = format!("{cache_dir}/cmdline.txt");
        if download_file(&cmdline_url, &cmdline_path).is_ok() {
            verify_artifact_hash(&cmdline_path, &names.cmdline, Some(expected_cmdline))?;
        }
    }
    if let Some(expected_manifest) = expected.get(&names.manifest) {
        let manifest_path = format!("{cache_dir}/manifest.json");
        if download_file(&manifest_url, &manifest_path).is_ok() {
            verify_artifact_hash(&manifest_path, &names.manifest, Some(expected_manifest))?;
        }
    }

    ui::success(&format!(
        "Builder VM image downloaded, hash-verified, and cached at {cache_dir}."
    ));
    Ok(())
}

/// Per-arch artifact filenames the release workflow's
/// `builder-vm-image` job uploads. Pure function — no I/O, no
/// network — so the unit test can verify naming matches the
/// release.yml side without touching the network.
#[allow(dead_code)]
struct BuilderVmArtifactNames {
    kernel: String,
    rootfs: String,
    cmdline: String,
    manifest: String,
    checksums: String,
}

#[allow(dead_code)]
fn builder_vm_artifact_names(arch: &str) -> BuilderVmArtifactNames {
    BuilderVmArtifactNames {
        kernel: format!("builder-vm-vmlinux-{arch}"),
        rootfs: format!("builder-vm-rootfs-{arch}.ext4"),
        cmdline: format!("builder-vm-{arch}.cmdline.txt"),
        manifest: format!("builder-vm-{arch}.manifest.json"),
        checksums: format!("builder-vm-{arch}-checksums-sha256.txt"),
    }
}

/// Plan 72 W5.B — build the dev-shell image via the libkrun-backed
/// builder VM.
///
/// Layer 1 (the builder VM image at `~/.cache/mvm/builder-vm/<arch>/`)
/// is bootstrapped via [`bootstrap_builder_vm_image`] on cache miss
/// (Stage 0 = microsandbox + nixos/nix:2.24.10). Layer 2 (the dev-shell
/// image the user boots into via `mvmctl dev up`) is built by
/// `LibkrunBuilderVm::run_build` against the in-repo
/// `nix/images/builder/` flake, inside a libkrun guest that mounts the
/// workspace at `/work` and writes its artifacts back through a
/// virtio-fs `/out` share.
///
/// On success returns the host-side paths to the produced `vmlinux`
/// and `rootfs.ext4` in `out_dir` (mirroring `build_image_via_microsandbox`).
///
/// Caller is expected to have:
///   - confirmed `mvm_libkrun::is_available()` true,
///   - confirmed `find_builder_vm_flake().is_ok()` (Layer 1 source is
///     present in the workspace),
///   - run [`prepare_dev_image_out_dir`] on `out_dir`.
// Gated only on `backends-builder-vm-libkrun` after Plan 72 W5.C:
// `bootstrap_builder_vm_image` handles the no-`contributor-bootstrap`
// case via the release-artifact download path. Stage 0 microsandbox
// is the contributor fast-path, not a hard requirement.
#[cfg(feature = "backends-builder-vm-libkrun")]
fn build_image_via_libkrun(out_dir: &str) -> Result<(String, String)> {
    use mvm_build::builder_vm::{BuilderJob, BuilderMounts, BuilderVm, host_system_linux};
    use mvm_build::libkrun_builder::LibkrunBuilderVm;

    // Stage 0 — ensure Layer 1 (the builder VM image) is in
    // `~/.cache/mvm/builder-vm/<arch>/`. Idempotent: first call builds
    // it via microsandbox; subsequent calls find the cache populated
    // and return immediately. Currently always pays the rebuild cost
    // because microsandbox doesn't surface a content-hash check — a
    // future PR can wire `manifest.json` SHA verification to make this
    // a cheap cache hit.
    bootstrap_builder_vm_image()
        .context("Stage 0 builder-VM image bootstrap (precondition for libkrun dispatch)")?;

    // Workspace root for the `/work` virtio-fs share. `find_dev_image_flake()`
    // returns `<workspace>/nix/images/builder`; the workspace itself is
    // three levels up. The dev-shell flake at
    // `nix/images/builder/flake.nix` reads `MVM_WORKSPACE_PATH=/work`
    // (set in the guest's `cmd.sh` by `LibkrunBuilderVm`) under
    // `--impure`, so the flake's `builtins.path` import lands on the
    // mount rather than the store-copied flake dir. Plan 72 W0
    // wired both halves of this.
    let dev_flake = find_dev_image_flake().context(
        "dev-shell flake missing at nix/images/builder/flake.nix; libkrun dispatch needs it as Layer 2 source",
    )?;
    let workspace_root = std::path::Path::new(&dev_flake)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive workspace root from {dev_flake}"))?
        .to_path_buf();

    // Inside the guest, `/work` is the workspace mount. The dev-shell
    // flake lives at `/work/nix/images/builder` from the cmd.sh's
    // perspective. `path:` forces Nix's filesystem flake fetcher (not
    // the git fetcher, which would discover `/work/.git` and trip on
    // worktree files whose `gitdir:` redirects point outside the
    // mount).
    let job = BuilderJob::Flake {
        flake_ref: "path:/work/nix/images/builder".to_string(),
        attr_path: format!("packages.{}.default", host_system_linux()),
    };
    let mounts = BuilderMounts {
        flake_src: workspace_root,
        // libkrun keeps `/nix` on a persistent virtio-blk; no host
        // bind-mount of `/nix/store` is used or wanted (would be a
        // Darwin-x-Linux closure mismatch on macOS anyway).
        host_nix_store: None,
        artifact_out: std::path::PathBuf::from(out_dir),
    };

    LibkrunBuilderVm::default()
        .run_build(&job, &mounts)
        .map_err(|e| anyhow::anyhow!("libkrun builder VM: {e}"))?;

    // run_build wrote vmlinux + rootfs.ext4 into out_dir via the
    // virtio-fs `/out` mount; the same files mvm-cli is about to
    // hand back to the dev-up path.
    let kernel = format!("{out_dir}/vmlinux");
    let rootfs = format!("{out_dir}/rootfs.ext4");
    if !std::path::Path::new(&kernel).exists() {
        anyhow::bail!("libkrun builder VM exited cleanly but did not produce {kernel}");
    }
    if !std::path::Path::new(&rootfs).exists() {
        anyhow::bail!("libkrun builder VM exited cleanly but did not produce {rootfs}");
    }
    Ok((kernel, rootfs))
}

/// Locate the bundled `nix/images/default-tenant/` flake.
///
/// This is the fallback used by image-taking commands (`mvmctl exec`,
/// `mvmctl up`) when neither `--flake` nor `--manifest` is supplied.
/// (Was `nix/default-microvm/` before W7.3.) Only the builder-VM path
/// in `ensure_default_microvm_image` consumes this helper; gating
/// matches that single call site.
#[cfg(feature = "contributor-bootstrap")]
fn find_default_microvm_flake() -> Result<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot find workspace root"))?;

    let candidate = workspace_root
        .join("nix")
        .join("images")
        .join("default-tenant");
    if candidate.join("flake.nix").exists() {
        return Ok(candidate.to_str().unwrap_or(".").to_string());
    }
    anyhow::bail!(
        "Default microVM image flake not found. Expected at nix/images/default-tenant/flake.nix"
    )
}

/// Ensure the bundled default microVM image (kernel + rootfs) is in the cache.
///
/// Used by any image-taking command when no `--flake` or `--manifest` was
/// supplied. Builds via the mvm-owned microsandbox builder VM on first
/// use and caches under `~/.cache/mvm/default-microvm/`. Returns
/// `(kernel_path, rootfs_path)`.
///
/// If the local build fails or no source flake is available (e.g. an
/// installed-binary build with `contributor-bootstrap` compiled out),
/// falls back to downloading a pre-built image from the matching
/// GitHub release.
pub(crate) fn ensure_default_microvm_image() -> Result<(String, String)> {
    let cache_dir = format!("{}/default-microvm", mvm_core::config::mvm_cache_dir());
    std::fs::create_dir_all(&cache_dir)?;

    let kernel_path = format!("{cache_dir}/vmlinux");
    let rootfs_path = format!("{cache_dir}/rootfs.ext4");

    if std::path::Path::new(&kernel_path).exists() && std::path::Path::new(&rootfs_path).exists() {
        return Ok((kernel_path, rootfs_path));
    }

    #[cfg(feature = "contributor-bootstrap")]
    if let Ok(flake_dir) = find_default_microvm_flake() {
        ui::info("Building default microVM image via microsandbox builder VM (first time only)...");
        match build_image_via_microsandbox(&flake_dir, &cache_dir) {
            Ok(_) => {
                ui::success("Default microVM image built and cached.");
                return Ok((kernel_path, rootfs_path));
            }
            Err(e) => {
                ui::warn(&format!(
                    "Local builder VM build failed ({e}); falling back to pre-built download."
                ));
            }
        }
    }

    download_default_microvm_image(&kernel_path, &rootfs_path)
}

/// Download a pre-built default microVM image (kernel + rootfs) from the
/// matching GitHub release. Mirrors `download_dev_image`, including the
/// ADR-002 §W5.1 hash-verify path.
fn download_default_microvm_image(
    kernel_path: &str,
    rootfs_path: &str,
) -> Result<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    let base_url = format!("https://github.com/tinylabscom/mvm/releases/download/v{version}");
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let kernel_name = format!("default-microvm-vmlinux-{arch}");
    let rootfs_name = format!("default-microvm-rootfs-{arch}.ext4");
    let checksums_name = format!("default-microvm-{arch}-checksums-sha256.txt");
    let kernel_url = format!("{base_url}/{kernel_name}");
    let rootfs_url = format!("{base_url}/{rootfs_name}");
    let checksums_url = format!("{base_url}/{checksums_name}");

    ui::info(&format!(
        "Downloading default microVM image (v{version})..."
    ));

    let expected = fetch_expected_hashes(&checksums_url, &[&kernel_name, &rootfs_name])?;

    ui::info("  Fetching kernel...");
    download_file(&kernel_url, kernel_path)
        .with_context(|| format!("Failed to download kernel from {kernel_url}"))?;
    verify_artifact_hash(
        kernel_path,
        &kernel_name,
        expected.get(kernel_name.as_str()),
    )?;

    ui::info("  Fetching rootfs...");
    download_file(&rootfs_url, rootfs_path)
        .with_context(|| format!("Failed to download rootfs from {rootfs_url}"))?;
    verify_artifact_hash(
        rootfs_path,
        &rootfs_name,
        expected.get(rootfs_name.as_str()),
    )?;

    ui::success("Default microVM image downloaded, hash-verified, and cached.");
    Ok((kernel_path.to_string(), rootfs_path.to_string()))
}

#[cfg(test)]
mod dev_status_image_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        home: Option<String>,
        data_dir: Option<String>,
        cache_dir: Option<String>,
    }

    impl EnvGuard {
        fn set(
            home: &std::path::Path,
            data_dir: &std::path::Path,
            cache_dir: &std::path::Path,
        ) -> Self {
            let guard = Self {
                home: std::env::var("HOME").ok(),
                data_dir: std::env::var("MVM_DATA_DIR").ok(),
                cache_dir: std::env::var("MVM_CACHE_DIR").ok(),
            };
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("MVM_DATA_DIR", data_dir);
                std::env::set_var("MVM_CACHE_DIR", cache_dir);
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.data_dir {
                    Some(value) => std::env::set_var("MVM_DATA_DIR", value),
                    None => std::env::remove_var("MVM_DATA_DIR"),
                }
                match &self.cache_dir {
                    Some(value) => std::env::set_var("MVM_CACHE_DIR", value),
                    None => std::env::remove_var("MVM_CACHE_DIR"),
                }
            }
        }
    }

    fn touch(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().expect("test path must have parent")).unwrap();
        std::fs::write(path, b"test").unwrap();
    }

    #[test]
    fn status_image_prefers_launchd_image_paths() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data_dir = tmp.path().join("data");
        let cache_dir = tmp.path().join("cache");
        let _env = EnvGuard::set(&home, &data_dir, &cache_dir);

        let launchd_kernel = tmp.path().join("daemon/vmlinux");
        let launchd_rootfs = tmp.path().join("daemon/rootfs.ext4");
        touch(&launchd_kernel);
        touch(&launchd_rootfs);
        touch(&data_dir.join("dev/current/vmlinux"));
        touch(&data_dir.join("dev/current/rootfs.ext4"));

        let plist_path = dev_launchd_plist_path();
        std::fs::create_dir_all(plist_path.parent().unwrap()).unwrap();
        std::fs::write(
            plist_path,
            format!(
                r#"<plist version="1.0">
<dict>
    <key>EnvironmentVariables</key>
    <dict>
        <key>MVM_DEV_KERNEL</key>
        <string>{}</string>
        <key>MVM_DEV_ROOTFS</key>
        <string>{}</string>
    </dict>
</dict>
</plist>"#,
                launchd_kernel.display(),
                launchd_rootfs.display()
            ),
        )
        .unwrap();

        assert_eq!(
            resolve_dev_status_image(),
            Some(DevStatusImage {
                kernel_path: Some(launchd_kernel.to_string_lossy().into_owned()),
                rootfs_path: launchd_rootfs.to_string_lossy().into_owned(),
            })
        );
    }

    #[test]
    fn status_image_reports_current_data_dir_image() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(
            &tmp.path().join("home"),
            &tmp.path().join("data"),
            &tmp.path().join("cache"),
        );
        let kernel = tmp.path().join("data/dev/current/vmlinux");
        let rootfs = tmp.path().join("data/dev/current/rootfs.ext4");
        touch(&kernel);
        touch(&rootfs);

        assert_eq!(
            resolve_dev_status_image(),
            Some(DevStatusImage {
                kernel_path: Some(kernel.to_string_lossy().into_owned()),
                rootfs_path: rootfs.to_string_lossy().into_owned(),
            })
        );
    }

    #[test]
    fn status_image_reports_versioned_prebuilt_when_current_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let _env = EnvGuard::set(
            &tmp.path().join("home"),
            &data_dir,
            &tmp.path().join("cache"),
        );
        let dir = data_dir
            .join("dev/prebuilt")
            .join(format!("v{}", env!("CARGO_PKG_VERSION")));
        let rootfs = dir.join("rootfs.ext4");
        touch(&rootfs);

        assert_eq!(
            resolve_dev_status_image(),
            Some(DevStatusImage {
                kernel_path: None,
                rootfs_path: rootfs.to_string_lossy().into_owned(),
            })
        );
    }

    #[test]
    fn status_image_falls_back_to_legacy_cache_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        let _env = EnvGuard::set(
            &tmp.path().join("home"),
            &tmp.path().join("data"),
            &cache_dir,
        );
        let rootfs = cache_dir.join("dev/rootfs.ext4");
        touch(&rootfs);

        assert_eq!(
            resolve_dev_status_image(),
            Some(DevStatusImage {
                kernel_path: None,
                rootfs_path: rootfs.to_string_lossy().into_owned(),
            })
        );
    }

    #[test]
    fn status_image_is_none_when_no_rootfs_exists() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(
            &tmp.path().join("home"),
            &tmp.path().join("data"),
            &tmp.path().join("cache"),
        );

        assert_eq!(resolve_dev_status_image(), None);
    }
}

#[cfg(test)]
mod hash_verify_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::sync::Mutex;

    /// Cargo test runs tests in parallel within a single binary. Two
    /// of these tests touch `MVM_SKIP_HASH_VERIFY` (the global env-var
    /// escape hatch from ADR-002 §W5.1), so they have to be serialised
    /// against each other and against any other test that hashes a
    /// real artifact. Static mutex held for the test's lifetime.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Compute the canonical lowercase-hex SHA-256 of a byte slice. Tests
    /// use this to derive matching expected values without rebuilding
    /// the production hash path.
    fn hex_sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn verify_hash_accepts_matching_artifact() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        let bytes = b"hello world\n";
        std::fs::write(&path, bytes).unwrap();
        let expected = hex_sha256(bytes);
        let result = verify_artifact_hash(path.to_str().unwrap(), "artifact", Some(&expected));
        assert!(
            result.is_ok(),
            "matching hash should be accepted: {result:?}"
        );
        // File must still exist on success.
        assert!(
            path.exists(),
            "verified file must not be deleted on success"
        );
    }

    #[test]
    fn verify_hash_rejects_mismatched_artifact_and_deletes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        std::fs::write(&path, b"actual contents").unwrap();
        let expected = hex_sha256(b"different contents");
        let err = verify_artifact_hash(path.to_str().unwrap(), "artifact", Some(&expected))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Integrity check failed"),
            "expected integrity-check error, got: {err}"
        );
        assert!(
            !path.exists(),
            "tampered file must be deleted to prevent reuse"
        );
    }

    #[test]
    fn verify_hash_skip_env_var_bypasses_check() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Ensure the file exists even though we'll set a "wrong" hash.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        std::fs::write(&path, b"contents").unwrap();
        let wrong = hex_sha256(b"definitely not the contents");

        // SAFETY: ENV_LOCK serialises every test that touches this env
        // var, so no concurrent reader observes a half-set value. The
        // unsafe block is only required by edition-2024's set_var /
        // remove_var signatures; behaviour is unchanged.
        unsafe {
            std::env::set_var("MVM_SKIP_HASH_VERIFY", "1");
        }
        let result = verify_artifact_hash(path.to_str().unwrap(), "artifact", Some(&wrong));
        unsafe {
            std::env::remove_var("MVM_SKIP_HASH_VERIFY");
        }
        assert!(result.is_ok(), "skip-env should bypass check: {result:?}");
    }

    #[test]
    fn fetch_expected_hashes_parses_sha256sum_format() {
        // Run a tiny in-process HTTP server? Overkill — the function
        // takes a URL and shells out to curl. Instead, we test the
        // parser by exercising it directly via a file:// URL: curl
        // accepts file:// and just copies the bytes.
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("checksums.txt");
        let mut f = std::fs::File::create(&manifest_path).unwrap();
        // Two-space gap is canonical sha256sum output. Mix in a leading
        // '*' on one line (binary mode) to confirm we strip it.
        writeln!(f, "{}  dev-vmlinux-x86_64", "a".repeat(64)).unwrap();
        writeln!(f, "{} *dev-rootfs-x86_64.ext4", "b".repeat(64)).unwrap();
        writeln!(f, "garbage line that is not a hash").unwrap();
        drop(f);

        let url = format!("file://{}", manifest_path.display());
        let map = fetch_expected_hashes(&url, &["dev-vmlinux-x86_64", "dev-rootfs-x86_64.ext4"])
            .expect("manifest should parse");
        assert_eq!(map.get("dev-vmlinux-x86_64").unwrap(), &"a".repeat(64));
        assert_eq!(map.get("dev-rootfs-x86_64.ext4").unwrap(), &"b".repeat(64));
    }

    #[test]
    fn fetch_expected_hashes_errors_when_artifact_missing_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("checksums.txt");
        std::fs::write(
            &manifest_path,
            format!("{}  some-other-file\n", "c".repeat(64)),
        )
        .unwrap();

        let url = format!("file://{}", manifest_path.display());
        let err = fetch_expected_hashes(&url, &["dev-vmlinux-x86_64"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("did not include") && err.contains("dev-vmlinux-x86_64"),
            "expected missing-entry error, got: {err}"
        );
    }
}

#[cfg(test)]
mod builder_vm_bootstrap_tests {
    //! Plan 72 W5 — `find_builder_vm_flake` + `bootstrap_builder_vm_image`.
    use super::*;

    #[test]
    fn find_builder_vm_flake_resolves_to_in_repo_path() {
        // From a source checkout, the helper must find the W2
        // flake at <workspace>/nix/images/builder-vm/flake.nix.
        // `env!("CARGO_MANIFEST_DIR")` is baked at compile time
        // and points at the workspace's mvm-cli crate dir, so
        // this assertion is robust across `cargo test` and
        // `cargo nextest`.
        let path = find_builder_vm_flake().expect("expected builder-vm flake present in repo");
        assert!(
            path.ends_with("nix/images/builder-vm"),
            "unexpected flake path: {path}"
        );
        // The flake file itself must be readable.
        assert!(
            std::path::Path::new(&path).join("flake.nix").is_file(),
            "flake.nix missing under {path}"
        );
    }

    #[cfg(not(feature = "contributor-bootstrap"))]
    #[test]
    fn source_checkout_dev_image_errors_without_contributor_bootstrap() {
        let err =
            source_checkout_requires_contributor_bootstrap("/repo/nix/images/builder").to_string();
        assert!(
            err.contains("Refusing to download the published prebuilt"),
            "error must make the no-download invariant explicit: {err}"
        );
        assert!(
            err.contains("cargo run --features contributor-bootstrap -- dev up"),
            "error should include the contributor rebuild command: {err}"
        );
    }

    /// Per-arch artifact filenames must match what the release
    /// workflow's `builder-vm-image` job uploads. Pure function —
    /// asserts the contract between `builder_vm_artifact_names()`
    /// (the consumer side that constructs download URLs) and the
    /// `cp "$STORE_PATH/..." "staging/builder-vm-..."` lines in
    /// `.github/workflows/release.yml` (the producer side).
    #[test]
    fn builder_vm_artifact_names_match_release_workflow() {
        let n = builder_vm_artifact_names("aarch64");
        assert_eq!(n.kernel, "builder-vm-vmlinux-aarch64");
        assert_eq!(n.rootfs, "builder-vm-rootfs-aarch64.ext4");
        assert_eq!(n.cmdline, "builder-vm-aarch64.cmdline.txt");
        assert_eq!(n.manifest, "builder-vm-aarch64.manifest.json");
        assert_eq!(n.checksums, "builder-vm-aarch64-checksums-sha256.txt");

        let n = builder_vm_artifact_names("x86_64");
        assert_eq!(n.kernel, "builder-vm-vmlinux-x86_64");
        assert_eq!(n.rootfs, "builder-vm-rootfs-x86_64.ext4");
        assert_eq!(n.cmdline, "builder-vm-x86_64.cmdline.txt");
        assert_eq!(n.manifest, "builder-vm-x86_64.manifest.json");
        assert_eq!(n.checksums, "builder-vm-x86_64-checksums-sha256.txt");
    }
}
