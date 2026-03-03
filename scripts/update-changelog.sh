#!/usr/bin/env bash
# Update CHANGELOG.md using git-cliff
set -euo pipefail

# Check if git-cliff is installed
if ! command -v git-cliff &> /dev/null; then
    echo "Error: git-cliff is not installed"
    echo "Install with: cargo install git-cliff"
    exit 1
fi

# Generate changelog
if [ "${1:-}" = "--tag" ]; then
    # Generate for specific tag range
    git-cliff --tag "$2" -o CHANGELOG.md
elif [ "${1:-}" = "--unreleased" ]; then
    # Show unreleased changes only (don't write to file)
    git-cliff --unreleased
else
    # Update entire changelog
    git-cliff -o CHANGELOG.md
    echo "✓ CHANGELOG.md updated"
fi
