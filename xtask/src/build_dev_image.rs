//! `cargo xtask build-dev-image` — build the dev VM image and drop it
//! into the source-checkout vendored slot at
//! `nix/images/dev-prebuilt/<arch>/{vmlinux, rootfs.ext4,
//! checksums-sha256.txt}`.
//!
//! ## Status
//!
//! The legacy implementation drove the upstream Rust microsandbox crate.
//! That crate vendors SeaORM / SQLx database dependencies, so the path is
//! intentionally disabled until the libkrun builder can own this xtask.
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
//! That contract is consumed by
//! `mvm_cli::commands::env::apple_container::find_vendored_dev_image`,
//! which is the highest-precedence path in `ensure_dev_image` for
//! source-checkout users.

use anyhow::Result;
use std::path::Path;

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

Builds the dev VM image into nix/images/dev-prebuilt/<arch>/.

This path is currently disabled because its legacy microsandbox
implementation pulled SeaORM / SQLx database crates into the workspace.

Args:
  --arch <arch>   Target architecture (aarch64 or x86_64).
                  Default: the host architecture mapped to its linux variant
                  (aarch64-darwin → aarch64, x86_64-linux → x86_64, etc.).

Prerequisites:
  - A replacement libkrun-backed implementation.
";

/// Map the host arch to the matching Linux-system identifier used by
/// `mvmctl`'s download path (`download_dev_image` uses the same mapping
/// at `apple_container.rs`).
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
    let _ = (workspace, arch);
    anyhow::bail!(
        "xtask build-dev-image is disabled until it is ported from the removed \
         microsandbox builder to the libkrun builder; the old path pulled SeaORM \
         / SQLx database crates"
    )
}

/// Resolve the path the workspace was built from. Lifted out so tests
/// can target a tempdir without setting `CARGO_MANIFEST_DIR`.
#[cfg(test)]
pub fn vendored_slot(workspace: &Path, arch: &str) -> std::path::PathBuf {
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
            msg.contains("xtask build-dev-image is disabled"),
            "expected a clear disabled-path error, got: {msg}"
        );
    }

    #[test]
    fn help_flag_short_circuits() {
        let tmp = tempfile::tempdir().unwrap();
        run(&["--help".to_string()], tmp.path()).expect("help should be Ok");
    }
}
