//! Stage 0 bootstrap — builds an in-memory initramfs that boots
//! against the libkrunfw kernel, runs `nix build` against the
//! in-repo `nix/images/builder-vm` flake, and emits the steady-state
//! builder VM image (kernel + rootfs.ext4) on the `/out` virtio-fs
//! share.
//!
//! Replaces the previous ur-seed flake + tarball + seed-contract
//! pipeline. The "ur-seed" used to be a ~190 MiB tarball produced
//! by `nix/ur-seed/flake.nix`, downloaded with sha256 verification
//! into `~/.cache/mvm/ur-seed/`. Now the bootstrap surface is:
//!
//! 1. **`init.sh`** (embedded via `include_str!`) — the PID-1
//!    shell script. Lives at [`INIT_SCRIPT`].
//! 2. **`busybox-aarch64-linux-musl`** static (~1 MiB) — bundles
//!    every userspace tool the init script needs (sh, mount, ip,
//!    udhcpc, cp, …). Downloaded once into [`stage0_cache_dir`].
//! 3. **`nix-portable-aarch64-linux`** static (~50 MiB) — daemon-less
//!    Nix runtime. Same cache dir.
//!
//! Both binaries are pinned by URL + SHA-256 in the
//! [`BootstrapAsset`] table below. No flake, no manifest schema,
//! no contract version, no `mvm-builder-init` baked into a
//! release-frozen rootfs.
//!
//! Per-run, [`build_initramfs`] walks the cache, the embedded
//! init script, and a small set of directory stubs into a cpio
//! newc archive (`crate::cpio`) that libkrun consumes via
//! `KrunContext::new_initramfs`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::cpio::{CpioArchive, Perm};

/// PID-1 shell script. Compiled into the binary so a fresh `mvmctl`
/// can always emit it without consulting the cache or the network.
pub const INIT_SCRIPT: &str = include_str!("stage0/init.sh");

/// One downloadable bootstrap asset, pinned by upstream URL +
/// SHA-256. The cache key is [`Self::cache_filename`].
#[derive(Debug, Clone, Copy)]
pub struct BootstrapAsset {
    /// Where the file lands inside [`stage0_cache_dir`].
    pub cache_filename: &'static str,
    /// Upstream URL. Pinned to a specific version; never moves.
    pub url: &'static str,
    /// SHA-256 of the byte stream at [`Self::url`]. Hex (64 chars).
    pub sha256_hex: &'static str,
    /// File mode the asset gets on disk (and inside the initramfs).
    pub mode: u32,
}

/// busybox-static aarch64-linux-musl. Hosted at
/// `tinylabscom/mvm-bootstrap` so the URL is stable across mvm
/// releases. Build recipe: `pkgs.pkgsStatic.busybox` on
/// `aarch64-linux`, then `strip --strip-all`. See
/// `nix/bootstrap/busybox.nix` (added in a follow-up).
///
/// The hash below is a placeholder until the first
/// `mvm-bootstrap` release is cut. `prepare_assets` will
/// surface a clear error when the placeholder doesn't match
/// the upstream byte stream.
pub const BUSYBOX_AARCH64: BootstrapAsset = BootstrapAsset {
    cache_filename: "busybox",
    url: "https://github.com/tinylabscom/mvm-bootstrap/releases/download/v0.1.0/busybox-aarch64-linux-musl",
    sha256_hex: "0000000000000000000000000000000000000000000000000000000000000000",
    mode: 0o755,
};

/// nix-portable aarch64-linux. Upstream release from
/// `DavHau/nix-portable` — well-maintained, single static binary,
/// runs Nix without a daemon or `/nix/store`.
pub const NIX_PORTABLE_AARCH64: BootstrapAsset = BootstrapAsset {
    cache_filename: "nix-portable",
    url: "https://github.com/DavHau/nix-portable/releases/download/v013/nix-portable-aarch64",
    sha256_hex: "0000000000000000000000000000000000000000000000000000000000000000",
    mode: 0o755,
};

/// Every asset Stage 0 needs on aarch64-linux hosts.
pub const ASSETS_AARCH64: &[&BootstrapAsset] = &[&BUSYBOX_AARCH64, &NIX_PORTABLE_AARCH64];

/// `~/.cache/mvm/stage0/`. Materialized by [`prepare_assets`].
pub fn stage0_cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join(".cache").join("mvm").join("stage0")
}

