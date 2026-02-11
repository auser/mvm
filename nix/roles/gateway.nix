# gateway.nix — NixOS role module for gateway instances.
#
# Gateways route traffic between tenants and the external network.
# They read their configuration from the config drive (/etc/mvm-config).
# Inbound routing rules are defined in /etc/mvm-config/routes.json
# and applied as iptables DNAT rules.
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkDefault "mvm-gateway";

  # Mount config drive (vdd) — gateway requires this
  fileSystems."/etc/mvm-config" = {
    device = "/dev/vdd";
    fsType = "ext4";
    options = [ "defaults" "noatime" "ro" ];
    autoFormat = false;
    neededForBoot = false;
  };

  # Gateway service — reads config and routing table, sets up iptables DNAT
  systemd.services."mvm-gateway" = {
    description = "mvm gateway service";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "etc-mvm\\x2dconfig.mount" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "mvm-gateway-start" ''
        set -euo pipefail
        echo "[mvm-gateway] Starting gateway role"

        # Load general config
        if [ -f /etc/mvm-config/config.json ]; then
          echo "[mvm-gateway] Config loaded from /etc/mvm-config/config.json"
        fi

        # Apply routing table if present
        if [ -f /etc/mvm-config/routes.json ]; then
          echo "[mvm-gateway] Loading routing table"

          # Parse routes and create DNAT rules
          # Each route with a port gets: PREROUTING DNAT to target_ip:target_port
          ${pkgs.jq}/bin/jq -c '.routes[]' /etc/mvm-config/routes.json 2>/dev/null | while read -r route; do
            PORT=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.match_rule.port // empty')
            TARGET_IP=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.target.instance_selector.by_ip // empty')
            TARGET_PORT=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.target.target_port // empty')
            NAME=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.name // "unnamed"')

            if [ -n "$PORT" ] && [ -n "$TARGET_IP" ]; then
              DEST_PORT="''${TARGET_PORT:-$PORT}"
              echo "[mvm-gateway] Route '$NAME': port $PORT -> $TARGET_IP:$DEST_PORT"
              iptables -t nat -A PREROUTING -p tcp --dport "$PORT" \
                -j DNAT --to-destination "$TARGET_IP:$DEST_PORT" || true
              # Also handle locally-originated traffic
              iptables -t nat -A OUTPUT -p tcp --dport "$PORT" \
                -j DNAT --to-destination "$TARGET_IP:$DEST_PORT" || true
            elif [ -n "$PORT" ]; then
              echo "[mvm-gateway] Route '$NAME': port $PORT (no target IP, skipped)"
            fi
          done

          echo "[mvm-gateway] Routing table applied"
        else
          echo "[mvm-gateway] No routing table found"
        fi

        mkdir -p /run/mvm
        touch /run/mvm/gateway-ready
      '';
    };
  };

  # Healthcheck timer — periodically verifies routing targets are reachable
  systemd.services."mvm-gateway-healthcheck" = {
    description = "mvm gateway healthcheck";
    serviceConfig = {
      Type = "oneshot";
      ExecStart = pkgs.writeShellScript "mvm-gateway-healthcheck" ''
        if [ ! -f /etc/mvm-config/routes.json ]; then
          exit 0
        fi

        FAILED=0
        ${pkgs.jq}/bin/jq -c '.routes[]' /etc/mvm-config/routes.json 2>/dev/null | while read -r route; do
          TARGET_IP=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.target.instance_selector.by_ip // empty')
          NAME=$(echo "$route" | ${pkgs.jq}/bin/jq -r '.name // "unnamed"')
          if [ -n "$TARGET_IP" ]; then
            if ! ping -c 1 -W 2 "$TARGET_IP" >/dev/null 2>&1; then
              echo "[mvm-gateway-healthcheck] Route '$NAME': target $TARGET_IP unreachable"
              FAILED=$((FAILED + 1))
            fi
          fi
        done

        if [ "$FAILED" -gt 0 ]; then
          echo "[mvm-gateway-healthcheck] $FAILED route(s) have unreachable targets"
        fi
      '';
    };
  };

  systemd.timers."mvm-gateway-healthcheck" = {
    description = "mvm gateway healthcheck timer";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnBootSec = "60s";
      OnUnitActiveSec = "60s";
    };
  };

  # Enable IP forwarding for routing
  boot.kernel.sysctl = {
    "net.ipv4.ip_forward" = 1;
  };

  environment.systemPackages = with pkgs; [
    socat
    iptables
    jq
  ];
}
