use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let workspace_cargo_toml = workspace_root.join("Cargo.toml");

    let toml_str =
        std::fs::read_to_string(&workspace_cargo_toml).expect("read workspace Cargo.toml");
    let parsed: toml::Value = toml::from_str(&toml_str).expect("parse workspace Cargo.toml");
    let pin = &parsed["workspace"]["metadata"]["mvm"]["toolchain"];

    let zig = pin["zig"].as_str().expect("zig pin missing");
    let zb = pin["cargo-zigbuild"]
        .as_str()
        .expect("cargo-zigbuild pin missing");
    let tgt = pin["target"].as_str().expect("target pin missing");

    println!("cargo:rustc-env=MVM_PINNED_ZIG={zig}");
    println!("cargo:rustc-env=MVM_PINNED_CARGO_ZIGBUILD={zb}");
    println!("cargo:rustc-env=MVM_PINNED_TARGET={tgt}");

    println!("cargo:rerun-if-changed={}", workspace_cargo_toml.display());
}
