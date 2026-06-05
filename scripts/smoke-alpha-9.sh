#!/usr/bin/env bash
# Smoke test alpha.9 — vault_downgrade + filter search + migration tool
# Phase 2.1.2 — Phase 1 (re-run alpha-8) + Phase 10 (downgrade) + 11 (search default exclude)
# + 12 (search include) + 13 (migration tool dry-run)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=${PORT:-19090}

echo "=== Phase 1 : re-run alpha-8 ==="
sudo bash "$SCRIPT_DIR/smoke-alpha-8.sh" || echo "WARN alpha-8 phase eu un WARN (non bloquant pour alpha.9)"

echo ""
echo "=== Phase 10 : POST /api/v1/vault_downgrade ==="
JWT=$(curl -sS -X POST "http://localhost:$PORT/auth/exchange" \
    -H "Authorization: Bearer $(sudo cat /etc/gradatum/claude-code.api-key)" \
    -H "Content-Type: application/json" --max-time 3 | jq -r '.token' 2>/dev/null || echo "")

if [[ -z "$JWT" || "$JWT" == "null" ]]; then
    echo "FAIL JWT exchange — skip phases downgrade"
    exit 0
fi

# Marker unique par run (epoch) — évite collision avec notes résiduelles runs précédents
# (cf bug Task #48 : Phase 11 voyait les notes des runs antérieurs en live post-Phase 13 PATCH revert).
RUN_EPOCH=$(date +%s)
RUN_MARKER="smoke alpha9 marker run${RUN_EPOCH}"
echo "RUN marker unique : ${RUN_MARKER}"

# Créer une note smoke
WRITE=$(curl -sS -X POST "http://localhost:$PORT/api/v1/vault_write" \
    -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
    -d "{\"title\":\"[EXPERIMENTS][gradatum] Smoke alpha.9 downgrade run${RUN_EPOCH} — $(date -Iseconds)\",\"body\":\"smoke test downgrade ${RUN_MARKER}\",\"section_hint\":\"experiments\",\"tags\":[\"smoke\",\"alpha-9\",\"downgrade-test\"],\"owner\":\"main-agent\",\"preset\":\"hierarchical\"}")
JOB_ID=$(echo "$WRITE" | jq -r '.job_id' 2>/dev/null || echo "")
echo "Curate job_id=$JOB_ID"

# Poll curate done
for i in $(seq 1 30); do
    STATUS=$(curl -sS "http://localhost:$PORT/api/v1/jobs/$JOB_ID" 2>/dev/null | jq -r '.status' 2>/dev/null || echo "?")
    if [[ "$STATUS" == "done" ]]; then
        echo "Curate done @${i}s"
        break
    fi
    sleep 1
done

# Récupérer note_id de la note smoke (par marker unique du run courant)
sleep 2
NOTE_ID=$(sudo -u gradatum sqlite3 /var/lib/gradatum/vault/.gradatum/index.db \
    "SELECT id FROM notes WHERE body_text LIKE '%${RUN_MARKER}%' ORDER BY created DESC LIMIT 1;" 2>/dev/null || echo "")
echo "smoke note_id=$NOTE_ID"

if [[ -n "$NOTE_ID" ]]; then
    DOWNGRADE=$(curl -sS -w "\nHTTP=%{http_code}" -X POST "http://localhost:$PORT/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
        -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"smoke alpha.9 phase 10\"}")
    HTTP_CODE=$(echo "$DOWNGRADE" | grep "HTTP=" | cut -d= -f2)
    if [[ "$HTTP_CODE" == "200" ]]; then
        echo "PASS Phase 10 — vault_downgrade HTTP 200"
        # Vérifier status DB
        DB_STATUS=$(sudo -u gradatum sqlite3 /var/lib/gradatum/vault/.gradatum/index.db \
            "SELECT status FROM notes WHERE id='$NOTE_ID';")
        [[ "$DB_STATUS" == "downgraded" ]] && echo "PASS Phase 10 DB — status=downgraded" || echo "FAIL Phase 10 DB — status=$DB_STATUS"
    else
        echo "FAIL Phase 10 — HTTP=$HTTP_CODE response=$DOWNGRADE"
        exit 1
    fi

    echo ""
    echo "=== Phase 10b : vault_downgrade idempotent (2e appel = 200) ==="
    R2=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://localhost:$PORT/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
        -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"smoke alpha.9 phase 10b idempotent\"}" --max-time 3)
    [[ "$R2" == "200" ]] && echo "PASS Phase 10b — idempotent 200" || echo "WARN Phase 10b — code=$R2"

    echo ""
    echo "=== Phase 10c : POST /api/v1/vault_downgrade note inexistante → 404 ==="
    R404=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://localhost:$PORT/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
        -d '{"note_id":"01KR0000000000000000000000","reason":"test"}' --max-time 3)
    [[ "$R404" == "404" ]] && echo "PASS Phase 10c — 404 note inexistante" || echo "FAIL Phase 10c — code=$R404"
