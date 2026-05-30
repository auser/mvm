use mvm_cli::doctor::{ZigbuildProbe, probe_zigbuild};

#[test]
fn probe_reports_pinned_versions_from_workspace_metadata() {
    let probe = probe_zigbuild();
    // Pinned versions live in Cargo.toml under
    // [workspace.metadata.mvm.toolchain]. The probe surfaces them
    // so a contributor can compare against what `zig --version`
    // reports.
    assert!(!probe.pinned_zig.is_empty(), "pinned zig version missing");
    assert!(
        !probe.pinned_cargo_zigbuild.is_empty(),
        "pinned cargo-zigbuild version missing"
    );
}
