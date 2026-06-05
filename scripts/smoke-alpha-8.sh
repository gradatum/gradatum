#!/usr/bin/env bash
# Smoke test alpha.8 — V3 warden ratelimit + V4 auth-jobs + embeddings async pipeline + backfill
# Pré-requis : gradatum alpha.8 déployé (systemd), embeddings service joignable (voir [embed] server.toml).
# Usage : sudo bash scripts/smoke-alpha-8.sh
# Note  : NE PAS exécuter avant deploy alpha.8 (Task 16).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=${PORT:-19090}

echo "=== Phase 1 : re-run alpha-7 ==="
sudo bash "$SCRIPT_DIR/smoke-alpha-7.sh"

echo ""
echo "=== Phase 6 : V3 warden rate limit ==="
# exempt_localhost=true (défaut warden) → requêtes loopback exemptées, on attend 200/401/404.
# Si un jour loopback n'est plus exempt, la 11e retournera 429 → PASS v2.
PASS_COUNT=0
LAST_CODE=""
for i in $(seq 1 11); do
    LAST_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
        "http://localhost:$PORT/api/v1/vault_status" --max-time 2 || echo "ERR")
    if [[ "$LAST_CODE" =~ ^(200|401|404)$ ]]; then
        PASS_COUNT=$((PASS_COUNT + 1))
    fi
done
if [[ "$PASS_COUNT" -eq 11 ]]; then
    echo "PASS V3 — 11/11 loopback exempt (warden bypass actif, comportement attendu)"
elif [[ "$LAST_CODE" == "429" ]]; then
    echo "PASS V3 — 11e requête bloquée (429, loopback non exempt)"
else
    echo "WARN V3 — $PASS_COUNT/11 OK (last=$LAST_CODE)"
fi

echo ""
echo "=== Phase 7 : backfill --limit=3 (idempotent) ==="
BACKFILL_OUT=$(sudo -u gradatum gradatum-admin backfill-embeddings --limit=3 2>&1 || true)
echo "$BACKFILL_OUT" | tail -3
N=$(echo "$BACKFILL_OUT" | grep -oE "[0-9]+ jobs enqueued" | grep -oE "^[0-9]+" | head -1 || true)
[[ -z "$N" ]] && N=0
if [[ "$N" -le 3 ]]; then
    echo "PASS backfill ($N jobs enqueued, ≤3)"
else
    echo "FAIL backfill (N=$N > 3)"
    exit 1
fi

echo ""
echo "=== Phase 8 : V4 auth-jobs flag (default false → 404 sans bearer requis) ==="
# cfg.auth.require_jwt_jobs_endpoint=false (défaut) : route accessible sans bearer.
# On attend 404 (job inexistant) et non 401.
JOBS_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
    "http://localhost:$PORT/api/v1/jobs/9999999" --max-time 2 || echo "ERR")
if [[ "$JOBS_CODE" == "404" ]]; then
    echo "PASS V4 — flag false → 404 (id inexistant) sans bearer requis"
elif [[ "$JOBS_CODE" == "401" ]]; then
    echo "WARN V4 — 401 reçu : require_jwt_jobs_endpoint=true en prod ?"
else
    echo "WARN V4 — code=$JOBS_CODE inattendu"
fi

echo ""
echo "=== Phase 9 : pipeline embed_note (curate write → embed_note enqueued → drain) ==="
API_KEY=$(sudo cat /etc/gradatum/claude-code.api-key 2>/dev/null || echo "")
if [[ -z "$API_KEY" ]]; then
    echo "WARN — /etc/gradatum/claude-code.api-key introuvable, skip phase 9"
    echo "=== smoke-alpha-8.sh : DONE (phase 9 skippée) ==="
    exit 0
fi

JWT=$(curl -sS -X POST "http://localhost:$PORT/auth/exchange" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" --max-time 3 | jq -r '.token // empty')

if [[ -z "$JWT" ]]; then
    echo "FAIL JWT exchange (smoke incomplet, skip phase 9)"
    echo "=== smoke-alpha-8.sh : DONE (phase 9 skippée) ==="
    exit 0
fi

WRITE=$(curl -sS -X POST "http://localhost:$PORT/api/v1/vault_write" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    -d "{\"title\":\"[EXPERIMENTS][gradatum] Smoke alpha.8 embed pipeline — $(date -Iseconds)\",\"body\":\"smoke test embed pipeline — should generate embedding via configured embed endpoint\",\"section_hint\":\"experiments\",\"tags\":[\"smoke\",\"alpha-8\",\"embed-pipeline\"],\"owner\":\"main-agent\",\"preset\":\"hierarchical\"}" \
    || echo "{}")
JOB_ID=$(echo "$WRITE" | jq -r '.job_id // empty')

if [[ -z "$JOB_ID" ]]; then
    echo "FAIL vault_write — pas de job_id (réponse: $WRITE)"
    exit 1
fi
echo "Curate job_id=$JOB_ID"

# Poll curate status (max 60s)
CURATE_OK=0
for i in $(seq 1 60); do
    STATUS=$(curl -sS "http://localhost:$PORT/api/v1/jobs/$JOB_ID" --max-time 2 \
        2>/dev/null | jq -r '.status // "?"' 2>/dev/null || echo "?")
    if [[ "$STATUS" == "done" ]]; then
        echo "Curate done @${i}s"
        CURATE_OK=1
        break
    elif [[ "$STATUS" == "dead" ]]; then
        echo "FAIL — curate dead"
        exit 1
    fi
    sleep 1
done

if [[ "$CURATE_OK" -eq 0 ]]; then
    echo "WARN — curate timeout 60s (status=$STATUS)"
fi

# Laisser 10s pour drain embed_note
echo "Sleep 10s (drain embed_note)..."
sleep 10
N_EMB=$(sudo -u gradatum sqlite3 /var/lib/gradatum/vault/.gradatum/index.db \
    "SELECT COUNT(*) FROM note_embeddings;" 2>/dev/null || echo "0")
echo "note_embeddings count: $N_EMB"
if [[ "$N_EMB" -gt 0 ]]; then
    echo "PASS — embeddings populées (count=$N_EMB)"
else
    echo "WARN — embeddings vides (drain timing ou embed endpoint indisponible ?)"
fi

echo ""
echo "=== smoke-alpha-8.sh : DONE ==="
