# openclaw.nix — NixOS role module for OpenClaw capability instances.
#
# Combines worker patterns (vsock, integration manager, sleep-prep) with
# gateway patterns (config drive mount) plus OpenClaw-specific services.
# These VMs run the OpenClaw Node.js application and handle Telegram/Discord
# messaging integrations with Claude AI.
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkDefault "mvm-openclaw";

  # --- Config drive mount (from gateway pattern) ---
  fileSystems."/etc/mvm-config" = {
    device = "/dev/vdd";
    fsType = "ext4";
    options = [ "defaults" "noatime" "ro" ];
    autoFormat = false;
    neededForBoot = false;
  };

  # --- OpenClaw environment assembler ---
  # Reads secrets from the secrets drive and assembles /run/openclaw/env
  systemd.services."mvm-openclaw-env" = {
    description = "mvm openclaw environment assembler";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    before = [ "openclaw-gateway.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "mvm-openclaw-env" ''
        set -euo pipefail
        echo "[mvm-openclaw] Assembling environment"

        mkdir -p /run/openclaw

        # Collect secrets from the secrets drive into a single env file.
        # Each secret is stored as a file: /run/secrets/<scope>/<KEY>
        ENV_FILE="/run/openclaw/env"
        : > "$ENV_FILE"
        chmod 600 "$ENV_FILE"

        if [ -d /run/secrets ]; then
          find /run/secrets -type f | sort | while read -r secret_file; do
            KEY=$(basename "$secret_file")
            VALUE=$(cat "$secret_file")
            echo "$KEY=$VALUE" >> "$ENV_FILE"
          done
          echo "[mvm-openclaw] Environment assembled from secrets drive"
        else
          echo "[mvm-openclaw] No secrets drive mounted, empty env"
        fi

        chown openclaw:openclaw "$ENV_FILE"
      '';
    };
  };

  # --- OpenClaw gateway service ---
  # The Node.js application that handles Telegram/Discord routing
  systemd.services."openclaw-gateway" = {
    description = "OpenClaw gateway service";
    wantedBy = [ "multi-user.target" ];
    after = [
      "network-online.target"
      "etc-mvm\\x2dconfig.mount"
      "mvm-openclaw-env.service"
      "mvm-integration-manager.service"
    ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "simple";
      User = "openclaw";
      Group = "openclaw";
      Restart = "always";
      RestartSec = 5;
      EnvironmentFile = "/run/openclaw/env";
      WorkingDirectory = "/opt/openclaw";
      ExecStart = pkgs.writeShellScript "openclaw-gateway-start" ''
        echo "[openclaw] Starting OpenClaw gateway"

        # Read instance config from config drive
        if [ -f /etc/mvm-config/config.json ]; then
          export MVM_INSTANCE_ID=$(${pkgs.jq}/bin/jq -r '.instance_id // empty' /etc/mvm-config/config.json)
          export MVM_POOL_ID=$(${pkgs.jq}/bin/jq -r '.pool_id // empty' /etc/mvm-config/config.json)
          export MVM_TENANT_ID=$(${pkgs.jq}/bin/jq -r '.tenant_id // empty' /etc/mvm-config/config.json)
          export MVM_GUEST_IP=$(${pkgs.jq}/bin/jq -r '.guest_ip // empty' /etc/mvm-config/config.json)
        fi

        # Start Node.js application
        exec ${pkgs.nodejs_22}/bin/node /opt/openclaw/index.js
      '';
    };
  };

  # --- Worker agent (vsock host communication) ---
  systemd.services."mvm-worker-agent" = {
    description = "mvm worker agent";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "simple";
      Restart = "always";
      RestartSec = 5;
      ExecStart = pkgs.writeShellScript "mvm-worker-agent" ''
        echo "[mvm-openclaw] Worker agent started"
        while true; do
          sleep 60
        done
      '';
    };
  };

  # --- Integration manager ---
  systemd.services."mvm-integration-manager" = {
    description = "mvm integration state manager";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "mvm-integration-manager-start" ''
        set -euo pipefail
        echo "[mvm-openclaw] Integration manager starting"

        CONFIG="/etc/mvm-config/config.json"
        if [ ! -f "$CONFIG" ]; then
          echo "[mvm-openclaw] No config drive, skipping integration setup"
          exit 0
        fi

        mkdir -p /data/integrations

        ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
          NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
          if [ -n "$NAME" ]; then
            mkdir -p "/data/integrations/$NAME/state"
            echo "[mvm-openclaw] Integration '$NAME': state dir ready"
          fi
        done

        echo "[mvm-openclaw] Integration manager ready"
      '';
    };
  };

  # --- Sleep-prep vsock listener ---
  systemd.services."mvm-sleep-prep-vsock" = {
    description = "mvm vsock sleep-prep listener";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    serviceConfig = {
      Type = "simple";
      Restart = "always";
      RestartSec = 5;
      ExecStart = pkgs.writeShellScript "mvm-sleep-prep-vsock" ''
        echo "[mvm-openclaw] Sleep-prep vsock listener started on port 5200"
        ${pkgs.socat}/bin/socat \
          VSOCK-LISTEN:5200,reuseaddr,fork \
          EXEC:"${pkgs.writeShellScript "sleep-handler" ''
            read CMD
            if [ "$CMD" = "SLEEP_PREP" ]; then
              # Signal OpenClaw to checkpoint integrations
              CONFIG="/etc/mvm-config/config.json"
              if [ -f "$CONFIG" ]; then
                ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
                  NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
                  CKPT_CMD=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.checkpoint_cmd // empty')
                  if [ -n "$CKPT_CMD" ] && [ -n "$NAME" ]; then
                    echo "[mvm-openclaw] Checkpointing integration '$NAME'"
                    if eval "$CKPT_CMD" 2>/dev/null; then
                      date -u +%Y-%m-%dT%H:%M:%SZ > "/data/integrations/$NAME/checkpoint"
                    else
                      echo "[mvm-openclaw] WARNING: Checkpoint failed for '$NAME'"
                    fi
                  fi
                done
              fi

              # Stop the gateway gracefully before sleep
              systemctl stop openclaw-gateway.service 2>/dev/null || true
              systemctl start mvm-sleep-prep.service 2>/dev/null || true
              echo "ACK"
            fi
          ''}"
      '';
    };
  };

  # --- Wake handler ---
  # Restores integration state and restarts the gateway after snapshot resume
  systemd.services."mvm-openclaw-wake" = {
    description = "mvm openclaw wake handler";
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = pkgs.writeShellScript "mvm-openclaw-wake" ''
        set -euo pipefail
        echo "[mvm-openclaw] Restoring after wake"

        # Re-assemble env (secrets may have rotated)
        systemctl restart mvm-openclaw-env.service

        # Restore integrations
        CONFIG="/etc/mvm-config/config.json"
        if [ -f "$CONFIG" ]; then
          ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
            NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
            RESTORE_CMD=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.restore_cmd // empty')
            if [ -n "$RESTORE_CMD" ] && [ -n "$NAME" ]; then
              echo "[mvm-openclaw] Restoring integration '$NAME'"
              eval "$RESTORE_CMD" 2>/dev/null || echo "[mvm-openclaw] WARNING: Restore failed for '$NAME'"
            fi
          done
        fi

        # Restart the OpenClaw gateway
        systemctl restart openclaw-gateway.service

        echo "[mvm-openclaw] Wake restore complete"
      '';
    };
  };

  # --- OpenClaw user ---
  users.users.openclaw = {
    isSystemUser = true;
    group = "openclaw";
    home = "/opt/openclaw";
    createHome = true;
  };
  users.groups.openclaw = {};

  # --- TCP keepalive for long-lived connections ---
  boot.kernel.sysctl = {
    "net.ipv4.tcp_keepalive_time" = 60;
    "net.ipv4.tcp_keepalive_intvl" = 10;
    "net.ipv4.tcp_keepalive_probes" = 6;
  };

  environment.systemPackages = with pkgs; [
    nodejs_22
    socat
    jq
    curl
    git
  ];
}
