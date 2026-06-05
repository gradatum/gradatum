#!/usr/bin/env bash
# Smoke test alpha.6 — RT5 (poll job réel) + BM25 sanity + sanitize_job_error
#
# Usage :
#   sudo bash scripts/smoke-alpha-6.sh
#   PORT=19091 bash scripts/smoke-alpha-6.sh
#
# Pré-requis : jq curl python3 (python3 pour la vérification de range BM25)
# Variables :
#   PORT    Port du serveur gradatum (défaut : 19090)
#
# Phases :
#   1 — auth/exchange (API key → JWT)
#   2 — RT5 : vault_write + poll jusqu'à done + transition leased observable
#   3 — RT5 : id inexistant → 404
#   4 — BM25 sanity : score dans [0,1]
#   5 — sanitize_job_error : last_error null ou code opaque
#
# Note : le script ne crée pas d'API key (contrairement à smoke-alpha-5).
# Il utilise la clé existante de main-agent (/etc/gradatum/claude-code.api-key).
# Si le worker est lent (LLM curator 35B ~30s/note), le poll timeout est 60s.
set -uo pipefail

PORT=${PORT:-19090}
SERVER="http://localhost:$PORT"

# ── Couleurs ANSI (TTY-aware) ─────────────────────────────────────────────────
if [[ -t 1 ]]; then
    GREEN="\033[0;32m"
    RED="\033[0;31m"
    YELLOW="\033[0;33m"
    RESET="\033[0m"
else
    GREEN=""
    RED=""
    YELLOW=""
    RESET=""
fi

# ── Compteurs ─────────────────────────────────────────────────────────────────
STEPS_PASS=0
STEPS_WARN=0
STEPS_FAIL=0

step_pass() { echo -e "    ${GREEN}PASS${RESET} — $1"; STEPS_PASS=$(( STEPS_PASS + 1 )); }
step_warn() { echo -e "    ${YELLOW}WARN${RESET} — $1"; STEPS_WARN=$(( STEPS_WARN + 1 )); }
step_fail() { echo -e "    ${RED}FAIL${RESET} — $1"; STEPS_FAIL=$(( STEPS_FAIL + 1 )); }

# ── Pré-requis ────────────────────────────────────────────────────────────────
for cmd in jq curl python3; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "ERREUR: pré-requis manquant : $cmd" >&2
        exit 1
    fi
done

echo "=== smoke-alpha-6 : $SERVER ==="
echo ""

# ── Phase 1 : auth/exchange ───────────────────────────────────────────────────
echo "Phase 1 : auth"
API_KEY=$(cat /etc/gradatum/claude-code.api-key 2>/dev/null || true)
if [[ -z "$API_KEY" ]]; then
    step_fail "/etc/gradatum/claude-code.api-key absent ou vide — vérifier le déploiement"
    echo -e "${RED}ABORT${RESET} — API key non disponible"
    exit 1
fi

JWT=$(curl -sS -X POST "$SERVER/auth/exchange" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    --max-time 5 2>/dev/null | jq -r '.token // empty')

if [[ "${#JWT}" -gt 100 ]]; then
    step_pass "auth/exchange OK (JWT ${#JWT} chars)"
else
    step_fail "auth/exchange échec ou token trop court (len=${#JWT})"
    echo -e "${RED}ABORT${RESET} — impossible de s'authentifier"
    exit 1
fi

# ── Phase 2 : RT5 — vault_write + poll → done ────────────────────────────────
echo ""
echo "Phase 2 : RT5 — write + poll → done"
WRITE_RESP=$(curl -sS -X POST "$SERVER/api/v1/vault_write" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    --max-time 5 \
    -d '{
        "title":"[EXPERIMENTS][gradatum] Smoke alpha.6 RT5",
        "body":"smoke test poll job real — alpha.6 RT5 validation",
        "section_hint":"experiments",
        "tags":["smoke","alpha-6","rt5"],
        "owner":"main-agent",
        "preset":"hierarchical"
    }' 2>/dev/null)

JOB_ID=$(echo "$WRITE_RESP" | jq -r '.job_id // empty')
if [[ -z "$JOB_ID" ]]; then
    step_fail "vault_write n'a pas retourné de job_id : $WRITE_RESP"
    echo -e "${RED}ABORT${RESET} — impossible de démarrer le poll"
    exit 1
fi
echo "    Job enqueued : $JOB_ID"

