# Boot-time setup for OpenClaw microVM — runs as root before the service starts.
#
# Nix replaces: @socat@, @tmpfsSize@, @openclaw@
# Wrapped by writeShellScript (shebang added automatically).
set -eu

# ── Scratch workspace (tmpfs — ephemeral, fast) ──────────────────────
# The /var/lib/openclaw mount point is created by the init's user setup
# (home dir for the openclaw user).
mount -t tmpfs -o "mode=0755,size=@tmpfsSize@m" tmpfs /var/lib/openclaw
chown openclaw:openclaw /var/lib/openclaw

# Runtime directories on tmpfs.
install -d -o openclaw -g openclaw /var/lib/openclaw/config
install -d -o openclaw -g openclaw /var/lib/openclaw/.state
install -d -o openclaw -g openclaw /var/lib/openclaw/logs

# ── Config ────────────────────────────────────────────────────────────
# Copy read-only config to a writable location so OpenClaw can update
# settings at runtime (enable skills, change models, etc.).
if [ -f /mnt/config/openclaw.json ]; then
  install -o openclaw -g openclaw -m 0644 \
    /mnt/config/openclaw.json /var/lib/openclaw/config/openclaw.json
else
  # No config provided — write a minimal default so the service can
  # start in local mode without requiring setup.
  # IMPORTANT: OpenClaw validates config strictly — do NOT add extra
  # keys (e.g. "version") or the service will fail to start.
  printf '{"gateway":{"mode":"local","port":3000}}\n' \
    > /var/lib/openclaw/config/openclaw.json
  chown openclaw:openclaw /var/lib/openclaw/config/openclaw.json
fi

# ── Persistent storage ───────────────────────────────────────────────
# Data drive (survives reboots) or tmpfs fallback (ephemeral).
if mountpoint -q /mnt/data 2>/dev/null; then
  for d in skills workspace sessions; do
    install -d -o openclaw -g openclaw "/mnt/data/openclaw/$d"
    ln -sfn "/mnt/data/openclaw/$d" "/var/lib/openclaw/$d"
  done
else
  install -d -o openclaw -g openclaw \
    /var/lib/openclaw/skills \
    /var/lib/openclaw/workspace \
    /var/lib/openclaw/sessions
fi

# ── Page cache warming ──────────────────────────────────────────────
# Pre-read the esbuild bundle into the Linux page cache so V8
# doesn't wait on virtio-block I/O during module compilation.
cat @openclaw@/lib/openclaw/dist/openclaw-bundle.mjs > /dev/null 2>/dev/null || true

# ── Socat forwarding ─────────────────────────────────────────────────
# OpenClaw binds to 127.0.0.1 only — use socat to forward incoming TAP
# traffic to loopback so port forwarding from the host works.
# (iptables DNAT won't work: the Firecracker guest kernel has no
# netfilter support.)
GUEST_IP=$(sed -n 's/.*mvm\.ip=\([0-9.]*\).*/\1/p' /proc/cmdline)
if [ -n "$GUEST_IP" ]; then
  @socat@/bin/socat TCP-LISTEN:3000,bind="$GUEST_IP",fork,reuseaddr TCP:127.0.0.1:3000 &
  @socat@/bin/socat TCP-LISTEN:3002,bind="$GUEST_IP",fork,reuseaddr TCP:127.0.0.1:3002 &
fi
