# worker.nix — NixOS role module for worker instances.
#
# Workers execute user workloads. They receive tasks via vsock or the
# host agent and report status back. Integration state is managed
# on the data disk (/data/integrations/<name>/state/).
{ config, lib, pkgs, ... }:

{
  networking.hostName = lib.mkDefault "mvm-worker";

  # Worker agent service — communicates with host via vsock
  systemd.services."mvm-worker-agent" = {
    description = "mvm worker agent";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "mvm-worker-ready.service" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      Type = "simple";
      Restart = "always";
      RestartSec = 5;
      ExecStart = pkgs.writeShellScript "mvm-worker-agent" ''
        echo "[mvm-worker] Worker agent started"
        # Listen on vsock for host commands
        while true; do
          sleep 60
        done
      '';
    };
  };

  # Integration manager — sets up integration state directories
  # and handles checkpoint/restore during sleep/wake lifecycle
  systemd.services."mvm-integration-manager" = {
    description = "mvm integration state manager";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "mvm-integration-manager-start" ''
        set -euo pipefail
        echo "[mvm-worker] Integration manager starting"

        # Read integration list from config drive (if mounted)
        CONFIG="/etc/mvm-config/config.json"
        if [ ! -f "$CONFIG" ]; then
          echo "[mvm-worker] No config drive, skipping integration setup"
          exit 0
        fi

        # Ensure base integrations directory exists on data disk
        mkdir -p /data/integrations

        # Create state directories for each integration
        ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
          NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
          if [ -n "$NAME" ]; then
            mkdir -p "/data/integrations/$NAME/state"
            echo "[mvm-worker] Integration '$NAME': state dir ready"
          fi
        done

        echo "[mvm-worker] Integration manager ready"
      '';
    };
  };

  # Vsock sleep-prep listener — checkpoints integrations then ACKs sleep
  systemd.services."mvm-sleep-prep-vsock" = {
    description = "mvm vsock sleep-prep listener";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    serviceConfig = {
      Type = "simple";
      Restart = "always";
      RestartSec = 5;
      ExecStart = pkgs.writeShellScript "mvm-sleep-prep-vsock" ''
        echo "[mvm-worker] Sleep-prep vsock listener started on port 5200"
        # Listen on vsock port 5200 for SLEEP_PREP commands
        ${pkgs.socat}/bin/socat \
          VSOCK-LISTEN:5200,reuseaddr,fork \
          EXEC:"${pkgs.writeShellScript "sleep-handler" ''
            read CMD
            if [ "$CMD" = "SLEEP_PREP" ]; then
              # Checkpoint integrations before acknowledging sleep
              CONFIG="/etc/mvm-config/config.json"
              if [ -f "$CONFIG" ]; then
                ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
                  NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
                  CKPT_CMD=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.checkpoint_cmd // empty')
                  if [ -n "$CKPT_CMD" ] && [ -n "$NAME" ]; then
                    echo "[mvm-worker] Checkpointing integration '$NAME'"
                    if eval "$CKPT_CMD" 2>/dev/null; then
                      date -u +%Y-%m-%dT%H:%M:%SZ > "/data/integrations/$NAME/checkpoint"
                    else
                      echo "[mvm-worker] WARNING: Checkpoint failed for '$NAME'"
                    fi
                  fi
                done
              fi

              systemctl start mvm-sleep-prep.service 2>/dev/null || true
              echo "ACK"
            fi
          ''}"
      '';
    };
  };

  # Wake handler — restores integration state after snapshot resume
  systemd.services."mvm-integration-restore" = {
    description = "mvm integration restore on wake";
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = pkgs.writeShellScript "mvm-integration-restore" ''
        set -euo pipefail
        echo "[mvm-worker] Restoring integrations after wake"

        CONFIG="/etc/mvm-config/config.json"
        if [ ! -f "$CONFIG" ]; then
          exit 0
        fi

        ${pkgs.jq}/bin/jq -c '.integrations[]? // empty' "$CONFIG" 2>/dev/null | while read -r entry; do
          NAME=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.name')
          RESTORE_CMD=$(echo "$entry" | ${pkgs.jq}/bin/jq -r '.restore_cmd // empty')
          if [ -n "$RESTORE_CMD" ] && [ -n "$NAME" ]; then
            echo "[mvm-worker] Restoring integration '$NAME'"
            eval "$RESTORE_CMD" 2>/dev/null || echo "[mvm-worker] WARNING: Restore failed for '$NAME'"
          fi
        done

        echo "[mvm-worker] Integration restore complete"
      '';
    };
  };

  environment.systemPackages = with pkgs; [
    socat
    jq
  ];
}
