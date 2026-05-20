//! Stage 0 bootstrap — materializes a host directory tree that
//! libkrun mounts as the guest root (via `krun_set_root`), boots
//! against libkrunfw's bundled kernel, runs `nix build` against the
//! in-repo `nix/images/builder-vm` flake, and emits the steady-state
//! builder VM image (kernel + rootfs.ext4) on the `/out` virtio-fs
//! share.
//!
//! Bootstrap surface:
//!
//! 1. **`init.sh`** (embedded via `include_str!`) — the PID-1
//!    shell script. Lives at [`INIT_SCRIPT`].
//! 2. **busybox-aarch64-linux-musl** static (~1.6 MiB) — vendored
//!    in-tree as `stage0/busybox-aarch64-linux-musl`, embedded via
//!    `include_bytes!` ([`BUSYBOX_AARCH64_BYTES`]). Provides sh,
//!    mount, ip, udhcpc, cp, … under the busybox multi-call binary.
//! 3. **nix-portable-aarch64-linux** static (~74 MiB) — daemon-less
//!    Nix runtime. Downloaded once from DavHau/nix-portable's
//!    upstream release into [`stage0_cache_dir`] with sha256
//!    verification ([`NIX_PORTABLE_AARCH64`]).
//!
//! Per-run, [`materialize_root_dir`] writes those three assets plus
//! a small set of directory stubs + busybox applet symlinks into a
//! caller-supplied directory. The supervisor hands that directory
//! to libkrun via `krun_set_root`; libkrun mounts it as the guest
//! root over virtiofs and runs `/init` as PID 1.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

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
    /// File mode the asset gets on disk (and inside the root dir).
    pub mode: u32,
}

/// busybox-static aarch64-linux-musl. Vendored in-tree (~1.6 MiB)
/// because it's small enough that embedding the bytes directly into
/// mvmctl is less pain than maintaining a separate fetch flow.
///
/// Origin: `pkgs.pkgsStatic.busybox` on `aarch64-linux` from
/// nixpkgs, then `strip --strip-all`.
pub const BUSYBOX_AARCH64_BYTES: &[u8] = include_bytes!("stage0/busybox-aarch64-linux-musl");

/// SHA-256 of [`BUSYBOX_AARCH64_BYTES`]. Verified at the bottom of
/// this file so a tampered vendored binary fails the workspace
/// test suite.
pub const BUSYBOX_AARCH64_SHA256: &str =
    "710d9568fb39d2450809551eb3517eda124398d21952993040284cbc386f0cc7";

/// nix-portable aarch64-linux. Upstream release from
/// `DavHau/nix-portable` — well-maintained, single static binary,
/// runs Nix without a daemon or `/nix/store`. ~74 MiB.
///
/// Downloaded once on first `dev up` into [`stage0_cache_dir`].
/// Too big to vendor.
pub const NIX_PORTABLE_AARCH64: BootstrapAsset = BootstrapAsset {
    cache_filename: "nix-portable",
    url: "https://github.com/DavHau/nix-portable/releases/download/v012/nix-portable-aarch64",
    sha256_hex: "af41d8defdb9fa17ee361220ee05a0c758d3e6231384a3f969a314f9133744ea",
    mode: 0o755,
};

/// Every downloadable asset Stage 0 needs on aarch64-linux hosts.
/// busybox is vendored (not downloaded), so it's not in this list.
pub const ASSETS_AARCH64: &[&BootstrapAsset] = &[&NIX_PORTABLE_AARCH64];

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

/// Directory stubs the init script (and the kernel's early-boot
/// mounts) need to exist before `mount(2)` succeeds. Listed relative
/// to the root, with no leading slash.
const ROOT_DIR_STUBS: &[&str] = &[
    "bin",
    "dev",
    "etc",
    "out",
    "proc",
    "run",
    "sys",
    "tmp",
    "usr",
    "usr/local",
    "usr/local/bin",
    "work",
];

/// busybox applets the init script invokes directly. Each one
/// gets a `bin/<name>` -> `busybox` symlink in the root dir.
const PRE_INSTALLED_APPLETS: &[&str] = &[
    "sh", "mount", "umount", "mkdir", "mountpoint", "cp", "ip", "udhcpc", "sync", "poweroff",
    "cat", "echo", "ls", "rm",
];

