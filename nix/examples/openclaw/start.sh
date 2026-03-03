#!/usr/bin/env bash
# OpenClaw MicroVM - Native npx installer
# First run downloads OpenClaw via npx (5-10 min)

set -euo pipefail

cd "$(dirname "$0")/../../.."

VM_NAME="${1:-openclaw}"
PORT="${2:-3000}"

echo "Starting OpenClaw MicroVM: $VM_NAME"
echo "  Port: $PORT"
echo ""
echo "Note: First run downloads OpenClaw via npx (~5-10 min)"
echo ""

# Stop existing VM if running
if cargo run --quiet -- status 2>/dev/null | grep -q "^  $VM_NAME"; then
    echo "Stopping existing VM '$VM_NAME'..."
    cargo run --quiet -- stop "$VM_NAME"
    sleep 2
fi

# Run with native npx installer (no complex bundling)
# Pass config and secrets from host directories
cargo run -- run \
    --flake ./nix/examples/openclaw \
    --name "$VM_NAME" \
    --cpus 4 \
    --memory 4096 \
    -p "$PORT:3000" \
    --config-dir ./nix/examples/openclaw/config \
    --secrets-dir ./nix/examples/openclaw/secrets \
    --forward

echo ""
echo "✓ OpenClaw is starting!"
echo ""
echo "Access at: http://localhost:$PORT"
echo ""
echo "To check status: cargo run -- status"
echo "To stop: cargo run -- stop $VM_NAME"