/// Ensure every entry in `assets` is present in the cache dir
/// with the matching SHA-256. Missing assets are fetched via
/// `reqwest::blocking::get`; mismatched ones are re-fetched
/// (the existing file is moved aside before the new one writes,
/// so an interrupted download never leaves a half-trusted file
/// in place).
///
/// Network access only happens on first run (or after a manual
/// cache prune). Subsequent invocations short-circuit on the
/// per-file sha256 check.
pub fn prepare_assets(assets: &[&BootstrapAsset]) -> Result<()> {
    let dir = stage0_cache_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    for asset in assets {
        let target = dir.join(asset.cache_filename);
        if target.is_file() && verify_sha256(&target, asset.sha256_hex)? {
            continue;
        }
        fetch_to(&target, asset).with_context(|| format!("fetching {}", asset.url))?;
    }
    Ok(())
}

/// Build the Stage 0 initramfs (cpio newc bytes) from the cached
/// bootstrap assets + the embedded init script. The returned bytes
/// can be written to a temp file and handed to libkrun via
/// `KrunContext::new_initramfs`.
///
/// `applet_names` is the list of busybox applet symlinks to
/// pre-create under `/bin/`. The init script also runs
/// `busybox --install -s /bin` at boot to be exhaustive, but
/// having the basics (`sh`, `mount`, `ip`, `udhcpc`, `cp`,
/// `mkdir`, `mountpoint`, `sync`, `poweroff`) as actual symlinks
/// lets the kernel resolve `#!/bin/sh` before `/init` runs.
pub fn build_initramfs() -> Result<Vec<u8>> {
    let dir = stage0_cache_dir();
    let busybox = read_asset(&dir, &BUSYBOX_AARCH64)?;
    let nix_portable = read_asset(&dir, &NIX_PORTABLE_AARCH64)?;

    let mut arc = CpioArchive::new();
    // Directory stubs the init script (and the kernel's early-boot
    // mounts) need to exist before mount(2) succeeds.
    for d in &[
        "/bin",
        "/dev",
        "/etc",
        "/out",
        "/proc",
        "/run",
        "/sys",
        "/tmp",
        "/usr",
        "/usr/local",
        "/usr/local/bin",
        "/work",
    ] {
        arc.dir(*d, Perm::DIR_755);
    }
    // /sbin is conventionally a symlink to /bin on busybox systems —
    // some applets (poweroff, udhcpc) are looked up under /sbin.
    arc.symlink("/sbin", "bin");

    arc.file("/init", Perm::FILE_755, INIT_SCRIPT.as_bytes());
    arc.file("/bin/busybox", Perm(BUSYBOX_AARCH64.mode), busybox);
    arc.file(
        "/usr/local/bin/nix-portable",
        Perm(NIX_PORTABLE_AARCH64.mode),
        nix_portable,
    );

    // Pre-materialize the busybox applet symlinks /init relies on
    // before `busybox --install -s` runs. The shebang `#!/bin/sh`
    // needs `/bin/sh` resolvable the moment the kernel executes
    // `/init`, which is before any line of /init has run.
    for applet in PRE_INSTALLED_APPLETS {
        arc.symlink(format!("/bin/{applet}"), "busybox");
    }

    arc.into_bytes().context("serializing Stage 0 cpio archive")
}

/// busybox applets the init script invokes directly. Each one
/// gets a /bin/<name> -> busybox symlink in the initramfs.
const PRE_INSTALLED_APPLETS: &[&str] = &[
    "sh",
    "mount",
    "umount",
    "mkdir",
    "mountpoint",
    "cp",
    "ip",
    "udhcpc",
    "sync",
    "poweroff",
    "cat",
    "echo",
    "ls",
    "rm",
];

fn read_asset(dir: &Path, asset: &BootstrapAsset) -> Result<Vec<u8>> {
    let path = dir.join(asset.cache_filename);
    let mut f = std::fs::File::open(&path).with_context(|| {
        format!(
            "opening Stage 0 asset {} (run `mvmctl dev fetch-stage0` first)",
            path.display(),
        )
    })?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(buf)
}

fn verify_sha256(path: &Path, expected_hex: &str) -> Result<bool> {
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for sha256 check", path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    Ok(got.eq_ignore_ascii_case(expected_hex))
}

