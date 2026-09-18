#!/usr/bin/env bash
#
# run.sh — end-to-end harness for the privacy sidecar.
#
# Starts a real pf-sidecar against a real model, drives it with tests/client.py,
# compares every span against the reference comparator built from the same
# pinned upstream revision, and checks the log for leaked body bytes.
#
#   PF_MODEL=/path/to/model.gguf bash tools/privacy-sidecar/tests/run.sh
#
# Environment (no default points at a private path):
#   PF_MODEL                  required — path to the GGUF weights
#   PF_SIDECAR_BUILD_DIR      default $HOME/.cache/gradatum-privacy-sidecar
#   PF_REF_CLI                default the comparator built above; override to
#                             point at any other `pf-cli` exposing text mode
#                             (`--classify <model> <threshold>`, body on stdin)
#   PF_SOCKET_DIR             default a fresh mktemp -d (never /run)
#   PF_MAX_INPUT_BYTES        default 262144
#   PF_TIMEOUT_S              default 300 — budget for one client run
#
# Exit codes: 0 all checks hold; 1 a check failed; 2 the harness could not run
# (missing artefact) — which is NOT a pass.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
BUILD_DIR="${PF_SIDECAR_BUILD_DIR:-$HOME/.cache/gradatum-privacy-sidecar}"
SIDECAR="$BUILD_DIR/build/pf-sidecar"
REF_CLI="${PF_REF_CLI:-$BUILD_DIR/build/pf-ref-cli}"
MODEL="${PF_MODEL:-}"
MAX_INPUT_BYTES="${PF_MAX_INPUT_BYTES:-262144}"
TIMEOUT_S="${PF_TIMEOUT_S:-300}"
RC_DIR="${PF_RC_DIR:-$ROOT/.rc}"
mkdir -p "$RC_DIR"

if [ -z "$MODEL" ]; then
    echo "ERREUR : PF_MODEL non défini — poids introuvables, harnais NON EXÉCUTÉ." >&2
    exit 2
fi
SOCKET_DIR="${PF_SOCKET_DIR:-}"
if [ -z "$SOCKET_DIR" ]; then
    SOCKET_DIR="$(mktemp -d -t pf-sidecar.XXXXXX)"
    CLEAN_SOCKET_DIR=1
else
    CLEAN_SOCKET_DIR=0
fi
mkdir -p "$SOCKET_DIR"
SOCKET="$SOCKET_DIR/pf-sidecar.sock"
OUT="$RC_DIR/e2e"
mkdir -p "$OUT"
LOG="$OUT/sidecar.log"
: > "$LOG"

missing=0
for f in "$SIDECAR" "$REF_CLI" "$MODEL"; do
    if [ ! -e "$f" ]; then
        echo "ABSENT : $f" >&2
        missing=1
    fi
done
if [ "$missing" -ne 0 ]; then
    echo "Harnais NON EXÉCUTÉ — artefact manquant (à construire : fetch-upstream.sh + cmake)." >&2
    echo "  Ce n'est PAS un succès." >&2
    exit 2
fi

pf_pid=""
stop() {
    local pid="$1"
    [ -n "$pid" ] || return 0
    kill -TERM "$pid" 2>/dev/null || return 0
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.2
    done
    echo "INFO: le sidecar n'a pas quitté sur SIGTERM (pid=$pid) — SIGKILL." >&2
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}
cleanup() {
    stop "$pf_pid"
    [ "$CLEAN_SOCKET_DIR" -eq 1 ] && rm -rf "$SOCKET_DIR"
}
trap cleanup EXIT

# ── 1. self-check: the digests the operator pins, printed without serving ────
"$SIDECAR" --selfcheck --model "$MODEL" > "$OUT/selfcheck.txt" 2>"$OUT/selfcheck.err"
echo "$?" > "$RC_DIR/selfcheck.rc"
if [ "$(cat "$RC_DIR/selfcheck.rc")" != "0" ]; then
    echo "ÉCHEC : --selfcheck rc=$(cat "$RC_DIR/selfcheck.rc")" >&2
    sed -n '1,20p' "$OUT/selfcheck.err" >&2
    exit 1
fi
# The self-check must reproduce the digests the host itself computes.
sha256sum "$SIDECAR" | cut -d' ' -f1 > "$OUT/sha256sum-bin.txt"
sha256sum "$MODEL" | cut -d' ' -f1 > "$OUT/sha256sum-model.txt"
for pair in "binary $(cat "$OUT/sha256sum-bin.txt")" "model $(cat "$OUT/sha256sum-model.txt")"; do
    kind="${pair%% *}"; want="${pair##* }"
    grep -q "^${kind}_sha256=${want}$" "$OUT/selfcheck.txt" \
        || { echo "ÉCHEC : selfcheck ne reproduit pas le digest ${kind} ($want)" >&2; exit 1; }
