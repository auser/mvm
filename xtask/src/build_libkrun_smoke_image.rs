//! `cargo xtask build-libkrun-smoke-image` — build the Plan 57 W3
//! examples/minimal flake into `examples/minimal/result/{vmlinux,
//! rootfs.ext4}`.
//!
//! Same shape as [`crate::build_dev_image`]: drives
//! [`mvm_build::builder_vm::MicrosandboxBuilderVm`] inside
//! `nixos/nix:2.24.10`, no host Nix required. The flake at
//! `examples/minimal/flake.nix` is the smallest-thing-that-boots
//! image — busybox + a static `vsock_ok` C binary, ~tens of MiB,
//! sized to fit inside microsandbox's 4 GiB overlay (unlike the
//! full dev-image, which carries rustc + a ~480-crate cargo
//! closure and overflows that overlay during evaluation).
//!
//! After the xtask runs, `cargo run --example libkrun-smoke
//! --features libkrun-sys` picks up the artifacts from the
//! conventional `result/` path and boots them under libkrun.

use anyhow::{Context, Result};
use std::path::Path;

use mvm_build::builder_vm::{
    BUILDER_GUEST_WORK_DIR, BuilderJob, BuilderMounts, BuilderVm, MicrosandboxBuilderVm,
};

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
            other => anyhow::bail!(
                "Unknown argument to build-libkrun-smoke-image: {other:?}. Try --help."
            ),
        }
    }

    let arch = arch.unwrap_or_else(|| host_arch_for_linux().to_string());
    validate_arch(&arch)?;
    build_and_install(workspace, &arch)
}

const HELP_TEXT: &str = "\
Usage: cargo xtask build-libkrun-smoke-image [--arch <arch>]

Builds the Plan 57 W3 smoke image (examples/minimal/flake.nix) inside
a microsandbox-managed Linux sandbox and copies vmlinux + rootfs.ext4
into examples/minimal/result/. No host Nix install required.

Args:
  --arch <arch>   Target architecture (aarch64 or x86_64).
                  Default: the host architecture mapped to its linux variant
                  (aarch64-darwin → aarch64, x86_64-linux → x86_64, etc.).

Prerequisites:
  - macOS 26+ on Apple Silicon, or Linux with KVM.
  - Network reachability to docker.io (first run pulls nixos/nix:2.24.10).

After it succeeds:
  cargo run --example libkrun-smoke --features libkrun-sys
";

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
    let flake_dir = workspace.join("examples").join("minimal");
    let flake_nix = flake_dir.join("flake.nix");
    if !flake_nix.exists() {
        anyhow::bail!(
            "Smoke-image flake not found at {}.\n\n\
             The flake should be checked in at examples/minimal/flake.nix \
             and expose packages.<arch>-linux.default producing vmlinux + \
             rootfs.ext4 in $out.",
            flake_nix.display(),
        );
    }

    // Same reasoning as build_dev_image: the sandbox bind-mounts the
    // workspace read-only, so a stale flake.lock with path:..-style
    // entries would trip strict pure-mode validation. Wipe it before
    // each run; the kernel/nixpkgs pins still resolve through the
    // input registry's hash.
    let lock = flake_dir.join("flake.lock");
    if lock.exists() {
        std::fs::remove_file(&lock)
            .with_context(|| format!("removing stale {}", lock.display()))?;
    }

    let artifact_out =
        tempfile::tempdir().context("creating tempdir for builder artifact extraction")?;
    let job = BuilderJob {
        // git+file://<sandbox-workspace>?dir=examples/minimal — same
        // pattern as build_dev_image; the sandbox sees the workspace
        // as a git repo via the bind-mounted .git directory, which
        // satisfies nix's strict pure-mode flake resolution.
        flake_ref: format!("git+file://{BUILDER_GUEST_WORK_DIR}?dir=examples/minimal"),
        attr_path: format!("packages.{arch}-linux.default"),
    };
    let mounts = BuilderMounts {
        flake_src: workspace.to_path_buf(),
        host_nix_store: None,
        artifact_out: artifact_out.path().to_path_buf(),
    };

    println!(
        "xtask build-libkrun-smoke-image: running mvm-build's MicrosandboxBuilderVm\n\
         (no host Nix needed — `nixos/nix:2.24.10` runs inside the sandbox)"
    );
    let builder = MicrosandboxBuilderVm::default();
    let artifacts = builder
        .run_build(&job, &mounts)
        .map_err(|e| anyhow::anyhow!("microsandbox builder failed: {e}"))?;

    let src_kernel = artifacts.kernel_path.ok_or_else(|| {
        anyhow::anyhow!(
            "smoke-image flake produced no vmlinux — \
             packages.{arch}-linux.default must put vmlinux into $out"
        )
    })?;
    if !artifacts.rootfs_path.is_file() {
        anyhow::bail!(
            "builder reported success but rootfs.ext4 is missing at {}",
            artifacts.rootfs_path.display(),
        );
    }

    // Drop the artifacts at `examples/minimal/result/` so they line up
    // with the `nix build` symlink convention. The smoke example's
    // default path probes that same directory.
    let dest_dir = flake_dir.join("result");
    // If a previous `nix build` left a symlink at this path, replace
    // it with a real directory we own. `create_dir_all` won't replace
    // a symlink; remove it explicitly first.
    if dest_dir.exists() || dest_dir.symlink_metadata().is_ok() {
        let _ = std::fs::remove_dir_all(&dest_dir);
        let _ = std::fs::remove_file(&dest_dir);
    }
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let dest_kernel = dest_dir.join("vmlinux");
    let dest_rootfs = dest_dir.join("rootfs.ext4");
    copy_with_mode(&src_kernel, &dest_kernel)?;
    copy_with_mode(&artifacts.rootfs_path, &dest_rootfs)?;

    println!("\nSmoke image installed:");
    println!("  {}", dest_kernel.display());
    println!("  {}", dest_rootfs.display());
    println!("\nRun the smoke test:");
    println!("  cargo run --example libkrun-smoke --features libkrun-sys");
    Ok(())
}

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
    fn build_fails_with_clear_message_when_flake_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = build_and_install(tmp.path(), "aarch64").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Smoke-image flake not found"),
            "expected a clear flake-missing error, got: {msg}"
        );
    }

    #[test]
    fn help_flag_short_circuits() {
        let tmp = tempfile::tempdir().unwrap();
        run(&["--help".to_string()], tmp.path()).expect("help should be Ok");
    }
}
