{
  description = "OpenClaw microVM template for mvm";

  inputs = {
    mvm.url = "path:../../";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nix-openclaw = {
      url = "github:openclaw/nix-openclaw";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { mvm, nixpkgs, nix-openclaw, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: builtins.listToAttrs (map (system:
        { name = system; value = f system; }
      ) systems);

      # Replace @var@ placeholders in a script file and make it executable.
      replaceVarsScript = pkgs: name: src: vars:
        pkgs.writeShellScript name (
          builtins.replaceStrings
            (map (k: "@${k}@") (builtins.attrNames vars))
            (map toString (builtins.attrValues vars))
            (builtins.readFile src)
        );

      # Build a guest image for a given OpenClaw role.
      #
      # Uses mkGuest (busybox init, no systemd) for fast boot and small
      # images.  Shell scripts live in ./scripts/ and get variable
      # substitution via replaceVarsScript at build time.
      mkRole = system: { role, tmpfsSizeMib ? 1024 }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ nix-openclaw.overlays.default ];
          };
          # Wrap nix-openclaw's gateway with esbuild bundling for fast
          # startup on Firecracker's virtio-block storage.
          openclaw = pkgs.callPackage ./pkgs/openclaw-bundled.nix {};
          serviceName = "openclaw-${role}";

          setupScript = replaceVarsScript pkgs "openclaw-setup" ./scripts/setup.sh {
            socat = pkgs.socat;
            tmpfsSize = toString tmpfsSizeMib;
            inherit openclaw;
          };

          commandScript = replaceVarsScript pkgs "${serviceName}-start" ./scripts/start.sh {
            openclaw = openclaw;
            inherit role;
          };
        in
        mvm.lib.${system}.mkGuest {
          name = "openclaw";
          hostname = "openclaw";
          packages = [ openclaw ];

          users.openclaw = {
            home = "/var/lib/openclaw";
          };

          services.${serviceName} = {
            preStart = "${setupScript}";
            command = "${commandScript}";
            user = "openclaw";
          };

          healthChecks.${serviceName} = {
            # Use busybox wget instead of nc -z (busybox nc may lack -z flag).
            healthCmd = "wget -q -O /dev/null http://127.0.0.1:3000/ 2>/dev/null";
            healthIntervalSecs = 10;
            healthTimeoutSecs = 5;
          };
        };
    in {
      packages = eachSystem (system: {
        # Gateway variant — lightweight MCP proxy, no persistent data disk.
        tenant-gateway = mkRole system { role = "gateway"; tmpfsSizeMib = 1024; };

        # Worker variant — agent execution, uses persistent data disk.
        tenant-worker = mkRole system { role = "worker"; tmpfsSizeMib = 2048; };

        # Default = gateway (backward compatible, lower resource requirement).
        default = mkRole system { role = "gateway"; tmpfsSizeMib = 1024; };
      });
    };
}
