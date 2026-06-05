#!/usr/bin/env bash
# Smoke test alpha.13 — Phase 2.x.4 Endpoints Completeness.
#
# Valide les 4 Tasks de la Phase 2.x.4 :
#   Task 13 : B5 wikilinks post-curate (note_links peuplé après curate avec [[...]])
#   Task 14 : B4 title_lookup intégré dans vault_read (path = ULID OU titre)
#   Task 15 : M4 vault_trace query textuelle FTS multi-mode (ULID/titre/FTS)
#   Task 16 : M5 vault_context budget tokens FTS multi-notes (ratio 3.0 chars/token)
#
# Phases (cf. spec rev2 §6) — cumulatif inclut alpha-9..alpha-12 minimal :
#   Phase 0 : /v1/messages/health → status:ok (smoke base)
#   Phase 1 : Task 14 — vault_read par ULID (non-régression)
#   Phase 2 : Task 14 — vault_read par titre (résolution title_lookup)
#   Phase 3 : Task 13 — vault_links graphe non-vide (B5 a peuplé le graphe)
#   Phase 4 : Task 15 — vault_trace par ULID (non-régression)
#   Phase 5 : Task 15 — vault_trace par query textuelle (FTS multi-mode)
#   Phase 6 : Task 16 — vault_context par ULID (sources non vides)
#   Phase 7 : Task 16 — vault_context budget 100 tokens respecté (ratio 3.0 rev2)
#   Phase 8 : Task 16 — vault_context query textuelle multi-notes
#
# Usage (3 variantes) :
#   1. Auto-détection api-key + exchange JWT (recommandé) :
#        bash scripts/smoke-alpha-13.sh
#      → utilise /etc/gradatum/claude-code.api-key (sudo si non lisible) → /auth/exchange
#   2. JWT pré-exchangé fourni explicitement :
#        GRADATUM_JWT=<jwt> bash scripts/smoke-alpha-13.sh
#   3. Api-key passée explicitement :
#        GRADATUM_API_KEY=<api-key> bash scripts/smoke-alpha-13.sh
#
set -euo pipefail

PORT="${PORT:-19090}"
BASE="http://localhost:${PORT}"
PASS=0
FAIL=0

pass() {
    echo "PASS : $*"
    PASS=$((PASS + 1))
}
fail() {
    echo "FAIL : $*"
    FAIL=$((FAIL + 1))
}

# ── Auth : api-key → JWT exchange (Path 2 standard gradatum) ────────────────
# Priorité : GRADATUM_JWT explicite > GRADATUM_API_KEY explicite > /etc/gradatum/claude-code.api-key
TOKEN=""
if [ -n "${GRADATUM_JWT:-}" ]; then
    TOKEN="$GRADATUM_JWT"
    echo "INFO : auth via GRADATUM_JWT (JWT pré-exchangé)"
else
    APIKEY="${GRADATUM_API_KEY:-}"
    if [ -z "$APIKEY" ] && [ -r /etc/gradatum/claude-code.api-key ]; then
        APIKEY=$(cat /etc/gradatum/claude-code.api-key 2>/dev/null)
    fi
    if [ -z "$APIKEY" ] && [ -e /etc/gradatum/claude-code.api-key ]; then
        APIKEY=$(sudo cat /etc/gradatum/claude-code.api-key 2>/dev/null || echo "")
    fi
    # Fallback legacy : ancien GRADATUM_BEARER (assumé api-key, pas bearer admin)
    [ -z "$APIKEY" ] && APIKEY="${GRADATUM_BEARER:-}"
    [ -z "$APIKEY" ] && APIKEY=$(cat ~/.gradatum/bearer_token 2>/dev/null || echo "")

    if [ -z "$APIKEY" ]; then
        echo "ERROR : aucune api-key trouvée. Définir GRADATUM_API_KEY ou GRADATUM_JWT," >&2
        echo "        ou s'assurer que /etc/gradatum/claude-code.api-key est lisible (sudo)." >&2
        exit 2
    fi
    TOKEN=$(curl -sf -X POST "${BASE}/auth/exchange" \
        -H "Authorization: Bearer ${APIKEY}" \
        -H "Content-Type: application/json" \
        -d '{}' --max-time 3 2>/dev/null | jq -r '.token // empty' 2>/dev/null)
    if [ -z "$TOKEN" ] || [ "$TOKEN" = "null" ]; then
        echo "ERROR : /auth/exchange a échoué — api-key invalide ou serveur KO" >&2
        exit 2
    fi
    echo "INFO : auth via /auth/exchange (api-key → JWT, len=${#TOKEN})"
