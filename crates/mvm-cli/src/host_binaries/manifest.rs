//! Compile-time Rust mirror of `nix/lib/mvm-host-binaries.nix`.
//! Parity with the Nix attrset is asserted by the
//! `check-mvm-host-binaries-sync` xtask (Task 3).

#[derive(Debug, Clone, Copy)]
pub struct HostBinary {
    /// Logical name: the install-time binary name, the nix attrset key,
    /// and the filename written to `~/.cache/mvm/host-bins/<hash>/`.
    /// This is what the nix manifest and xtask sync check use.
    pub name: &'static str,
    /// Cargo package name used with `cargo build -p <cargo_pkg>`.
    /// May differ from `name` when a crate was renamed after the nix
    /// attrset was established (e.g. mvm-host-vm-init ships as
    /// mvm-builder-init). The xtask sync check compares `name`, not
    /// `cargo_pkg`, against the Nix attrset.
    pub cargo_pkg: &'static str,
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
        // The crate was renamed to mvm-host-vm-init in Plan 107 A1b;
        // the logical install name and nix attrset key remain mvm-builder-init.
        cargo_pkg: "mvm-host-vm-init",
        install_path: "/sbin/mvm-builder-init",
        mode: 0o755,
    },
    HostBinary {
        name: "mvm-egress-proxy",
        cargo_pkg: "mvm-egress-proxy",
        install_path: "/sbin/mvm-egress-proxy",
        mode: 0o755,
    },
];
