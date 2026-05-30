/// Integration test: run the full `cargo xtask check-mvm-host-binaries-sync`
/// subprocess and assert it exits 0. This catches regressions where the
/// xtask compiles but the two manifests have drifted.
#[test]
fn xtask_check_sync_passes_on_main() {
    let status = std::process::Command::new("cargo")
        .args(["xtask", "check-mvm-host-binaries-sync"])
        .status()
        .expect("spawn cargo xtask");
    assert!(
        status.success(),
        "xtask reported a sync drift between Rust manifest and Nix attrset"
    );
}
