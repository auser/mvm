//! `cargo xtask build-dev-image` — build the dev VM image via Nix and drop
//! the outputs into the vendored slot at
//! `nix/images/dev-prebuilt/<arch>/{vmlinux, rootfs.ext4}`.
//!
//! That slot is the highest-precedence source in
//! `mvm_cli::commands::env::apple_container::find_vendored_dev_image` — when
//! the binary runs from a source checkout that has those files, it boots
//! from them directly and skips the GitHub-release download. So populating
//! the slot is what flips `mvmctl dev up` from "downloading prebuilt"
//! (currently a 404 against `tinylabscom/mvm` v0.14.0) to "using vendored
//! dev image from source checkout".
//!
//! ## Contract
//!
//! Mirrors the asset-shape produced by `.github/workflows/release.yml`'s
//! `dev-image` job: each invocation produces an `<arch>` sibling under
//! `nix/images/dev-prebuilt/` with:
//!
//! - `vmlinux` — the kernel image.
//! - `rootfs.ext4` — the ext4 root filesystem.
//! - `checksums-sha256.txt` — SHA-256 of both files, in the same
//!   `<hash>  <name>` format that `sha256sum` and
//!   `mvm-security::image_verify::verify_unsigned_checksums` parse.
//!
//! ## Prerequisites
//!
//! The xtask shells out to `nix build`; the host must have Nix on PATH
//! and able to build the requested Linux system. On native Linux that's
//! the kernel's KVM; on macOS that's either a remote builder via
//! `builders` in `/etc/nix/nix.conf` or `nix-darwin`'s `linux-builder`
//! NixOS module. Failure to build is propagated with the underlying
//! Nix stderr — the xtask never silently substitutes a stale or
//! prebuilt artifact.
//!
//! The expected flake is at `nix/images/builder/flake.nix` and exposes
//! `packages.<system>.default` as a derivation whose `$out` directory
//! contains `vmlinux` and `rootfs.ext4`. If the flake is missing the
//! xtask fails fast with a pointer at the git history (commit `20f776e`
//! has the historical version) so callers know exactly what to restore
//! before retrying.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Parse `cargo xtask build-dev-image [--arch <arch>]` and dispatch.
pub fn run(args: &[String], workspace: &Path) -> Result<()> {
    let mut arch: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--arch" => {
                arch = iter.next().cloned();
            }
            "--help" | "-h" => {
                println!("{HELP_TEXT}");
                return Ok(());
            }
            other => anyhow::bail!("Unknown argument to build-dev-image: {other:?}. Try --help."),
        }
    }

    let arch = arch.unwrap_or_else(|| host_arch_for_linux().to_string());
    validate_arch(&arch)?;
    build_and_install(workspace, &arch)
}

const HELP_TEXT: &str = "\
Usage: cargo xtask build-dev-image [--arch <arch>]

Builds the dev VM image via `nix build ./nix/images/builder#packages.<arch>-linux.default`
and copies vmlinux + rootfs.ext4 + checksums into nix/images/dev-prebuilt/<arch>/.

Args:
  --arch <arch>   Target architecture (aarch64 or x86_64).
                  Default: the host architecture mapped to its linux variant
                  (aarch64-darwin → aarch64, x86_64-linux → x86_64, etc.).

Prerequisites:
  - `nix` on PATH with flakes enabled.
  - A working builder for the target Linux system (native KVM on Linux,
    remote builder or nix-darwin linux-builder on macOS).
  - The flake at nix/images/builder/flake.nix exposes
    packages.<arch>-linux.default with vmlinux + rootfs.ext4 in $out.
";

/// Map the host arch to the matching Linux-system identifier used by
/// `mvmctl`'s download path (`download_dev_image` uses the same mapping
/// at `apple_container.rs`). Defaults are "aarch64" on Apple Silicon /
/// aarch64-linux and "x86_64" everywhere else — mirrors the
/// `cfg!(target_arch = "aarch64")` test there.
fn host_arch_for_linux() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn validate_arch(arch: &str) -> Result<()> {
    if !matches!(arch, "aarch64" | "x86_64") {
        anyhow::bail!("Unsupported --arch {arch:?}. Supported: aarch64, x86_64.");
    }
    Ok(())
}

