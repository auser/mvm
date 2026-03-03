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

exec @openclaw@/bin/openclaw @role@ --port 3000 --allow-unconfigured
