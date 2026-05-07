#!/usr/bin/env bash
# Combined production-agent symbol contract (ADR-002 §W4.3 + ADR-007 §W5).
#
# The production guest agent binary has TWO load-bearing symbol-level
# invariants that must hold against the *same* binary that ships:
#
#   1. The dev-only `mvm_guest_agent::do_exec` symbol must be ABSENT.
#      This is the W4.3 invariant — `do_exec` is the dev-shell-gated
#      arbitrary-shell handler. A prod build must omit `--features
#      dev-shell` and therefore must not contain the symbol.
#
#   2. The W2 `mvm_guest_agent::handle_run_entrypoint` symbol must be
#      PRESENT. This is the constrained `RunEntrypoint` handler —
#      ADR-007's production-safe call surface. A prod build that lacks
#      it can't actually serve `mvmctl invoke`, which means the
#      shipping artifact is broken.
#
# Asserting both on the same binary in one CI step prevents
# feature-flag drift from regressing half the contract silently
# (ADR-007 / plan 41 W5).
#
# Usage: scripts/check-prod-agent-no-exec.sh
#
# Exit codes: 0 = clean, 1 = symbol contract violated, 2 = build failed.
set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

echo "==> building mvm-guest-agent (production: no dev-shell feature, profile=$PROFILE)"
# --no-default-features and explicit feature list both omit dev-shell, but
# the crate has no default features today so --no-default-features is the
# defensive choice — adding a default later won't silently arm the gate.
#
# `profile.release.strip = "none"` override: the workspace's release
# profile sets `strip = true`, which removes ALL symbols from the
# binary. Without this override every `nm`-based check below would
# trivially "succeed" on the negative gate (do_exec absent because
# everything is stripped) and "fail" on the positive gate
# (handle_run_entrypoint absent for the same reason). The override
# only affects this verification build under `target/release/`; the
# shipping artifact built by callers without this script still gets
# stripped per the workspace profile.
cargo build \
    -p mvm-guest \
    --bin mvm-guest-agent \
    --profile "$PROFILE" \
    --no-default-features \
    --config 'profile.release.strip="none"'

case "$PROFILE" in
    dev) PROFILE_DIR="debug" ;;
    *)   PROFILE_DIR="$PROFILE" ;;
esac
BIN="$TARGET_DIR/$PROFILE_DIR/mvm-guest-agent"

if [[ ! -f "$BIN" ]]; then
    echo "error: built binary not found at $BIN" >&2
    exit 2
fi

echo "==> scanning $BIN for forbidden Exec symbols"

# Mach-O (macOS) and ELF (Linux) both support `nm`. We pipe through
# rustfilt-like demangling via `nm -C` where supported; fall back to plain
# `nm` if `-C` is rejected.
if nm -C "$BIN" >/dev/null 2>&1; then
    NM_CMD=(nm -C)
else
    NM_CMD=(nm)
fi

# The forbidden symbol is `mvm_guest_agent::do_exec`, the dev-shell-gated
# command runner. We anchor on the crate name to avoid matching stdlib's
# unrelated `<std::sys::process::unix::common::Command>::do_exec`, which is
# always present because libstd uses the same identifier internally.
PATTERN='mvm_guest_agent::do_exec'
if "${NM_CMD[@]}" "$BIN" 2>/dev/null | grep -F "$PATTERN" >/dev/null; then
    echo "error: forbidden symbol '$PATTERN' present in production guest agent" >&2
    echo "       this means the dev-shell feature is enabled on a path it" >&2
    echo "       should not be. See ADR-002 §W4.3 and the dev-shell gate" >&2
    echo "       in crates/mvm-guest/src/bin/mvm-guest-agent.rs." >&2
    "${NM_CMD[@]}" "$BIN" 2>/dev/null | grep -F "$PATTERN" >&2 || true
    exit 1
fi

echo "==> ok: no do_exec symbol in $BIN"

# ─── Positive: handle_run_entrypoint must be PRESENT (ADR-007 §W5) ─────
# The W2 handler is feature-independent (no `dev-shell` gate) — every
# prod build must contain it. Absence means either the function was
# accidentally removed, gated behind a feature, or renamed without
# updating this gate. Either way, the prod artifact can't serve
# `mvmctl invoke` and is broken.
RUNENTRY_PATTERN='mvm_guest_agent::handle_run_entrypoint'
if ! "${NM_CMD[@]}" "$BIN" 2>/dev/null | grep -F "$RUNENTRY_PATTERN" >/dev/null; then
    echo "error: required symbol '$RUNENTRY_PATTERN' missing from production guest agent" >&2
    echo "       this means the W2 RunEntrypoint handler is not compiled in," >&2
    echo "       and the shipping artifact cannot serve 'mvmctl invoke'." >&2
    echo "       See ADR-007 §W5 and the handler in" >&2
    echo "       crates/mvm-guest/src/bin/mvm-guest-agent.rs::handle_run_entrypoint." >&2
    exit 1
