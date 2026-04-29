#!/usr/bin/env bash
# Live smoke test for `mvmctl exec` (issue #3).
#
# Walks the acceptance matrix from the issue: positive/negative exit
# codes, guest kernel identity, --add-dir read-only enforcement, Ctrl-C
# teardown, the bundled default-microvm path, and end-to-end latency.
#
# Requirements:
#   - Linux host with /dev/kvm  OR  a running Lima dev VM on macOS
#   - mvmctl on PATH
#   - nix (only for the `nix build` step)
#
# Usage:
#   ./scripts/smoke-exec.sh             # run the full matrix
#   STEPS="exit add-dir" ./scripts/smoke-exec.sh   # run a subset
#
# Each step prints PASS/FAIL and the script exits non-zero on any failure.

set -uo pipefail

steps_default="exit kernel add-dir add-dir-readonly ctrl-c default-up nix-build latency"
steps="${STEPS:-$steps_default}"

mvmctl="${MVMCTL:-mvmctl}"
default_template="${DEFAULT_TEMPLATE:-default}"

pass() { printf '\033[32mPASS\033[0m  %s\n' "$1"; }
fail() { printf '\033[31mFAIL\033[0m  %s\n' "$1"; failures=$((failures + 1)); }
info() { printf '       %s\n' "$1"; }

failures=0
host_tmp=
trap '[ -n "${host_tmp:-}" ] && rm -rf "$host_tmp"' EXIT

want() {
    case " $steps " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

# 1. Exit codes
if want exit; then
    "$mvmctl" exec --template "$default_template" -- /bin/true
    rc=$?
    [ "$rc" -eq 0 ] && pass "exec /bin/true exits 0" || fail "exec /bin/true exit=$rc (want 0)"

    "$mvmctl" exec --template "$default_template" -- /bin/false
    rc=$?
    [ "$rc" -eq 1 ] && pass "exec /bin/false exits 1" || fail "exec /bin/false exit=$rc (want 1)"
fi

# 2. Guest kernel identity
if want kernel; then
    out=$("$mvmctl" exec --template "$default_template" -- uname -a 2>&1) || true
    info "$out"
    host_uname=$(uname -a 2>/dev/null)
    if echo "$out" | grep -q '^Linux ' && [ "$out" != "$host_uname" ]; then
        pass "uname -a reports a Linux guest kernel distinct from the host"
    else
        fail "uname -a didn't look like a distinct guest kernel: $out"
    fi
fi

# 3. --add-dir mount + read
if want add-dir; then
    host_tmp=$(mktemp -d)
    echo hello > "$host_tmp/foo"
    out=$("$mvmctl" exec --template "$default_template" --add-dir "$host_tmp:/host" -- cat /host/foo 2>&1)
    rc=$?
    [ "$rc" -eq 0 ] && [ "$out" = "hello" ] \
        && pass "--add-dir read-only mount visible in guest" \
        || fail "--add-dir read failed: rc=$rc out=$out"
fi

# 4. --add-dir read-only enforcement (default :ro)
if want add-dir-readonly; then
    host_tmp="${host_tmp:-$(mktemp -d)}"
    out=$("$mvmctl" exec --template "$default_template" --add-dir "$host_tmp:/host" \
            -- sh -c 'echo nope > /host/should-fail' 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -qiE 'read-only|EROFS|read only file'; then
        pass "--add-dir default :ro rejects writes (EROFS)"
    else
        fail "--add-dir default :ro accepted a write (rc=$rc): $out"
    fi
fi

# 5. Ctrl-C teardown — leaves no orphan firecracker / TAP / staging dir
if want ctrl-c; then
    info "Ctrl-C teardown — start a sleep, SIGINT, and verify cleanup."
    "$mvmctl" exec --template "$default_template" -- sleep 600 &
    pid=$!
    sleep 5
    kill -INT "$pid"
    wait "$pid" 2>/dev/null || true

    pgrep -lf firecracker | grep -v grep || true
    if pgrep -af firecracker | grep -q exec-; then
        fail "orphan firecracker process after Ctrl-C"
    else
        pass "no orphan firecracker process after Ctrl-C"
    fi

    if ip -br link 2>/dev/null | awk '{print $1}' | grep -qE '^tap-exec-'; then
        fail "leftover TAP device after Ctrl-C"
    else
        pass "no leftover TAP device after Ctrl-C"
    fi

    vms_dir="${MVM_VMS_DIR:-$HOME/.mvm/vms}"
    if [ -d "$vms_dir" ] && find "$vms_dir" -maxdepth 2 -name 'extras' -type d 2>/dev/null | grep -q .; then
        fail "leftover extras/ staging dir under $vms_dir"
    else
        pass "no leftover staging dir under $vms_dir"
    fi
fi

# 6. `mvmctl up` boots the bundled default microVM (no flake/template)
if want default-up; then
    if "$mvmctl" up --help >/dev/null 2>&1; then
        if "$mvmctl" up >/tmp/mvmctl-up.log 2>&1; then
            pass "mvmctl up boots bundled default microVM"
            "$mvmctl" stop default >/dev/null 2>&1 || true
        else
            fail "mvmctl up failed (see /tmp/mvmctl-up.log)"
        fi
    else
        info "skipping (mvmctl up not in this build)"
    fi
fi

# 7. `nix build` of the bundled default-microvm flake
if want nix-build && command -v nix >/dev/null 2>&1; then
    sys=$(uname -m)
    case "$sys" in
        x86_64|amd64)  sys=x86_64-linux ;;
        aarch64|arm64) sys=aarch64-linux ;;
        *)             info "unknown arch '$sys' — skipping nix-build"; sys= ;;
    esac
    if [ -n "$sys" ] && [ -d nix/default-microvm ]; then
        if nix build "./nix/default-microvm#packages.$sys.default" --no-link 2>&1 | tail -5; then
            pass "nix build default-microvm.$sys"
        else
            fail "nix build default-microvm.$sys"
        fi
    fi
fi

# 8. Boot+exec+teardown latency on KVM (capture, no threshold)
if want latency; then
    if [ -e /dev/kvm ]; then
        t=$( { time "$mvmctl" exec --template "$default_template" -- /bin/true; } 2>&1 | awk '/^real/ {print $2}' )
        if [ -n "$t" ]; then
            pass "latency: $t  (target <2s once snapshot restore lands; tracked separately)"
        else
            fail "could not capture timing"
        fi
    else
        info "skipping latency (no /dev/kvm)"
    fi
fi

echo
if [ "$failures" -eq 0 ]; then
    printf '\033[32mAll smoke checks passed.\033[0m\n'
    exit 0
else
    printf '\033[31m%d smoke check(s) failed.\033[0m\n' "$failures"
    exit 1
fi