fn fetch_to(target: &Path, asset: &BootstrapAsset) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("target {} has no parent", target.display()))?;
    let staging = parent.join(format!(
        ".{}.staging.{}",
        asset.cache_filename,
        std::process::id()
    ));

    eprintln!("[mvm] downloading {} -> {}", asset.url, target.display());
    let resp = reqwest::blocking::get(asset.url)
        .with_context(|| format!("GET {}", asset.url))?
        .error_for_status()
        .with_context(|| format!("GET {} returned non-success status", asset.url))?;
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading body from {}", asset.url))?;

    let got_hex = hex::encode(Sha256::digest(&bytes));
    if !got_hex.eq_ignore_ascii_case(asset.sha256_hex) {
        bail!(
            "sha256 mismatch fetching {asset_url}: expected {want}, got {got}. \
             Either the upstream byte stream drifted or the pinned hash needs an update.",
            asset_url = asset.url,
            want = asset.sha256_hex,
            got = got_hex,
        );
    }

    {
        let mut out = std::fs::File::create(&staging)
            .with_context(|| format!("creating staging file {}", staging.display()))?;
        out.write_all(&bytes)
            .with_context(|| format!("writing {}", staging.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(asset.mode))
            .with_context(|| format!("chmod {} -> {:o}", staging.display(), asset.mode))?;
    }
    std::fs::rename(&staging, target).with_context(|| {
        format!(
            "atomic-rename {} -> {}",
            staging.display(),
            target.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_script_is_embedded_and_nonempty() {
        assert!(!INIT_SCRIPT.is_empty());
        assert!(
            INIT_SCRIPT.contains("#!/bin/sh"),
            "init script has a shebang"
        );
        assert!(
            INIT_SCRIPT.contains("ip link set eth0 up"),
            "init script brings eth0 up explicitly (the udhcpc-ENETDOWN bug fix)"
        );
        assert!(
            INIT_SCRIPT.contains("nix-portable"),
            "init script invokes nix-portable"
        );
    }

    #[test]
    fn cache_dir_is_under_home() {
        let path = stage0_cache_dir();
        let s = path.to_string_lossy();
        assert!(s.contains(".cache/mvm/stage0"), "got {s}");
    }

    #[test]
    fn verify_sha256_matches_real_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"hello").unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&path, expected).unwrap());
        let wrong = "0".repeat(64);
        assert!(!verify_sha256(&path, &wrong).unwrap());
    }

    #[test]
    fn assets_table_covers_busybox_and_nix_portable() {
        let names: Vec<_> = ASSETS_AARCH64.iter().map(|a| a.cache_filename).collect();
        assert!(names.contains(&"busybox"));
        assert!(names.contains(&"nix-portable"));
    }

    #[test]
    fn pinned_asset_urls_are_https() {
        for asset in ASSETS_AARCH64 {
            assert!(
                asset.url.starts_with("https://"),
                "{} pins a non-https URL: {}",
                asset.cache_filename,
                asset.url
            );
        }
    }

    #[test]
    fn pinned_asset_sha256_is_64_hex_chars() {
        for asset in ASSETS_AARCH64 {
            assert_eq!(
                asset.sha256_hex.len(),
                64,
                "{} sha256 is not 64 chars",
                asset.cache_filename
            );
            assert!(
                asset.sha256_hex.chars().all(|c| c.is_ascii_hexdigit()),
                "{} sha256 contains non-hex chars",
                asset.cache_filename
            );
        }
    }

    /// Smoke: building the initramfs against a temp cache dir with
    /// stub binaries produces a valid cpio archive containing the
    /// init script.
    #[test]
    fn build_initramfs_against_stub_cache() {
        let dir = TempDir::new().unwrap();
        // Place stub assets at the cache filenames the module expects.
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("busybox"), b"FAKE_BUSYBOX").unwrap();
        std::fs::write(dir.path().join("nix-portable"), b"FAKE_NIX_PORTABLE").unwrap();

        // The module's `stage0_cache_dir()` reads from $HOME; swap.
        // SAFETY: this test mutates env vars but isn't expected to
        // race with the others in this module (none touch HOME).
        let saved = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", dir.path().parent().unwrap_or(dir.path()));
        }
        // The path is `$HOME/.cache/mvm/stage0`; we need our stubs
        // there. Move them.
        let real_dir = stage0_cache_dir();
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::rename(dir.path().join("busybox"), real_dir.join("busybox")).unwrap();
        std::fs::rename(
            dir.path().join("nix-portable"),
            real_dir.join("nix-portable"),
        )
        .unwrap();

        let bytes = build_initramfs().expect("build_initramfs succeeds with stubs in place");

        // Restore HOME.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        // The archive should contain the init script content + the
        // stub asset bytes.
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("#!/bin/sh"), "init script content present");
        assert!(s.contains("FAKE_BUSYBOX"), "stub busybox bytes present");
        assert!(
            s.contains("FAKE_NIX_PORTABLE"),
            "stub nix-portable bytes present"
        );
        assert!(s.contains("TRAILER!!!"), "cpio archive is well-terminated");

        // Cleanup our test cache dir.
        let _ = std::fs::remove_dir_all(real_dir);
    }
}