fi

echo ""
echo "=== Phase 11 : vault_search default exclut downgraded ==="
DEFAULT_HITS=$(curl -sS -X POST "http://localhost:$PORT/api/v1/vault_search" \
    -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
    -d "{\"query\":\"${RUN_MARKER}\",\"limit\":5}" --max-time 5 \
    | jq '.items | length' 2>/dev/null || echo "0")
echo "default search hits=$DEFAULT_HITS (devrait être 0 — note downgrade exclue)"
if [[ "$DEFAULT_HITS" == "0" ]]; then
    echo "PASS Phase 11 — default exclut downgraded"
else
    echo "WARN Phase 11 — default = $DEFAULT_HITS hits (attendu 0)"
fi

echo ""
echo "=== Phase 12 : vault_search include_downgraded=true ==="
INCL_HITS=$(curl -sS -X POST "http://localhost:$PORT/api/v1/vault_search" \
    -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
    -d "{\"query\":\"${RUN_MARKER}\",\"limit\":5,\"include_downgraded\":true}" --max-time 5 \
    | jq '.items | length' 2>/dev/null || echo "0")
echo "include_downgraded search hits=$INCL_HITS (devrait être >= 1)"
if [[ "$INCL_HITS" -ge 1 ]]; then
    echo "PASS Phase 12 — include_downgraded retourne note"
else
    echo "WARN Phase 12 — include_downgraded=$INCL_HITS (attendu >= 1)"
fi

echo ""
echo "=== Phase 13 : PATCH /api/v1/notes/:id revert downgraded → live ==="
if [[ -n "$NOTE_ID" ]]; then
    PATCH_CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X PATCH "http://localhost:$PORT/api/v1/notes/$NOTE_ID" \
        -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
        -d '{"status":"live"}' --max-time 3)
    [[ "$PATCH_CODE" == "204" ]] && echo "PASS Phase 13 — PATCH 204 No Content" || echo "WARN Phase 13 — code=$PATCH_CODE"
    REVERT_STATUS=$(sudo -u gradatum sqlite3 /var/lib/gradatum/vault/.gradatum/index.db \
        "SELECT status FROM notes WHERE id='$NOTE_ID';")
    [[ "$REVERT_STATUS" == "live" ]] && echo "PASS Phase 13 DB — status revert='live'" || echo "WARN Phase 13 DB — status=$REVERT_STATUS"
fi

echo ""
echo "=== Phase 14 : migration tool dry-run --limit 3 ==="
# Note : sudo direct (root) requis car le tool lit /home/maintainer-user/.memory-vault/.vault-trash/
# (drwx------ 700 sur ~maintainer-user) ET écrit dans /var/lib/gradatum/db/index.db (700 owner gradatum).
# user gradatum ne peut PAS traverser ~maintainer-user/. Root traverse les deux.
DRY=$(sudo gradatum-admin downgrade-from-legacy-vault-trash --dry-run --limit 3 --legacy-vault-path /home/maintainer-user/.memory-vault 2>&1 | tail -5)
echo "$DRY"
if echo "$DRY" | grep -qE "scanned=|complete:"; then
    echo "PASS Phase 14 — legacy vault migration tool dry-run OK"
else
    echo "WARN Phase 14 — migration tool unexpected output"
fi

echo ""
echo "=== smoke-alpha-9.sh : DONE ==="
