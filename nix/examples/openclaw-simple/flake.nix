{
  description = "OpenClaw microVM - simple native install";

  inputs = {
    mvm.url = "path:../../";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { mvm, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: builtins.listToAttrs (map (system:
        { name = system; value = f system; }
      ) systems);
    in {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in {
          default = mvm.lib.${system}.mkGuest {
            name = "openclaw";
            hostname = "openclaw";

            # Just Node.js 22 - OpenClaw will install itself
            packages = [ pkgs.nodejs_22 ];

            users.openclaw = {
              home = "/var/lib/openclaw";
            };

            services.openclaw = {
              preStart = pkgs.writeShellScript "openclaw-setup" ''
                # Create working directory on tmpfs
                mount -t tmpfs -o mode=0755,size=2048m tmpfs /var/lib/openclaw
                chown openclaw:openclaw /var/lib/openclaw

                # Create subdirectories
                install -d -o openclaw -g openclaw /var/lib/openclaw/{.npm,.cache,workspace,data}

                # Write minimal config
                cat > /var/lib/openclaw/config.json <<'CONFIG'
                {
                  "gateway": {
                    "mode": "local",
                    "port": 3000
                  }
                }
                CONFIG
                chown openclaw:openclaw /var/lib/openclaw/config.json
              '';

              command = pkgs.writeShellScript "openclaw-start" ''
                set -eu
                cd /var/lib/openclaw

                # Set npm config for this user
                export npm_config_cache=/var/lib/openclaw/.npm
                export npm_config_userconfig=/var/lib/openclaw/.npmrc

                # Run OpenClaw via npx (downloads and caches on first run)
                echo "[openclaw] starting via npx (first run may take 5-10 min to download)" >&2
                exec ${pkgs.nodejs_22}/bin/npx --yes openclaw@latest gateway \
                  --port 3000 \
                  --config /var/lib/openclaw/config.json \
                  --allow-unconfigured
              '';

              user = "openclaw";
            };

            healthChecks.openclaw = {
              healthCmd = "wget -q -O /dev/null http://127.0.0.1:3000/ 2>/dev/null";
              healthIntervalSecs = 10;
              healthTimeoutSecs = 5;
            };
          };
        });
    };
}
