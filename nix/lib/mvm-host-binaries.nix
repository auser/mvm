# Single source of truth (Nix view) for the mvm-internal Linux
# binaries that mvmctl embeds and bakes into the builder/dev VM
# rootfs via extraFiles. The Rust mirror at
# crates/mvm-cli/src/host_binaries/manifest.rs must agree on the
# name set and install paths; CI enforces parity (see
# xtask/src/check_mvm_host_binaries_sync.rs).
#
# Adding a binary here is part of the Plan 115 / ADR-065 contract;
# new uses of rustPlatform.buildRustPackage in mvm's flakes are
# forbidden (see ADR-065 §Principle).
{
  mvm-host-vm-init = {
    install_path = "/sbin/mvm-host-vm-init";
    mode = "0755";
  };
  mvm-egress-proxy = {
    install_path = "/sbin/mvm-egress-proxy";
    mode = "0755";
  };
}
