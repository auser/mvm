{
  description = "OpenClaw microVM template for mvm";

  inputs = {
    mvm.url = "path:../../";
    # Unstable required — pnpm_10.fetchDeps is only in nixpkgs-unstable.
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { mvm, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: builtins.listToAttrs (map (system:
        { name = system; value = f system; }
      ) systems);

      # Build the gateway locally instead of using the nix-openclaw overlay,
      # which bundles ML tools (whisper/torch/triton) that fail on aarch64.
      openclawFor = system:
        let pkgs = import nixpkgs { inherit system; };
        in pkgs.callPackage ./pkgs/openclaw.nix {};
    in {
      packages = eachSystem (system: {
        default = mvm.lib.${system}.mkGuest {
          name = "openclaw";
          modules = [
            ({ ... }: { _module.args.openclaw = openclawFor system; })
            ./role.nix
          ];
        };
      });
    };
}
