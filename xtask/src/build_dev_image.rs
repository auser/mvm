//! `cargo xtask build-dev-image` — build the dev VM image and drop it
//! into the source-checkout vendored slot at
//! `nix/images/dev-prebuilt/<arch>/{vmlinux, rootfs.ext4,
//! checksums-sha256.txt}`.
//!
//! ## Self-bootstrapping — no host Nix required
//!
//! The task does **not** shell out to a host `nix` binary. Instead it
//! drives [`mvm_build::builder_vm::MicrosandboxBuilderVm`] — the same
//! microsandbox-backed Linux builder that `mvmctl build` uses to build
//! user microVM images. That sandbox spawns `docker.io/nixos/nix:2.24.10`
//! via Apple Virtualization Framework / libkrun (macOS) or KVM (Linux),
//! bind-mounts the workspace at `/work`, and runs `nix build` *inside*
//! the sandbox. The host needs zero Nix install — `mvmctl` ships
//! microsandbox in-binary and microsandbox pulls the public
//! `nixos/nix:2.24.10` image once.
//!
//! Net effect: on a fresh macOS 26+ Apple Silicon host with nothing
//! installed but `mvmctl`, `cargo xtask build-dev-image` produces a
//! working dev VM image. After it runs once, the image lives in the
//! vendored slot and `mvmctl dev up` boots from there at the highest-
//! precedence layer of `ensure_dev_image`'s cascade — so subsequent
//! starts don't even need microsandbox or network reachability.
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

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use mvm_build::builder_vm::{
    BUILDER_GUEST_WORK_DIR, BuilderJob, BuilderMounts, BuilderVm, MicrosandboxBuilderVm,
};

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

Builds the dev VM image inside a microsandbox-managed Linux sandbox
(via Apple Virtualization Framework / libkrun on macOS, KVM on Linux)
and copies vmlinux + rootfs.ext4 + checksums into
nix/images/dev-prebuilt/<arch>/. No host Nix install required.

Args:
  --arch <arch>   Target architecture (aarch64 or x86_64).
                  Default: the host architecture mapped to its linux variant
                  (aarch64-darwin → aarch64, x86_64-linux → x86_64, etc.).

Prerequisites:
  - macOS 26+ on Apple Silicon, or Linux with KVM.
  - Network reachability to docker.io (first run pulls nixos/nix:2.24.10;
    subsequent runs are cached).
  - The flake at nix/images/builder/flake.nix exposes
    packages.<arch>-linux.default with vmlinux + rootfs.ext4 in $out.
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
    let flake_nix = workspace
        .join("nix")
        .join("images")
        .join("builder")
        .join("flake.nix");
    if !flake_nix.exists() {
        anyhow::bail!(
            "Builder flake not found at {}.\n\n\
             The flake should be checked into the source tree at\n\
             nix/images/builder/flake.nix and expose\n\
             packages.<arch>-linux.default producing vmlinux + rootfs.ext4.",
            flake_nix.display(),
        );
    }

    // The builder runs `nix build` inside a read-only bind of the
    // workspace, so a stale `flake.lock` with `path:..`-style entries
    // is unrepresentable: nix can't write a new lock (EROFS) and the
    // old one trips strict pure-mode validation. Pair this with the
    // `--no-write-lock-file --impure` flags inside the builder
    // (mvm-build/src/builder_vm.rs:run_build_async). Net: every xtask
    // run re-evaluates inputs from scratch — fine for the bootstrap,
    // and the kernel + nixpkgs revs are still pinned through the
    // parent flake's lock at `nix/flake.lock`.
    let lock = flake_nix.parent().unwrap().join("flake.lock");
    if lock.exists() {
        std::fs::remove_file(&lock)
            .with_context(|| format!("removing stale {}", lock.display()))?;
    }

    // The sandbox bind-mounts `workspace` at /work (read-only).
    // microsandbox owns /out for artifact extraction. The flake_ref
    // inside the sandbox is the absolute path to the builder flake
    // under that mount — `path:..` resolves there to the parent
    // mvm flake at /work/nix/, which in turn resolves its own
    // `mvm-workspace = path:..` to /work/, where `Cargo.lock` lives.
    let artifact_out =
        tempfile::tempdir().context("creating tempdir for builder artifact extraction")?;
    let job = BuilderJob {
        // Bare path — nix auto-detects /work as a git repo (the host
        // workspace's `.git` is in the bind-mount) and uses the
        // git+file fetcher. That gives us two things `path:` doesn't:
        //   1. The flake resolves against the workspace root, so
        //      `path:../..`-style relative inputs find their
        //      neighbours (`path:` stages only the leaf subdir and
        //      escapes the store with "outside of its parent's store
        //      path").
        //   2. `?dir=` works in older nix versions (the `path:`
        //      variant errors "unsupported parameter 'dir'" on nix
        //      2.24, which is what the builder image ships).
        //
        // `git config --global --add safe.directory '*'` inside the
        // sandbox (set by run_build_async's build_script) is what
        // makes the git fetcher work across the bind-mount's
        // host-uid ownership; without it, git refuses with
        // "repository '/work' is not owned by current user".
        flake_ref: format!("git+file://{BUILDER_GUEST_WORK_DIR}?dir=nix/images/builder"),
        attr_path: format!("packages.{arch}-linux.default"),
    };
    let mounts = BuilderMounts {
        flake_src: workspace.to_path_buf(),
        // Never bind the host /nix on macOS: it's root-owned and
        // contains Darwin-targeted closures the Linux sandbox can't
        // execute. See the same reasoning in
        // `apple_container.rs::build_image_via_microsandbox`.
        host_nix_store: None,
        artifact_out: artifact_out.path().to_path_buf(),
    };

    println!(
        "xtask build-dev-image: running mvm-build's MicrosandboxBuilderVm\n\
         (no host Nix needed — `nixos/nix:2.24.10` runs inside the sandbox)"
    );
    let builder = MicrosandboxBuilderVm::default();
    let artifacts = builder
        .run_build(&job, &mounts)
        .map_err(|e| anyhow::anyhow!("microsandbox builder failed: {e}"))?;

    let src_kernel = artifacts.kernel_path.ok_or_else(|| {
        anyhow::anyhow!(
            "builder produced no vmlinux — the flake's \
             packages.{arch}-linux.default output must include `vmlinux` \
             in its $out directory"
        )
    })?;
    if !artifacts.rootfs_path.is_file() {
        anyhow::bail!(
            "builder reported success but rootfs.ext4 is missing at {}",
            artifacts.rootfs_path.display(),
        );
    }

    let dest_dir = vendored_slot(workspace, arch);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating vendored slot at {}", dest_dir.display()))?;
    let dest_kernel = dest_dir.join("vmlinux");
    let dest_rootfs = dest_dir.join("rootfs.ext4");
    copy_with_mode(&src_kernel, &dest_kernel)?;
    copy_with_mode(&artifacts.rootfs_path, &dest_rootfs)?;
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
         slot at highest precedence — no GitHub release download needed."
    );
    Ok(())
}

/// Copy a file, then chmod the destination to 0644. Nix store files
/// come back mode 0444; preserving that source mode would make a
/// subsequent xtask re-run EACCES on overwrite.
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
    use sha2::Digest;
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
        hasher.update(&buf[..n]);
    }
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
    }

    #[test]
    fn help_flag_short_circuits() {
        let tmp = tempfile::tempdir().unwrap();
        run(&["--help".to_string()], tmp.path()).expect("help should be Ok");
    }
}
