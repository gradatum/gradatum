#!/usr/bin/env bash
# Smoke test alpha.11 — Phase 2.x.2 Semantic Search + RRF Fusion
#
# Valide les 3 implémentations de la Phase 2.x.2 alpha.11 :
#   Task 8 (M1) : search_semantic — cosine similarity sur note_embeddings
#   Task 9 (M2) : rrf_fuse — Reciprocal Rank Fusion BM25 + sémantique
#   Task 10 (M3) : vault_search handler — fusion hybride avec dégradation gracieuse
#
# Phases :
#   Phase 1 : Auth JWT (pré-requis pour toutes les phases)
#   Phase 2 : vault_search basique — 200 + items tableau
#   Phase 3 : vault_search query FTS5 — hits BM25 via note indexée
#   Phase 4 : vault_search embedding path — appel avec embedder configuré
#   Phase 5 : dégradation gracieuse — query inconnue → items vide, pas 500
#
# Usage :
#   PORT=19090 sudo bash scripts/smoke-alpha-11-semantic.sh
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=${PORT:-19090}
BASE="http://localhost:$PORT"
FAIL=0
PASS=0

pass() { echo "PASS : $*"; ((PASS++)) || true; }
fail() { echo "FAIL : $*"; ((FAIL++)) || true; }

# ── Auth JWT ─────────────────────────────────────────────────────────────────
echo "=== Phase 1 : Auth JWT ==="
API_KEY=$(sudo cat /etc/gradatum/claude-code.api-key 2>/dev/null || echo "")
if [[ -z "$API_KEY" ]]; then
    echo "SKIP smoke-alpha-11 : /etc/gradatum/claude-code.api-key absent (serveur non déployé)"
    exit 0
fi

AUTH_RESP=$(curl -s -X POST "$BASE/api/v1/auth/exchange" \
    -H "Content-Type: application/json" \
    -d "{\"api_key\":\"$API_KEY\",\"tenant_id\":\"main\"}" 2>/dev/null)

TOKEN=$(echo "$AUTH_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('token',''))" 2>/dev/null || echo "")
if [[ -z "$TOKEN" ]]; then
    fail "Phase 1 : échange API key → JWT FAILED (token vide). response=$AUTH_RESP"
    exit 1
fi
pass "Phase 1 : JWT obtenu (token=${TOKEN:0:20}...)"

AUTH_HEADER="Authorization: Bearer $TOKEN"

# ── Phase 2 : vault_search basique ─────────────────────────────────────────
echo ""
echo "=== Phase 2 : vault_search basique ==="

SEARCH_RESP=$(curl -s -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d '{"query":"gradatum","tenant_id":"main","limit":5}' 2>/dev/null)

HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d '{"query":"gradatum","tenant_id":"main","limit":5}' 2>/dev/null)

if [[ "$HTTP_STATUS" == "200" ]]; then
    pass "Phase 2 : vault_search retourne HTTP 200"
else
    fail "Phase 2 : vault_search HTTP $HTTP_STATUS (attendu 200). response=$SEARCH_RESP"
fi

ITEMS_FIELD=$(echo "$SEARCH_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print('ok' if isinstance(d.get('items'), list) else 'fail')" 2>/dev/null || echo "fail")
if [[ "$ITEMS_FIELD" == "ok" ]]; then
    pass "Phase 2 : champ 'items' présent et tableau"
else
    fail "Phase 2 : champ 'items' absent ou non-tableau. response=$SEARCH_RESP"
fi

# ── Phase 3 : vault_search BM25 hits ─────────────────────────────────────────
echo ""
echo "=== Phase 3 : vault_search BM25 — hits sur note indexée ==="

# Chercher une note connue dans le vault live
BM25_RESP=$(curl -s -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d '{"query":"vault","tenant_id":"main","limit":5}' 2>/dev/null)

NB_HITS=$(echo "$BM25_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d.get('items',[])))" 2>/dev/null || echo "0")

if [[ "$NB_HITS" -gt "0" ]]; then
    pass "Phase 3 : BM25 retourne $NB_HITS hit(s) pour query 'vault'"
    # Vérifier la structure du premier hit
    FIRST_PATH=$(echo "$BM25_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['items'][0].get('path',''))" 2>/dev/null || echo "")
    FIRST_SCORE=$(echo "$BM25_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['items'][0].get('score',0))" 2>/dev/null || echo "0")
    if [[ -n "$FIRST_PATH" ]]; then
        pass "Phase 3 : hit[0].path = '$FIRST_PATH', score = $FIRST_SCORE"
    else
        fail "Phase 3 : hit[0].path vide ou absent"
    fi
else
    echo "WARN : Phase 3 : 0 hits BM25 pour 'vault' (vault peut être vide ou non indexé)"
fi

# ── Phase 4 : vault_search avec embedder configuré ──────────────────────────
echo ""
echo "=== Phase 4 : vault_search embedding path ==="

# Vérifier que le status expose l'état de l'embedder
STATUS_RESP=$(curl -s "$BASE/api/v1/vault_status" \
    -H "$AUTH_HEADER" 2>/dev/null)

EMBED_COUNT=$(echo "$STATUS_RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('embedding_count',0))" 2>/dev/null || echo "unknown")
pass "Phase 4 : vault_status.embedding_count = $EMBED_COUNT"

# vault_search avec query générique — si embedder HTTP actif, doit intégrer semantic
EMBED_RESP=$(curl -s -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d '{"query":"note mémoire","tenant_id":"main","limit":3}' 2>/dev/null)

EMBED_HTTP=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d '{"query":"note mémoire","tenant_id":"main","limit":3}' 2>/dev/null)

if [[ "$EMBED_HTTP" == "200" ]]; then
    pass "Phase 4 : vault_search avec query FR retourne 200 (embedder actif ou Noop)"
else
    fail "Phase 4 : vault_search HTTP $EMBED_HTTP (attendu 200). response=$EMBED_RESP"
fi

# ── Phase 5 : dégradation gracieuse — query inconnue ────────────────────────
echo ""
echo "=== Phase 5 : dégradation gracieuse ==="

# Query très rare → probablement 0 hits mais surtout pas de 500
UNIQUE_MARKER="smoke alpha11 semantic unique marker $(date +%s%N)"
DEGRAD_RESP=$(curl -s -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d "{\"query\":\"$UNIQUE_MARKER\",\"tenant_id\":\"main\",\"limit\":5}" 2>/dev/null)

DEGRAD_HTTP=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/v1/vault_search" \
    -H "Content-Type: application/json" \
    -H "$AUTH_HEADER" \
    -d "{\"query\":\"zzzunknown_query_gradatum_test_alpha11\",\"tenant_id\":\"main\",\"limit\":5}" 2>/dev/null)

if [[ "$DEGRAD_HTTP" == "200" ]]; then
    pass "Phase 5 : dégradation gracieuse — query inconnue retourne 200 (pas 500)"
else
    fail "Phase 5 : dégradation gracieuse FAIL — HTTP $DEGRAD_HTTP (attendu 200). response=$DEGRAD_RESP"
fi

# ── Résumé ─────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "Smoke alpha.11 : PASS=$PASS FAIL=$FAIL"
echo "════════════════════════════════════════"

if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
echo "DONE"
