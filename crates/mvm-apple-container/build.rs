use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Declare custom cfg for check-cfg lint
    println!("cargo::rustc-check-cfg=cfg(apple_container_stub)");

    // Only build Swift bridge on macOS ARM64
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    if target_os != "macos" || target_arch != "aarch64" {
        // Emit a cfg flag so lib.rs can conditionally compile
        println!("cargo:rustc-cfg=apple_container_stub");
        return;
    }

    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
    let swift_dir = PathBuf::from(manifest_dir).join("swift");

    // Check if pre-built library exists; if not, build it
    let lib_path = swift_dir
        .join(".build")
        .join("arm64-apple-macosx")
        .join("release")
        .join("libMvmContainerBridge.a");

    if !lib_path.exists() {
        eprintln!("Building Swift bridge (first time only, ~2min)...");
        let status = Command::new("swift")
            .args(["build", "--configuration", "release"])
            .current_dir(&swift_dir)
            .status()
            .expect("Failed to run swift build. Is Swift installed?");

        if !status.success() {
            // Fall back to stub mode if Swift build fails
            eprintln!("Swift build failed — using stub mode");
            println!("cargo:rustc-cfg=apple_container_stub");
            return;
        }
    }

    let lib_dir = lib_path
        .parent()
        .expect("library path must have a parent directory");

    // Link the static library
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=MvmContainerBridge");

    // Link Swift runtime and system frameworks
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Virtualization");
    println!("cargo:rustc-link-lib=framework=vmnet");

    // System libraries needed by Containerization framework dependencies
    println!("cargo:rustc-link-lib=archive"); // libarchive for ContainerizationArchive
    println!("cargo:rustc-link-lib=z"); // zlib

    // Swift runtime libraries
    let sdk_output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun --show-sdk-path failed");
    let sdk_path = String::from_utf8(sdk_output.stdout)
        .expect("SDK path must be valid UTF-8")
        .trim()
        .to_string();

    let toolchain_output = Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .expect("xcrun --find swiftc failed");
    let swiftc_path = String::from_utf8(toolchain_output.stdout)
        .expect("swiftc path must be valid UTF-8")
        .trim()
        .to_string();
    let swiftc_bin = PathBuf::from(swiftc_path);
    let toolchain_lib = swiftc_bin
        .parent()
        .expect("swiftc must have parent dir")
        .parent()
        .expect("swiftc bin must have grandparent dir")
        .join("lib")
        .join("swift")
        .join("macosx");

    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-search=native={}/usr/lib/swift", sdk_path);

    // Add rpath for Swift runtime libraries (needed by test binaries)
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        toolchain_lib.display()
    );
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Only rebuild if Swift source changes
    println!("cargo:rerun-if-changed=swift/Sources/");
    println!("cargo:rerun-if-changed=swift/Package.swift");
}