fn build_and_install(workspace: &Path, arch: &str) -> Result<()> {
    let builder_flake = workspace.join("nix").join("images").join("builder");
    let flake_nix = builder_flake.join("flake.nix");
    if !flake_nix.exists() {
        anyhow::bail!(
            "Builder flake not found at {}.\n\n\
             The flake existed historically at git commit 20f776e; restore it via\n\
             `git show 20f776e:nix/images/builder/flake.nix > {}`\n\
             (adapt inputs as needed — the historical version references a\n\
             nix/dev/ sibling that no longer exists in the tree).",
            flake_nix.display(),
            flake_nix.display(),
        );
    }

    if !nix_on_path() {
        anyhow::bail!(
            "`nix` not found on PATH. Install Nix from https://nixos.org/download \
             and re-run."
        );
    }

    let attr = format!("packages.{arch}-linux.default");
    let flake_ref = format!("{}#{attr}", builder_flake.display());

    println!("xtask build-dev-image: nix build {flake_ref}");
    let store_path = run_nix_build(&flake_ref)?;
    let store_path = Path::new(&store_path);

    let src_kernel = store_path.join("vmlinux");
    let src_rootfs = store_path.join("rootfs.ext4");
    if !src_kernel.is_file() {
        anyhow::bail!(
            "nix build succeeded but {} is missing — does the flake's \
             packages.{arch}-linux.default output expose a vmlinux file in \
             its $out directory?",
            src_kernel.display(),
        );
    }
    if !src_rootfs.is_file() {
        anyhow::bail!(
            "nix build succeeded but {} is missing — does the flake's \
             packages.{arch}-linux.default output expose a rootfs.ext4 file \
             in its $out directory?",
            src_rootfs.display(),
        );
    }

    let dest_dir = vendored_slot(workspace, arch);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating vendored slot at {}", dest_dir.display()))?;

    let dest_kernel = dest_dir.join("vmlinux");
    let dest_rootfs = dest_dir.join("rootfs.ext4");
    copy_with_mode(&src_kernel, &dest_kernel)?;
    copy_with_mode(&src_rootfs, &dest_rootfs)?;
    let checksums = format!(
        "{}  vmlinux\n{}  rootfs.ext4\n",
        sha256_hex(&dest_kernel)?,
        sha256_hex(&dest_rootfs)?,
    );
    let checksums_path = dest_dir.join("checksums-sha256.txt");
    std::fs::write(&checksums_path, checksums)
        .with_context(|| format!("writing {}", checksums_path.display()))?;

    println!("\nVendored dev image installed:");
    println!("  {}", dest_kernel.display());
    println!("  {}", dest_rootfs.display());
    println!("  {}", checksums_path.display());
    println!(
        "\n`mvmctl dev up` from this checkout will now boot from the vendored\n\
         slot instead of downloading from the release page."
    );
    Ok(())
}

fn nix_on_path() -> bool {
    std::process::Command::new("nix")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `nix build <flake_ref> --no-link --print-out-paths` and return the
/// resolved store path of the first output line. Nix's stderr is
/// inherited so the operator sees per-derivation build progress in real
/// time — capturing it would silently swallow a 30-minute toolchain
/// rebuild's status updates.
fn run_nix_build(flake_ref: &str) -> Result<String> {
    let output = std::process::Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            flake_ref,
            "--no-link",
            "--print-out-paths",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("spawning `nix build {flake_ref}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`nix build {flake_ref}` failed with exit {}. See stderr above.",
            output.status.code().unwrap_or(-1)
        );
    }
    let stdout = String::from_utf8(output.stdout).context("nix build emitted non-UTF-8 stdout")?;
    let store_path = stdout
        .lines()
        .find(|l| l.starts_with("/nix/store/"))
        .ok_or_else(|| {
            anyhow::anyhow!("nix build produced no /nix/store/... output path (stdout: {stdout:?})")
        })?
        .trim()
        .to_string();
    Ok(store_path)
}

/// Copy a file, then chmod the destination to 0644 so the operator can
/// re-run the xtask without `rm`-ing read-only Nix store copies. Nix
/// store files are mode 0444; `std::fs::copy` preserves source mode by
/// default, which would make a subsequent overwrite EACCES.
fn copy_with_mode(src: &Path, dest: &Path) -> Result<()> {
    std::fs::copy(src, dest)
        .with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("chmod 0644 on {}", dest.display()))?;
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {} during hash", path.display()))?;
        if n == 0 {
            break;
        }
        use sha2::Digest;
        hasher.update(&buf[..n]);
    }
    use sha2::Digest;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Resolve the path the workspace was built from. Lifted out so tests
/// can target a tempdir without setting `CARGO_MANIFEST_DIR`.
pub fn vendored_slot(workspace: &Path, arch: &str) -> PathBuf {
    workspace
        .join("nix")
        .join("images")
        .join("dev-prebuilt")
        .join(arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_arch_accepts_supported() {
        assert!(validate_arch("aarch64").is_ok());
        assert!(validate_arch("x86_64").is_ok());
    }

    #[test]
    fn validate_arch_rejects_others() {
        assert!(validate_arch("riscv64").is_err());
        assert!(validate_arch("").is_err());
        // Guard against accidentally accepting full system identifiers —
        // the flake attribute we emit assumes the caller passes the
        // bare arch and we append `-linux`.
        assert!(validate_arch("aarch64-linux").is_err());
    }

    #[test]
    fn host_arch_is_one_of_the_supported() {
        let arch = host_arch_for_linux();
        assert!(validate_arch(arch).is_ok());
    }

    #[test]
    fn vendored_slot_resolves_under_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let slot = vendored_slot(tmp.path(), "aarch64");
        assert!(slot.starts_with(tmp.path()));
        assert!(slot.ends_with("nix/images/dev-prebuilt/aarch64"));
    }

    #[test]
    fn build_fails_with_clear_message_when_flake_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = build_and_install(tmp.path(), "aarch64").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Builder flake not found"),
            "expected a clear flake-missing error, got: {msg}"
        );
        assert!(
            msg.contains("20f776e"),
            "expected the historical-recovery pointer, got: {msg}"
        );
    }

    #[test]
    fn help_flag_short_circuits() {
        // --help should not require a workspace state — should print and exit Ok.
        let tmp = tempfile::tempdir().unwrap();
        run(&["--help".to_string()], tmp.path()).expect("help should be Ok");
    }
}
