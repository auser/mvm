use mvm_cli::host_binaries::manifest::{HOST_BINARIES, HostBinary};

#[test]
fn manifest_lists_mvm_host_vm_init_and_egress_proxy() {
    let names: Vec<&str> = HOST_BINARIES.iter().map(|b| b.name).collect();
    assert!(names.contains(&"mvm-host-vm-init"));
    assert!(names.contains(&"mvm-egress-proxy"));
    assert_eq!(
        HOST_BINARIES.len(),
        2,
        "expected exactly two host binaries in this ADR's scope"
    );
}

#[test]
fn manifest_install_paths_match_adr_064() {
    let by_name = |n: &str| -> &HostBinary { HOST_BINARIES.iter().find(|b| b.name == n).unwrap() };
    assert_eq!(
        by_name("mvm-host-vm-init").install_path,
        "/sbin/mvm-host-vm-init"
    );
    assert_eq!(by_name("mvm-host-vm-init").mode, 0o755);
    assert_eq!(
        by_name("mvm-egress-proxy").install_path,
        "/sbin/mvm-egress-proxy"
    );
    assert_eq!(by_name("mvm-egress-proxy").mode, 0o755);
}
