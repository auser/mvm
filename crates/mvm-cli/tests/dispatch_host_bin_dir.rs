/// Integration test for Plan 115 / ADR-064 T7:
/// ensure_extracted populates the host-bin dir that dispatch wires into
/// BuilderMounts.host_bin_dir before launching the builder VM.
#[test]
fn dispatch_populates_host_bin_dir_before_builder_call() {
    use mvm_cli::host_binaries::extract::ensure_extracted;
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = ensure_extracted(tmp.path()).unwrap();
    assert!(dir.join("mvm-builder-init").exists());
    assert!(dir.join("mvm-egress-proxy").exists());
}
