# OpenClaw service start — runs as the openclaw user.
#
# Nix replaces: @openclaw@, @role@
# Wrapped by writeShellScript (shebang added automatically).
set -eu

# Source mvm-injected port mappings and environment variables.
[ -f /mnt/config/mvm-ports.env ] && . /mnt/config/mvm-ports.env
[ -f /mnt/config/mvm-env.env ] && . /mnt/config/mvm-env.env

# Source optional environment overrides.
[ -f /mnt/config/openclaw.env ] && . /mnt/config/openclaw.env
[ -f /mnt/secrets/openclaw-secrets.env ] && . /mnt/secrets/openclaw-secrets.env

# Set defaults (env files may override these).
: "${OPENCLAW_CONFIG_PATH:=/var/lib/openclaw/config/openclaw.json}"
: "${OPENCLAW_HOME:=/var/lib/openclaw}"
: "${OPENCLAW_STATE_DIR:=/var/lib/openclaw/.state}"
export OPENCLAW_CONFIG_PATH OPENCLAW_HOME OPENCLAW_STATE_DIR

cd /var/lib/openclaw

echo "[openclaw] starting @role@ (pid $$)" >&2
echo "[openclaw] config: $OPENCLAW_CONFIG_PATH" >&2
echo "[openclaw] binary: @openclaw@/bin/openclaw" >&2

# Extract the node binary from the wrapper (line 3: exec "...node" ...).
NODE_BIN=$(sed -n '3s/^exec "\([^"]*\)".*/\1/p' @openclaw@/bin/openclaw)
ENTRY_JS=$(sed -n '3s/^exec "[^"]*"  \([^ ]*\).*/\1/p' @openclaw@/bin/openclaw)
echo "[openclaw] node=$NODE_BIN" >&2
echo "[openclaw] entry=$ENTRY_JS" >&2

# Run OpenClaw with error tracing enabled.
echo "[openclaw] launching openclaw..." >&2
export NODE_PATH="$(sed -n "2s/^export NODE_PATH='\\(.*\\)'/\\1/p" @openclaw@/bin/openclaw)"
export NODE_OPTIONS="--trace-uncaught --trace-warnings --unhandled-rejections=warn"
$NODE_BIN "$ENTRY_JS" @role@ --port 3000 --allow-unconfigured 2>&1 &
OC_PID=$!

# Heartbeat: print status every 15s to confirm node is still alive.
(
  i=0
  while kill -0 $OC_PID 2>/dev/null; do
    i=$((i + 15))
    echo "[openclaw] still running after ${i}s (pid $OC_PID)" >&2
    sleep 15
  done
  wait $OC_PID 2>/dev/null
  echo "[openclaw] process exited with code $?" >&2
) &

wait $OC_PID 2>/dev/null
RC=$?
echo "[openclaw] process exited with code $RC" >&2