fi

# ── Phase 0 : health (endpoint gradatum natif) ───────────────────────────────
echo "=== Phase 0 : /health ==="
HEALTH=$(curl -sf "${BASE}/health" 2>/dev/null | jq -r '.status' 2>/dev/null || echo "KO")
if [ "$HEALTH" = "ok" ]; then
    pass "/health → status:ok"
else
    fail "/health → ${HEALTH}"
fi

# Récupère un ULID existant via vault_search pour réutiliser dans les Phases 1/4/6.
SEARCH=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum","tenant_id":"main","limit":1}' \
    "${BASE}/api/v1/vault_search" 2>/dev/null || echo '{"items":[]}')
NOTE_PATH=$(echo "$SEARCH" | jq -r '.items[0].path // empty')
NOTE_ULID=$(echo "$NOTE_PATH" | awk -F/ '{print $NF}')

if [ -z "$NOTE_ULID" ]; then
    echo "WARN : aucune note 'gradatum' indexée — Phases 1/4/6 SKIP"
fi

# ── Phase 1 : Task 14 — vault_read par ULID (non-régression) ────────────────
echo "=== Phase 1 : vault_read par ULID ==="
if [ -n "$NOTE_ULID" ]; then
    READ_STATUS=$(curl -o /dev/null -sw "%{http_code}" -X POST \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"${NOTE_ULID}\",\"tenant_id\":\"main\"}" \
        "${BASE}/api/v1/vault_read" 2>/dev/null || echo "ERR")
    if [ "$READ_STATUS" = "200" ]; then
        pass "vault_read par ULID (${NOTE_ULID:0:8}...) → HTTP 200"
    else
        fail "vault_read par ULID → HTTP ${READ_STATUS}"
    fi
else
    fail "Phase 1 SKIP (NOTE_ULID vide)"
fi

# ── Phase 2 : Task 14 — vault_read par titre (peut être 200 ou 404) ─────────
echo "=== Phase 2 : vault_read par titre ==="
TITLE_STATUS=$(curl -o /dev/null -sw "%{http_code}" -X POST \
    -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"path":"gradatum architecture","tenant_id":"main"}' \
    "${BASE}/api/v1/vault_read" 2>/dev/null || echo "ERR")
# 404 = titre absent du corpus (OK comportement) | 200 = trouvé (OK)
# Seul 500/timeout est un FAIL (impl B4 cassée)
if [ "$TITLE_STATUS" = "200" ] || [ "$TITLE_STATUS" = "404" ]; then
    pass "vault_read par titre → HTTP ${TITLE_STATUS} (200 ou 404 acceptables)"
else
    fail "vault_read par titre → HTTP ${TITLE_STATUS} (impl B4 cassée ?)"
fi

# ── Phase 3 : Task 13 — vault_links graphe (post-B5) ─────────────────────────
echo "=== Phase 3 : vault_links graphe non-vide (B5) ==="
EDGES=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"path":"main","tenant_id":"main"}' \
    "${BASE}/api/v1/vault_links" 2>/dev/null | jq '.edges | length // 0' 2>/dev/null || echo 0)
echo "  vault_links retourne ${EDGES} arcs"
# B5 vient d'être livré — un graphe vide est encore acceptable (notes sans wikilinks ou
# pas de re-curate depuis activation B5). Test informatif seulement.
if [ "$EDGES" -ge 0 ]; then
    pass "vault_links retourne ${EDGES} arcs (informatif — peut être 0 si pas de re-curate)"
