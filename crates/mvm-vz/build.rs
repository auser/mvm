//! Plan 97 Phase B — auto-build the `mvm-vz-supervisor` Swift binary
//! during `cargo build` on macOS so contributors don't have to run
//! `tools/build.sh` manually before `mvmctl run --backend vz`.
//!
//! Skipped on non-macOS hosts and when the Swift toolchain is
//! unavailable — both produce a warning via `cargo:warning=...` rather
//! than failing the build, so Linux contributors and macOS hosts
//! without Xcode CLT can still build the workspace.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Whenever any source file in the sibling Swift package changes,
    // re-run this script. The package layout is small (Package.swift +
    // Sources/<one-target>/*.swift + tools/build.sh + Entitlements.plist),
    // so emitting rerun-if-changed on the package root is precise
    // enough — cargo recurses for us when the root mtime moves.
    let supervisor_root = supervisor_package_root();
    println!(
        "cargo:rerun-if-changed={}",
        supervisor_root.join("Package.swift").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        supervisor_root.join("Sources").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        supervisor_root.join("tools/build.sh").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        supervisor_root.join("Entitlements.plist").display()
    );

    // The `MVM_VZ_SKIP_SUPERVISOR_BUILD` escape hatch lets CI / users
    // turn off the Swift build (e.g. when working only on the Rust
    // side from a macOS contributor host without Xcode CLT installed).
    // Off-by-default — the common case is to build everything.
    println!("cargo:rerun-if-env-changed=MVM_VZ_SKIP_SUPERVISOR_BUILD");
    if std::env::var_os("MVM_VZ_SKIP_SUPERVISOR_BUILD").is_some() {
        println!(
            "cargo:warning=mvm-vz: MVM_VZ_SKIP_SUPERVISOR_BUILD set; skipping Swift supervisor build"
        );
        return;
    }

    // Only macOS can host the Vz backend; on every other target the
    // supervisor is irrelevant. Skip silently — the Rust side of this
    // crate (`SupervisorConfig` types, path helpers) still compiles
    // and gets used by `mvm-backend` for shape checks.
    if !cfg!(target_os = "macos") {
        return;
    }

    // If the host has no Swift toolchain (no Xcode CLT, no
    // standalone `swift` install), emit a warning instead of failing
    // — a contributor working on the Rust side should still be able
    // to `cargo build` the workspace.
    if !swift_available() {
        println!(
            "cargo:warning=mvm-vz: `swift` not found on PATH; skipping mvm-vz-supervisor build. \
             Install Xcode Command Line Tools (`xcode-select --install`) and rebuild to enable \
             `mvmctl run --backend vz`."
        );
        return;
    }

    // Cargo populates `PROFILE` with `debug` or `release`. Mirror it
    // into the Swift build so we don't ship a debug Swift binary
    // alongside a release Rust binary.
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let build_script = supervisor_root.join("tools/build.sh");

    println!(
        "cargo:warning=mvm-vz: building Swift supervisor ({profile}) via {}",
        build_script.display()
    );

    let status = Command::new(&build_script)
        .arg(&profile)
        .current_dir(&supervisor_root)
        .status();

    match status {
        Ok(s) if s.success() => {
            // tools/build.sh prints the signed binary path on stdout;
            // surface that to cargo so it appears in build output.
            println!("cargo:warning=mvm-vz: Swift supervisor build OK");
        }
        Ok(s) => {
            // Non-zero exit: warn loudly but don't fail the build.
            // The `VzBackend` resolver will refuse to start a VM
            // when the binary is missing, with a clear actionable
            // error pointing at `tools/build.sh`. A `cargo build`
            // failure here would block contributors who don't intend
            // to use the Vz backend at all.
            println!(
                "cargo:warning=mvm-vz: Swift supervisor build exited with {s}; \
                 run `crates/mvm-vz-supervisor/tools/build.sh` manually to diagnose"
            );
        }
        Err(e) => {
            println!(
                "cargo:warning=mvm-vz: failed to spawn {}: {e}",
                build_script.display()
            );
        }
    }
}

/// Resolve the absolute path to `crates/mvm-vz-supervisor/`. We are
/// `crates/mvm-vz/build.rs`, so the supervisor package is a sibling.
fn supervisor_package_root() -> PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    PathBuf::from(manifest_dir)
        .parent()
        .expect("crates/mvm-vz has a parent")
        .join("mvm-vz-supervisor")
}

/// `which swift` — without pulling in a `which` crate as a
/// build-script dependency. Probes the same PATH the build script
/// will see when it shells out to `tools/build.sh`.
fn swift_available() -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("swift");
        if candidate.is_file() {
            return true;
        }
    }
    false
}
