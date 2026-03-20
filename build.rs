fn main() {
    // On macOS, add rpath for Swift runtime libraries so the final
    // mvmctl binary can find libswift_Concurrency.dylib at runtime.
    // This is needed because mvm-apple-container links a Swift static
    // library that depends on Swift runtime dylibs.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "macos" {
        if let Ok(output) = std::process::Command::new("xcrun")
            .args(["--find", "swiftc"])
            .output()
        {
            if let Ok(swiftc) = String::from_utf8(output.stdout) {
                let swiftc = swiftc.trim();
                if let Some(bin_dir) = std::path::Path::new(swiftc).parent() {
                    if let Some(toolchain_dir) = bin_dir.parent() {
                        let swift_lib = toolchain_dir.join("lib").join("swift").join("macosx");
                        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", swift_lib.display());
                    }
                }
            }
        }
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
