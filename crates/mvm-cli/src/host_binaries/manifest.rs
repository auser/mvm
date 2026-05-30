//! Compile-time Rust mirror of `nix/lib/mvm-host-binaries.nix`.
//! Parity with the Nix attrset is asserted by the
//! `check-mvm-host-binaries-sync` xtask (Task 3).

#[derive(Debug, Clone, Copy)]
pub struct HostBinary {
    /// Cargo package name + name on disk after extraction.
    pub name: &'static str,
    /// Absolute path inside the builder/dev VM rootfs.
    pub install_path: &'static str,
    /// Unix mode (e.g. 0o755) applied via the flake's extraFiles.
    /// Mirror note: `nix/lib/mvm-host-binaries.nix` stores this as
    /// a decimal string (`"0755"`); the `check-mvm-host-binaries-sync`
    /// xtask (Task 3) parses + compares numerically.
    pub mode: u32,
}

pub const HOST_BINARIES: &[HostBinary] = &[
    HostBinary {
        name: "mvm-builder-init",
        install_path: "/sbin/mvm-builder-init",
        mode: 0o755,
    },
    HostBinary {
        name: "mvm-egress-proxy",
        install_path: "/sbin/mvm-egress-proxy",
        mode: 0o755,
    },
];