/// Materialize a Stage 0 guest root at `dest` from the embedded
/// busybox + the cached nix-portable + the embedded init script.
/// The supervisor hands `dest` to libkrun via `krun_set_root`,
/// which mounts it as the guest root over virtiofs.
///
/// Idempotent over a clean `dest`. Caller is responsible for making
/// sure `dest` is empty (or doesn't exist) — the function creates
/// the directory tree from scratch.
///
/// Caller must have already run [`prepare_assets`] so nix-portable
/// is present in the cache dir.
pub fn materialize_root_dir(dest: &Path) -> Result<()> {
    let cache = stage0_cache_dir();
    let nix_portable_src = cache.join(NIX_PORTABLE_AARCH64.cache_filename);
    if !nix_portable_src.is_file() {
        bail!(
            "Stage 0 asset missing: {} (run `prepare_assets` first)",
            nix_portable_src.display()
        );
    }

    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating Stage 0 root dir {}", dest.display()))?;

    for stub in ROOT_DIR_STUBS {
        let p = dest.join(stub);
        std::fs::create_dir_all(&p)
            .with_context(|| format!("creating Stage 0 stub dir {}", p.display()))?;
    }

    // /sbin → bin (some applets like poweroff/udhcpc are looked up
    // under /sbin by upstream conventions).
    symlink_relative("bin", &dest.join("sbin"))?;

    write_file_mode(&dest.join("init"), INIT_SCRIPT.as_bytes(), 0o755)
        .context("writing Stage 0 /init")?;
    write_file_mode(
        &dest.join("bin").join("busybox"),
        BUSYBOX_AARCH64_BYTES,
        0o755,
    )
    .context("writing Stage 0 /bin/busybox")?;

    // nix-portable is ~74 MiB; copy from cache rather than load it
    // through a Vec<u8>.
    let np_dst = dest.join("usr").join("local").join("bin").join("nix-portable");
    std::fs::copy(&nix_portable_src, &np_dst).with_context(|| {
        format!(
            "copying {} -> {}",
            nix_portable_src.display(),
            np_dst.display()
        )
    })?;
    set_mode(&np_dst, NIX_PORTABLE_AARCH64.mode)?;

    // Materialize busybox applet symlinks. The kernel's binfmt_script
    // handler will need `bin/sh` resolvable the moment libkrun execs
    // `/init`, before any line of /init has run.
    for applet in PRE_INSTALLED_APPLETS {
        symlink_relative("busybox", &dest.join("bin").join(applet))?;
    }

    Ok(())
}

