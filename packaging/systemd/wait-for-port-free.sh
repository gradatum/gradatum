#!/usr/bin/env bash
# wait-for-port-free.sh — ExecStartPre guard for gradatum-engine@.service.
#
# Closes the port-race root cause behind incident 2026-07-08 20:47 (deep engine,
# GPU engine host): on `systemctl restart`, the new instance's ExecStart could race the
# previous instance's ExecStopPost (`fuser -k <child_port>`) and start binding
# child_port before it was actually released, making llama-server fail to bind
# and exit immediately (bind-fail). This script makes ExecStart wait until
# child_port is provably free instead of hoping ExecStopPost already ran.
#
# Usage: wait-for-port-free.sh <path-to-70-engine-*.toml>
#
# Reads `child_port` from the given TOML config, polls it every 0.2s until no
# listener is bound (loopback), and exits 0 as soon as it's free. Exits 1 if
# still occupied after WAIT_TIMEOUT_SECS, OR if `child_port` cannot be parsed
# from the config at all (fail-loud, not fail-open — see the check below).
# Either way systemd reports the unit as failed to start —
# `Restart=on-failure`/`RestartSec` on the unit takes over, same escalation
# path as the child_restart_max exhaustion fix in supervisor.rs).
#
# Deliberately dependency-light: only `grep`/`ss`, both already present on the
# GPU engine host (confirmed 2026-07-10, read-only check).

set -euo pipefail

CONFIG_PATH="${1:?usage: wait-for-port-free.sh <path-to-conf.toml>}"
WAIT_TIMEOUT_SECS="${WAIT_FOR_PORT_FREE_TIMEOUT_SECS:-10}"
POLL_INTERVAL_SECS="0.2"

if [[ ! -f "$CONFIG_PATH" ]]; then
    echo "wait-for-port-free.sh: config not found: $CONFIG_PATH" >&2
    exit 1
fi

child_port="$(grep -oP '^\s*child_port\s*=\s*\K[0-9]+' "$CONFIG_PATH" || true)"

if [[ -z "$child_port" ]]; then
    # R2 fix (reviewer finding on d8b8b28): fail-loud, not fail-open. A silently
    # unparsable child_port (regex miss, TOML format drift, key renamed) used to
    # `exit 0` here — the guard disarms itself with zero signal, and we're back
    # to the exact port-race this script exists to close, just quietly. Exiting
    # 1 instead makes ExecStartPre fail, systemd reports the unit as failed to
    # start (visible in `systemctl status`/`journalctl`), and `Restart=on-failure`
    # takes over — same escalation path as everywhere else in this fix, never a
    # silent no-op.
    echo "wait-for-port-free.sh: no child_port found in $CONFIG_PATH — failing loud \
(regex miss or TOML format drift), not disarming the guard silently" >&2
    exit 1
fi

deadline=$(( $(date +%s%N) / 1000000 + WAIT_TIMEOUT_SECS * 1000 ))

while true; do
    # Loopback-only check (child_port is always bound to 127.0.0.1 — see
    # supervisor.rs build_child_args, --host 127.0.0.1 is authoritative).
    if ! ss -H -tln "sport = :${child_port}" 2>/dev/null | grep -q .; then
        echo "wait-for-port-free.sh: child_port ${child_port} is free" >&2
        exit 0
    fi

    now_ms=$(( $(date +%s%N) / 1000000 ))
    if (( now_ms >= deadline )); then
        echo "wait-for-port-free.sh: child_port ${child_port} still occupied after ${WAIT_TIMEOUT_SECS}s — giving up" >&2
        exit 1
    fi

    sleep "$POLL_INTERVAL_SECS"
done