# Poll jusqu'à done (timeout 60s — curator LLM peut prendre ~30s)
SAW_LEASED=0
FINAL_STATUS=""
for i in $(seq 1 60); do
    POLL_RESP=$(curl -sS "$SERVER/api/v1/jobs/$JOB_ID" \
        --max-time 5 2>/dev/null)
    STATUS=$(echo "$POLL_RESP" | jq -r '.status // empty')
    echo "    Poll $i : status=${STATUS:-<vide>}"
    [[ "$STATUS" == "leased" ]] && SAW_LEASED=1
    if [[ "$STATUS" == "done" || "$STATUS" == "failed" ]]; then
        FINAL_STATUS="$STATUS"
        break
    fi
    sleep 1
done

if [[ "$FINAL_STATUS" == "done" ]]; then
    step_pass "RT5 poll done (leased_observed=$SAW_LEASED)"
elif [[ "$FINAL_STATUS" == "failed" ]]; then
    step_warn "RT5 job terminé en failed (worker KO ou curator timeout) — statut observable RT5 OK"
else
    step_fail "RT5 timeout 60s sans done/failed — poll bloqué ou worker arrêté"
fi

# ── Phase 3 : RT5 — id inexistant → 404 ──────────────────────────────────────
echo ""
echo "Phase 3 : RT5 — id inexistant → 404"
HTTP=$(curl -sS -o /dev/null -w '%{http_code}' \
    "$SERVER/api/v1/jobs/999999999" \
    --max-time 5 2>/dev/null)
if [[ "$HTTP" == "404" ]]; then
    step_pass "RT5 404 (id inexistant → 404 Not Found)"
else
    step_fail "RT5 404 attendu, reçu HTTP $HTTP"
fi

# ── Phase 4 : BM25 sanity ─────────────────────────────────────────────────────
echo ""
echo "Phase 4 : BM25 sanity"
SEARCH_RESP=$(curl -sS -X POST "$SERVER/api/v1/vault_search" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    --max-time 5 \
    -d '{"query":"smoke alpha","limit":3}' 2>/dev/null)

NB=$(echo "$SEARCH_RESP" | jq -r '.items | length // 0')
SCORE=$(echo "$SEARCH_RESP" | jq -r '.items[0].score // 0')
echo "    Hits: $NB, top score: $SCORE"

if [[ "$NB" -gt 0 ]] && python3 -c "
import sys
s = float(sys.argv[1])
sys.exit(0 if 0.0 < s <= 1.0 else 1)
" "$SCORE" 2>/dev/null; then
    step_pass "BM25 score dans (0, 1] — ranking réel actif"
elif [[ "$NB" -eq 0 ]]; then
    step_warn "BM25 : 0 résultats (note smoke pas encore indexée ou worker lent)"
else
    step_fail "BM25 score hors [0,1] : $SCORE"
fi

# ── Phase 5 : sanitize_job_error ─────────────────────────────────────────────
echo ""
echo "Phase 5 : sanitize_job_error"
# Vérification indirecte : si le job s'est terminé (done/failed), last_error
# doit être null (done) ou un code opaque (failed). Jamais un chemin FS brut.
FINAL_RESP=$(curl -sS "$SERVER/api/v1/jobs/$JOB_ID" \
    --max-time 5 2>/dev/null)
LAST_ERROR=$(echo "$FINAL_RESP" | jq -r '.last_error')
echo "    last_error : $LAST_ERROR"

if [[ "$LAST_ERROR" == "null" ]]; then
    step_pass "sanitize (last_error null — job done sans erreur)"
elif echo "$LAST_ERROR" | grep -qE "^(invalid_input|vault_error|storage_error|processing_error)$"; then
    step_pass "sanitize (code opaque : $LAST_ERROR)"
else
    step_fail "sanitize : last_error contient potentiellement une info brute : $LAST_ERROR"
fi

# ── Résumé ────────────────────────────────────────────────────────────────────
echo ""
echo "=== Résumé smoke-alpha-6 ==="
echo -e "    ${GREEN}PASS${RESET} : $STEPS_PASS"
echo -e "    ${YELLOW}WARN${RESET} : $STEPS_WARN"
echo -e "    ${RED}FAIL${RESET} : $STEPS_FAIL"

if [[ "$STEPS_FAIL" -gt 0 ]]; then
    echo ""
    echo -e "    ${RED}SMOKE FAILED${RESET} — $STEPS_FAIL test(s) en échec"
    exit 1
elif [[ "$STEPS_WARN" -gt 0 ]]; then
    echo ""
    echo -e "    ${YELLOW}SMOKE WARN${RESET} — $STEPS_PASS PASS / $STEPS_WARN WARN (vérifier worker)"
    exit 0
else
    echo ""
    echo -e "    ${GREEN}SMOKE PASS${RESET} — $STEPS_PASS/5 PASS"
    exit 0
fi