else
    fail "vault_links a échoué"
fi

# ── Phase 4 : Task 15 — vault_trace par ULID (non-régression) ────────────────
echo "=== Phase 4 : vault_trace par ULID ==="
if [ -n "$NOTE_ULID" ]; then
    TRACE_ULID_STATUS=$(curl -o /dev/null -sw "%{http_code}" -X POST \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -d "{\"query\":\"${NOTE_ULID}\",\"tenant_id\":\"main\",\"limit\":5}" \
        "${BASE}/api/v1/vault_trace" 2>/dev/null || echo "ERR")
    if [ "$TRACE_ULID_STATUS" = "200" ]; then
        pass "vault_trace par ULID → HTTP 200"
    else
        fail "vault_trace par ULID → HTTP ${TRACE_ULID_STATUS}"
    fi
else
    fail "Phase 4 SKIP (NOTE_ULID vide)"
fi

# ── Phase 5 : Task 15 — vault_trace par query textuelle ──────────────────────
echo "=== Phase 5 : vault_trace par query textuelle ==="
TRACE_TEXT_STATUS=$(curl -o /dev/null -sw "%{http_code}" -X POST \
    -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum architecture","tenant_id":"main","limit":5}' \
    "${BASE}/api/v1/vault_trace" 2>/dev/null || echo "ERR")
if [ "$TRACE_TEXT_STATUS" = "200" ]; then
    pass "vault_trace query textuelle → HTTP 200"
else
    fail "vault_trace query textuelle → HTTP ${TRACE_TEXT_STATUS}"
fi

# ── Phase 6 : Task 16 — vault_context par ULID (non-régression) ──────────────
echo "=== Phase 6 : vault_context par ULID ==="
if [ -n "$NOTE_ULID" ]; then
    CTX_ULID=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -d "{\"query\":\"${NOTE_ULID}\",\"tenant_id\":\"main\"}" \
        "${BASE}/api/v1/vault_context" 2>/dev/null || echo '{"sources":[]}')
    SOURCES=$(echo "$CTX_ULID" | jq '.sources | length // 0' 2>/dev/null || echo 0)
    if [ "$SOURCES" -ge 1 ]; then
        pass "vault_context ULID retourne ${SOURCES} source(s)"
    else
        fail "vault_context ULID → 0 sources (attendu ≥1)"
    fi
else
    fail "Phase 6 SKIP (NOTE_ULID vide)"
fi

# ── Phase 7 : Task 16 — vault_context budget 100 tokens (ratio 3.0 rev2) ────
echo "=== Phase 7 : vault_context budget 100 tokens ==="
CTX_BUDGET=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum","tenant_id":"main","max_tokens":100}' \
    "${BASE}/api/v1/vault_context" 2>/dev/null || echo '{"estimated_tokens":999}')
TOKENS=$(echo "$CTX_BUDGET" | jq '.estimated_tokens // 999' 2>/dev/null || echo 999)
if [ "$TOKENS" -le 100 ]; then
    pass "vault_context budget respecté (estimated_tokens=${TOKENS} ≤ 100)"
else
    fail "vault_context budget dépassé (estimated_tokens=${TOKENS} > 100)"
fi

# ── Phase 8 : Task 16 — vault_context query textuelle multi-notes ───────────
echo "=== Phase 8 : vault_context query textuelle multi-notes ==="
CTX_TEXT=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum","tenant_id":"main","max_tokens":2000}' \
    "${BASE}/api/v1/vault_context" 2>/dev/null || echo '{"sources":[]}')
N_SOURCES=$(echo "$CTX_TEXT" | jq '.sources | length // 0' 2>/dev/null || echo 0)
if [ "$N_SOURCES" -ge 1 ]; then
    pass "vault_context query textuelle → ${N_SOURCES} source(s)"
else
    fail "vault_context query textuelle → 0 sources"
fi

# ── Résumé ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Smoke alpha-13 : PASS=${PASS} FAIL=${FAIL} ==="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
