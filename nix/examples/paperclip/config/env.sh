# Optional environment overrides for Paperclip server.
# Mount this directory with: -v ./config:/mnt/config
#
# Variables can also be set at launch with:
#   mvmctl run --env PORT=3200 --env PAPERCLIP_DEPLOYMENT_MODE=authenticated
# (--env values are set globally in the microVM before services start)

# Server port (default: 3100)
# export PORT=3100

# Deployment mode: local_trusted (no auth) or authenticated
# export PAPERCLIP_DEPLOYMENT_MODE=local_trusted

# Deployment exposure: private (LAN/VPN) or public (internet-facing)
# export PAPERCLIP_DEPLOYMENT_EXPOSURE=private

# External database (overrides the in-VM PostgreSQL)
# export DATABASE_URL=postgresql://user:pass@host:5432/paperclip

# Node.js memory limit (default: V8 auto-sizes based on available RAM)
# export NODE_OPTIONS="--max-old-space-size=2048"
