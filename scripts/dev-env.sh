#!/usr/bin/env bash
# scripts/dev-env.sh — per-worktree env for mvm.
#
# Source from the worktree root to isolate mvmctl + cargo state from
# the main checkout and from other worktrees:
#
#   source scripts/dev-env.sh
#
# What it sets:
#   MVM_DATA_DIR       mvmctl templates, sockets, microVM registry,
#                      snapshots, signing keys land under
#                      $WORKTREE/.mvm-test instead of ~/.mvm.
#   CARGO_TARGET_DIR   cargo target output goes to
#                      $WORKTREE/.mvm-test/target so two worktrees
#                      don't fight on output paths.
#   CARGO_HOME         cargo registry/cache + .package-cache lock are
#                      per-worktree. Without this, concurrent
#                      `cargo test` invocations across worktrees
#                      serialize on ~/.cargo/registry/.package-cache
#                      and one blocks until the other completes
#                      dependency resolution.
#   MVM_NO_LEGACY_BANNER silences the legacy `mvm` Lima VM warning
#                      so worktrees don't spam it on every command.
#
# Wrappers (`bin/dev`) and recipes (`just dev-*`) source this file.
# direnv users: `cp .envrc.example .envrc && direnv allow`.

# Resolve the worktree root so this works regardless of caller CWD.
__mvm_dev_env_root="${BASH_SOURCE[0]:-$0}"
__mvm_dev_env_root="$(cd "$(dirname "$__mvm_dev_env_root")/.." && pwd)"

export MVM_DATA_DIR="${MVM_DATA_DIR:-$__mvm_dev_env_root/.mvm-test}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$__mvm_dev_env_root/.mvm-test/target}"
export CARGO_HOME="${CARGO_HOME:-$__mvm_dev_env_root/.mvm-test/cargo}"
export MVM_NO_LEGACY_BANNER="${MVM_NO_LEGACY_BANNER:-1}"

mkdir -p "$MVM_DATA_DIR" "$CARGO_TARGET_DIR" "$CARGO_HOME"

unset __mvm_dev_env_root
