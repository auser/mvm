# Build the mvm-guest-agent binary from the workspace.
#
# Usage from a flake:
#   mvm-guest-agent = import ../../nix/modules/guest-agent-pkg.nix {
#     inherit pkgs;
#     mvmSrc = ../../.;
#   };

{ pkgs, mvmSrc }:

pkgs.rustPlatform.buildRustPackage {
  pname = "mvm-guest-agent";
  version = "0.3.0";

  # Only include Rust source to avoid rebuilds when docs/specs change.
  src = pkgs.lib.fileset.toSource {
    root = mvmSrc;
    fileset = pkgs.lib.fileset.unions [
      "${mvmSrc}/Cargo.toml"
      "${mvmSrc}/Cargo.lock"
      "${mvmSrc}/src"
      "${mvmSrc}/crates"
    ];
  };

  cargoLock.lockFile = "${mvmSrc}/Cargo.lock";
  cargoBuildFlags = [ "--bin" "mvm-guest-agent" ];
  doCheck = false;
}
