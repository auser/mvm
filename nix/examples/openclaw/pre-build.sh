#!/usr/bin/env bash
# Pre-build hook: install OpenClaw using the official installer script.
#
# Runs inside the Lima VM before `nix build`. The installed files at
# /opt/openclaw are referenced by the flake via builtins.path (requires
# --impure, which dev_build adds automatically when this hook exists).
set -euo pipefail

OPENCLAW_VERSION="2026.2.26"
INSTALL_DIR="/opt/openclaw"

# Skip if already installed at the correct version.
if [ -f "$INSTALL_DIR/package.json" ]; then
  current=$(grep -o '"version":\s*"[^"]*"' "$INSTALL_DIR/package.json" | head -1 | grep -o '[0-9][^"]*')
  if [ "$current" = "$OPENCLAW_VERSION" ]; then
    echo "OpenClaw $OPENCLAW_VERSION already installed at $INSTALL_DIR"
    exit 0
  fi
fi

# Use Nix to provide Node.js for the installer (no apt needed).
# This avoids polluting the Lima VM with system-wide Node.js and
# sidesteps apt lock races entirely.
NIX_NODE="nix shell nixpkgs#nodejs_22 --command"

echo "Installing OpenClaw $OPENCLAW_VERSION via official installer..."
echo "Using Node.js from Nix: $($NIX_NODE node --version)"
$NIX_NODE bash -c "curl -fsSL https://openclaw.ai/install.sh | bash -s -- --no-onboard --version $OPENCLAW_VERSION"

# Locate the installed package (npm global directory).
NPM_GLOBAL=$($NIX_NODE npm root -g 2>/dev/null || echo "")
SRC_DIR="$NPM_GLOBAL/openclaw"

if [ ! -d "$SRC_DIR" ]; then
  # Fallback: check common npm global locations
  for candidate in \
    "$HOME/.local/lib/node_modules/openclaw" \
    "/usr/local/lib/node_modules/openclaw" \
    "/usr/lib/node_modules/openclaw"; do
    if [ -d "$candidate" ]; then
      SRC_DIR="$candidate"
      break
    fi
  done
fi

if [ ! -d "$SRC_DIR" ]; then
  echo "ERROR: OpenClaw not found after installation" >&2
  exit 1
fi

# Copy to the stable path that the flake references.
sudo rm -rf "$INSTALL_DIR"
sudo mkdir -p "$INSTALL_DIR"
sudo cp -a "$SRC_DIR/." "$INSTALL_DIR/"
sudo chmod -R a+rX "$INSTALL_DIR"

echo "OpenClaw $OPENCLAW_VERSION installed to $INSTALL_DIR"