fn write_file_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut f = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    set_mode(path, mode)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {} -> {:o}", path.display(), mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Create a symlink at `link` pointing at `target` (a relative path).
/// Replaces an existing symlink at `link` if any.
fn symlink_relative(target: &str, link: &Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(link)
            .with_context(|| format!("removing existing {}", link.display()))?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("symlink {} -> {target}", link.display()))?;
    }
    #[cfg(not(unix))]
    {
        bail!(
            "Stage 0 root materialization requires a Unix host (symlink at {})",
            link.display()
        );
    }
    Ok(())
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
    set_mode(&staging, asset.mode)?;
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
    fn vendored_busybox_matches_pinned_sha256() {
        let got = hex::encode(Sha256::digest(BUSYBOX_AARCH64_BYTES));
        assert_eq!(
            got, BUSYBOX_AARCH64_SHA256,
            "vendored busybox bytes do not match the pinned sha256; \
             someone tampered with `stage0/busybox-aarch64-linux-musl` or \
             forgot to update BUSYBOX_AARCH64_SHA256"
        );
    }

    #[test]
    fn vendored_busybox_is_aarch64_elf() {
        assert!(
            BUSYBOX_AARCH64_BYTES.len() > 64,
            "busybox too small to be a real ELF"
        );
        assert_eq!(
            &BUSYBOX_AARCH64_BYTES[0..4],
            &[0x7f, b'E', b'L', b'F'],
            "vendored busybox lacks ELF magic"
        );
        assert_eq!(
            BUSYBOX_AARCH64_BYTES[18], 0xB7,
            "vendored busybox e_machine is not EM_AARCH64 (0xB7)"
        );
    }

    #[test]
    fn downloadable_assets_table_covers_nix_portable() {
        let names: Vec<_> = ASSETS_AARCH64.iter().map(|a| a.cache_filename).collect();
        assert!(names.contains(&"nix-portable"));
        assert!(!names.contains(&"busybox"));
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

    /// Smoke: materializing the root dir against a temp cache dir
    /// with a stub nix-portable produces a tree containing the init
    /// script, the vendored busybox, the stub nix-portable, and the
    /// busybox applet symlinks.
    #[test]
    fn materialize_root_dir_against_stub_cache() {
        let dir = TempDir::new().unwrap();
        // The module's `stage0_cache_dir()` reads from $HOME; swap.
        // SAFETY: this test mutates env vars but isn't expected to
        // race with the others in this module (none touch HOME).
        let saved = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let real_cache = stage0_cache_dir();
        std::fs::create_dir_all(&real_cache).unwrap();
        std::fs::write(real_cache.join("nix-portable"), b"FAKE_NIX_PORTABLE").unwrap();

        let root = dir.path().join("stage0-root");
        materialize_root_dir(&root).expect("materialize succeeds with stubs in place");

        // Restore HOME.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        // Expected layout.
        assert!(root.join("init").is_file(), "/init present");
        assert!(root.join("bin").join("busybox").is_file(), "busybox present");
        assert!(
            root.join("usr/local/bin/nix-portable").is_file(),
            "nix-portable copied"
        );
        for stub in ROOT_DIR_STUBS {
            assert!(root.join(stub).is_dir(), "stub dir {stub} present");
        }
        // /sbin → bin symlink.
        let sbin = root.join("sbin");
        assert!(
            sbin.symlink_metadata().unwrap().file_type().is_symlink(),
            "/sbin is a symlink"
        );
        assert_eq!(std::fs::read_link(&sbin).unwrap(), std::path::Path::new("bin"));
        // Applet symlinks.
        for applet in PRE_INSTALLED_APPLETS {
            let p = root.join("bin").join(applet);
            assert!(
                p.symlink_metadata().unwrap().file_type().is_symlink(),
                "/bin/{applet} is a symlink"
            );
            assert_eq!(std::fs::read_link(&p).unwrap(), std::path::Path::new("busybox"));
        }

        // Permissions: /init and /bin/busybox executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let init_mode = std::fs::metadata(root.join("init")).unwrap().permissions().mode();
            assert_eq!(init_mode & 0o777, 0o755, "/init mode 0755");
            let bb_mode = std::fs::metadata(root.join("bin/busybox"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(bb_mode & 0o777, 0o755, "/bin/busybox mode 0755");
            let np_mode = std::fs::metadata(root.join("usr/local/bin/nix-portable"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(np_mode & 0o777, 0o755, "/usr/local/bin/nix-portable mode 0755");
        }

        // The script in /init is byte-identical to the embedded
        // INIT_SCRIPT — guards against silent corruption.
        let on_disk = std::fs::read_to_string(root.join("init")).unwrap();
        assert_eq!(on_disk, INIT_SCRIPT);

        let _ = std::fs::remove_dir_all(real_cache);
    }

    /// Materializing without first calling `prepare_assets` (nix-portable
    /// missing from cache) is rejected with a clear error.
    #[test]
    fn materialize_root_dir_rejects_missing_nix_portable() {
        let dir = TempDir::new().unwrap();
        let saved = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let real_cache = stage0_cache_dir();
        // Do NOT create nix-portable.
        std::fs::create_dir_all(&real_cache).unwrap();

        let root = dir.path().join("stage0-root");
        let err = materialize_root_dir(&root).expect_err("missing asset should fail");

        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        let msg = format!("{err:#}");
        assert!(
            msg.contains("nix-portable"),
            "error names the missing asset: {msg}"
        );

        let _ = std::fs::remove_dir_all(real_cache);
    }
}