done

# A pinned digest that does not match must stop the process, not warn. A sidecar
# that serves under an unpinned artefact is the failure this option exists for.
BOGUS="$(printf '0%.0s' $(seq 1 64))"
"$SIDECAR" --selfcheck --model "$MODEL" --expect-binary-sha256 "$BOGUS" \
    > "$OUT/refuse.out" 2>&1
echo "$?" > "$RC_DIR/refuse.rc"
if [ "$(cat "$RC_DIR/refuse.rc")" = "0" ]; then
    echo "ÉCHEC : un digest binaire divergent a été accepté (rc=0)" >&2
    exit 1
fi
grep -q "divergence" "$OUT/refuse.out" \
    || { echo "ÉCHEC : refus sans motif de divergence" >&2; cat "$OUT/refuse.out" >&2; exit 1; }
# And the matching pin must be accepted, so the refusal above is not vacuous.
"$SIDECAR" --selfcheck --model "$MODEL" \
    --expect-binary-sha256 "$(cat "$OUT/sha256sum-bin.txt")" \
    --expect-model-sha256 "$(cat "$OUT/sha256sum-model.txt")" \
    > "$OUT/accept.out" 2>&1
echo "$?" > "$RC_DIR/accept.rc"
[ "$(cat "$RC_DIR/accept.rc")" = "0" ] \
    || { echo "ÉCHEC : les digests épinglés corrects ont été refusés (rc=$(cat "$RC_DIR/accept.rc"))" >&2; exit 1; }

# ── 2. serve ────────────────────────────────────────────────────────────────
rm -f "$SOCKET"
"$SIDECAR" --socket "$SOCKET" --model "$MODEL" --threads 4 \
    --max-input-bytes "$MAX_INPUT_BYTES" >>"$LOG" 2>&1 &
pf_pid=$!

for _ in $(seq 1 600); do
    [ -S "$SOCKET" ] && break
    kill -0 "$pf_pid" 2>/dev/null || { echo "ÉCHEC : le sidecar est mort au démarrage" >&2; break; }
    sleep 0.5
done
if [ ! -S "$SOCKET" ]; then
    echo "ÉCHEC : socket $SOCKET jamais créée" >&2
    sed -n '1,30p' "$LOG" >&2
    exit 1
fi
stat -c '%a' "$SOCKET" > "$OUT/socket-mode.txt"
[ "$(cat "$OUT/socket-mode.txt")" = "660" ] \
    || { echo "ÉCHEC : permissions du socket = $(cat "$OUT/socket-mode.txt"), attendu 660" >&2; exit 1; }

# ── 3. drive the protocol ───────────────────────────────────────────────────
timeout "$TIMEOUT_S" python3 "$HERE/client.py" \
    --socket "$SOCKET" --model "$MODEL" --ref-cli "$REF_CLI" \
    --sidecar-bin "$SIDECAR" --log "$LOG" --fixtures "$HERE/fixtures.txt" \
    --max-input-bytes "$MAX_INPUT_BYTES" > "$OUT/client.out" 2>&1
echo "$?" > "$RC_DIR/client.rc"
cat "$OUT/client.out"

# ── 4. peak RSS: the kernel's own high-water mark, read before shutdown ─────
# /usr/bin/time is absent on this host; VmHWM is the kernel's monotonic peak
# and does not depend on a sampling interval.
if [ -r "/proc/$pf_pid/status" ]; then
    awk '/^VmHWM:/ {print $2}' "/proc/$pf_pid/status" > "$OUT/peak-rss-kb.txt"
fi
stop "$pf_pid"
pf_pid=""

rc=0
if [ "$(cat "$RC_DIR/client.rc")" != "0" ]; then
    echo "ÉCHEC : client rc=$(cat "$RC_DIR/client.rc")" >&2
    rc=1
fi
# A clean SIGTERM must leave no socket behind: a stale path is what makes the
# next start refuse to bind.
if [ -e "$SOCKET" ]; then
    echo "ÉCHEC : socket laissée derrière après SIGTERM" >&2
    rc=1
fi
if [ -s "$OUT/peak-rss-kb.txt" ]; then
    echo "peak_rss_kb=$(cat "$OUT/peak-rss-kb.txt")"
else
    echo "AVERTISSEMENT : pic RSS non relevé" >&2
fi
exit "$rc"
