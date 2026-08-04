#!/usr/bin/env bash
# test-wait-for-port-free.sh — smoke test for wait-for-port-free.sh (A1b/R2).
#
# Ephemeral, self-contained (no bats/test-framework dependency — matches the
# `scripts/smoke-*.sh` convention already used in this repo, see
# packaging/systemd/README.md). Exercises the 4 exit paths of the script:
#
#   1. port already free           -> exit 0 immediately
#   2. port occupied, freed later  -> exit 0 after waiting
#   3. port occupied permanently   -> exit 1 after WAIT_FOR_PORT_FREE_TIMEOUT_SECS
#   4. child_port unparsable (R2)  -> exit 1 immediately, fail-loud (not fail-open)
#
# Usage: bash packaging/systemd/test-wait-for-port-free.sh
# Exits 0 if all 4 scenarios pass, 1 otherwise (prints a PASS/FAIL line each).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$SCRIPT_DIR/wait-for-port-free.sh"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

PASS=0
FAIL=0

check() {
    local name="$1" expected_code="$2" actual_code="$3"
    if [[ "$actual_code" -eq "$expected_code" ]]; then
        echo "PASS: $name (exit $actual_code)"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $name (expected exit $expected_code, got $actual_code)"
        FAIL=$((FAIL + 1))
    fi
}

TEST_PORT=48081

# --- Scenario 1: port free from the start -> exit 0 ---
cat > "$TMPDIR/free.toml" <<EOF
[engine]
child_port = ${TEST_PORT}
EOF
"$SCRIPT" "$TMPDIR/free.toml" >/dev/null 2>&1
check "port already free" 0 $?

# --- Scenario 2: port occupied, released after ~1s -> exit 0 (waits) ---
python3 -c "
import socket, time
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', ${TEST_PORT}))
s.listen(1)
time.sleep(1.0)
s.close()
" &
OCCUPY_PID=$!
sleep 0.3
"$SCRIPT" "$TMPDIR/free.toml" >/dev/null 2>&1
check "port freed mid-wait" 0 $?
wait "$OCCUPY_PID" 2>/dev/null

# --- Scenario 3: port occupied permanently -> exit 1 after timeout ---
python3 -c "
import socket, time
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', ${TEST_PORT}))
s.listen(1)
time.sleep(4)
s.close()
" &
OCCUPY_PID=$!
sleep 0.3
WAIT_FOR_PORT_FREE_TIMEOUT_SECS=1 "$SCRIPT" "$TMPDIR/free.toml" >/dev/null 2>&1
check "port occupied permanently -> timeout" 1 $?
wait "$OCCUPY_PID" 2>/dev/null

# --- Scenario 4 (R2): child_port unparsable -> exit 1, fail-loud, not exit 0 ---
cat > "$TMPDIR/no-child-port.toml" <<EOF
[engine]
model_path = "/tmp/whatever.gguf"
# no child_port key at all — simulates a TOML format drift / renamed key
EOF
"$SCRIPT" "$TMPDIR/no-child-port.toml" >/dev/null 2>&1
check "R2: unparsable child_port -> fail-loud (exit 1, not 0)" 1 $?

echo ""
echo "=== $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
