#!/usr/bin/env bash
# Plan 97 Phase A — convenience wrapper around `swift build` + `codesign`.
#
# Vz refuses to start a VM unless the calling process carries the
# `com.apple.security.virtualization` entitlement (Hypervisor.framework
# rejects unsigned processes). Swift PM's auto-emitted entitlements
# file only carries `get-task-allow`, so we re-sign with the local
# `Entitlements.plist` after the build.
#
# Used by:
#   - contributors locally (`./tools/build.sh && ./.build/debug/mvm-vz-supervisor < cfg.json`)
#   - Phase B's `mvm-vz` Rust crate build.rs (it will shell out here)
#   - CI's `vz-smoke` job

set -euo pipefail

CONFIGURATION="${1:-debug}"

cd "$(dirname "$0")/.."
PACKAGE_ROOT="$(pwd)"

case "$CONFIGURATION" in
    debug)
        SWIFT_FLAGS=()
        ;;
    release)
        SWIFT_FLAGS=(-c release)
        ;;
    *)
        echo "usage: $0 [debug|release]" >&2
        exit 2
        ;;
esac

swift build "${SWIFT_FLAGS[@]}"

ARCH="$(uname -m | sed 's/x86_64/x86_64/;s/arm64/arm64/')"
BINARY="$PACKAGE_ROOT/.build/${ARCH}-apple-macosx/${CONFIGURATION}/mvm-vz-supervisor"

if [[ ! -x "$BINARY" ]]; then
    # Swift PM sometimes drops binaries in the unprefixed location
    # (`.build/<configuration>/`). Fall back to that before giving up.
    ALT="$PACKAGE_ROOT/.build/${CONFIGURATION}/mvm-vz-supervisor"
    if [[ -x "$ALT" ]]; then
        BINARY="$ALT"
    else
        echo "error: built binary not found at $BINARY" >&2
        exit 3
    fi
fi

# Ad-hoc sign (`-s -`) with our entitlements. Dev / source-checkout
# builds use ad-hoc; release-pipeline builds substitute a Developer ID
# identity and run `notarytool` after this script (Plan 97 §"Notarization
# & Gatekeeper"). The `--force` flag re-signs over Swift PM's auto-emitted
# entitlements; without it, codesign would error on an already-signed
# binary.
codesign --force --sign - \
    --entitlements "$PACKAGE_ROOT/Entitlements.plist" \
    --options runtime \
    "$BINARY"

echo "built and signed: $BINARY"
