#!/usr/bin/env bash
# OpenClaw MicroVM - Production Configuration
# Optimized settings: 4 CPUs, 4GB RAM, port forwarding

set -euo pipefail

# Change to mvm project root (3 levels up from nix/examples/openclaw)
cd "$(dirname "$0")/../../.."

# Configuration
VM_NAME="${1:-oc-prod}"
INSTANCE_NAME="${2:-openclaw-1}"
PORT="${3:-3000}"

echo "Starting OpenClaw MicroVM: $VM_NAME"
echo "  Instance: $INSTANCE_NAME"
echo "  Port: $PORT"
echo ""

# Stop existing VM if running
if cargo run --quiet -- status 2>/dev/null | grep -q "$VM_NAME"; then
    echo "Stopping existing VM '$VM_NAME'..."
    cargo run --quiet -- stop "$VM_NAME"
    sleep 2
fi

# Start with optimal settings
cargo run -- run \
    --template openclaw-prod \
    --name "$VM_NAME" \
    --env "OPENCLAW_INSTANCE_NAME=$INSTANCE_NAME" \
    --cpus 4 \
    --memory 4096 \
    -p "$PORT:3000" \
    --forward

echo ""
echo "✓ OpenClaw is starting!"
echo ""
echo "Access at: http://localhost:$PORT"
echo ""
echo "To check status: cargo run -- status"
echo "To stop: cargo run -- stop $VM_NAME"