fi

echo "==> ok: handle_run_entrypoint symbol present in $BIN"

# ─── Positive: dispatch_via_warm_pool must be PRESENT (plan 43) ────────
# Plan 43 (warm-process function dispatch, mvmforge ADR-0011 tier 2)
# adds a worker-pool path that runs alongside the cold-tier handler.
# The substrate is always linked into the prod binary (the path is
# opt-in at runtime via `/etc/mvm/runtime.json`, not at build time).
# Asserting its presence as positive evidence catches:
#   - someone gating `dispatch_via_warm_pool` behind a feature flag
#   - LTO inlining erasing the symbol (fixed by `#[inline(never)]`
#     on the function, but the gate is the safety net)
#   - the worker-pool module being accidentally unlinked
# A prod build without this symbol cannot serve warm-tier images,
# even though the cold tier still works — partial substrate is
# worse than honest absence.
WARM_PATTERN='mvm_guest_agent::dispatch_via_warm_pool'
if ! "${NM_CMD[@]}" "$BIN" 2>/dev/null | grep -F "$WARM_PATTERN" >/dev/null; then
    echo "error: required symbol '$WARM_PATTERN' missing from production guest agent" >&2
    echo "       this means the warm-process worker-pool dispatch path is" >&2
    echo "       not compiled in, and warm-tier images will fall through to" >&2
    echo "       the cold path silently." >&2
    echo "       See plan 43 / mvmforge ADR-0011 and the helper in" >&2
    echo "       crates/mvm-guest/src/bin/mvm-guest-agent.rs::dispatch_via_warm_pool." >&2
    exit 1
fi

echo "==> ok: dispatch_via_warm_pool symbol present in $BIN"

# ─── Positive: install_signal_handlers must be PRESENT (plan 44) ───────
# Plan 44 (guest agent signal handling) wires SIGTERM/SIGINT
# handlers that call `WorkerPool::shutdown` so warm-process workers
# drain cleanly before the agent exits. The handler installation is
# a one-shot at boot; absence means a manually-killed agent
# strands in-flight calls and leaves workers reparented to PID 1
# without orderly drain. Asserting presence catches:
#   - someone removing the install call from `main()`
#   - LTO erasing the symbol despite `#[inline(never)]`
#   - the function being feature-gated unintentionally
SIGNAL_PATTERN='mvm_guest_agent::install_signal_handlers'
if ! "${NM_CMD[@]}" "$BIN" 2>/dev/null | grep -F "$SIGNAL_PATTERN" >/dev/null; then
    echo "error: required symbol '$SIGNAL_PATTERN' missing from production guest agent" >&2
    echo "       this means the signal-handler installation is not compiled in," >&2
    echo "       and SIGTERM/SIGINT will not drain the warm-process pool." >&2
    echo "       See plan 44 and the install in" >&2
    echo "       crates/mvm-guest/src/bin/mvm-guest-agent.rs::install_signal_handlers." >&2
    exit 1
fi

echo "==> ok: install_signal_handlers symbol present in $BIN"

# ─── Variant ↔ feature pairing (W7.1) ────────────────────────────────────
# `mkGuest` (in nix/flake.nix) asserts at build time that:
#   variant="prod" ↔ guestAgent.passthru.devShell == false
#   variant="dev"  ↔ guestAgent.passthru.devShell == true
# The flake-level assertion is the primary enforcement. Below we also do a
# best-effort eval-time cross-check on `nix/default-microvm` so a mistakenly
# edited dev-image flake (e.g. someone passing variant="dev" to a prod
# image) fails loudly even before the rootfs build runs. Skipped silently
# when `nix` isn't on PATH (host dev shells without Nix installed).
if command -v nix >/dev/null 2>&1; then
    echo "==> eval: nix/default-microvm rootfs variant tag"
    SYSTEM="$(nix eval --impure --raw --expr 'builtins.currentSystem' 2>/dev/null || echo "")"
    if [[ -z "$SYSTEM" ]]; then
        echo "warning: could not detect builtins.currentSystem; skipping variant cross-check" >&2
    else
        VARIANT="$(nix eval --raw \
            "./nix/default-microvm#packages.${SYSTEM}.default.variant" \
            2>/dev/null || echo "")"
        if [[ -z "$VARIANT" ]]; then
            echo "warning: could not evaluate variant attribute (eval failed or system not exposed); skipping" >&2
        elif [[ "$VARIANT" != "prod" ]]; then
            echo "error: nix/default-microvm rootfs variant='$VARIANT' (expected 'prod')" >&2
            echo "       a non-prod variant tag on the default tenant fallback rootfs" >&2
            echo "       means the dev-shell feature is leaking into production." >&2
            exit 1
        else
            echo "==> ok: nix/default-microvm rootfs variant='prod'"
        fi
    fi
else
    echo "==> skip: nix not on PATH; flake-level variant assertion still enforces pairing at build time"
fi
