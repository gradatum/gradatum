#!/usr/bin/env bash
# Smoke test alpha.12 — Phase 2.x.3 Multi-facteur scoring + Jina cross-encoder reranker.
#
# Valide les 4 Tasks de la Phase 2.x.3 :
#   Task 11 : recency_factor + pagerank_factor + composite_score (gradatum-search/scoring.rs)
#   Task 12 : backlink_count + get_note_created_and_indegree (gradatum-index/queries.rs)
#   Task 13 : composite scoring multi-facteur post-RRF (handler vault_search)
#   Task 14 : Jina cross-encoder reranker (NoopReranker default + JinaOnnxReranker feature)
#
# Phases (cf. spec rev2 §7.1) :
#   Phase 1 : /v1/messages/health → status:ok
#   Phase 2 : vault_search retourne ≥1 résultat
#   Phase 3 : scores composite ∈ [0.0, 1.0]
#   Phase 4 : scores décroissants (ordre composite ou reranker)
#   Phase 5 : query vide → items vide (pas de 500)
#   Phase 6 : query avec ponctuation → 200 (échappement FTS5)
#   Phase 7 : query clé → ≥3 résultats
#
# Usage :
#   PORT=19090 GRADATUM_BEARER=<token> bash scripts/smoke-alpha-12.sh
#
set -euo pipefail

PORT="${PORT:-19090}"
BASE="http://localhost:${PORT}"
TOKEN="${GRADATUM_BEARER:-$(cat ~/.gradatum/bearer_token 2>/dev/null || echo MISSING)}"
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

if [ "$TOKEN" = "MISSING" ]; then
    echo "ERROR : GRADATUM_BEARER non défini et ~/.gradatum/bearer_token absent" >&2
    exit 2
fi

# ── Phase 1 : endpoint health ────────────────────────────────────────────────
echo "=== Phase 1 : /health ==="
HEALTH=$(curl -sf "${BASE}/v1/messages/health" 2>/dev/null | jq -r '.status' 2>/dev/null || echo "KO")
if [ "$HEALTH" = "ok" ]; then
    pass "/v1/messages/health → status:ok"
else
    fail "/v1/messages/health → ${HEALTH}"
fi

# ── Phase 2 : vault_search retourne ≥1 résultat ─────────────────────────────
echo "=== Phase 2 : vault_search basique ==="
SEARCH=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum","tenant_id":"main","limit":5}' \
    "${BASE}/api/v1/vault_search" 2>/dev/null || echo '{"items":[]}')
N=$(echo "$SEARCH" | jq '.items | length' 2>/dev/null || echo 0)
if [ "$N" -ge 1 ]; then
    pass "vault_search retourne ${N} résultat(s)"
else
    fail "vault_search → 0 items (attendu ≥1)"
fi

# ── Phase 3 : scores ∈ [0.0, 1.0] (alpha.12 — composite ou reranker) ────────
echo "=== Phase 3 : scores ∈ [0.0, 1.0] ==="
if [ "$N" -ge 1 ]; then
    MAX_SCORE=$(echo "$SEARCH" | jq '[.items[].score] | max' 2>/dev/null || echo 0)
    MIN_SCORE=$(echo "$SEARCH" | jq '[.items[].score] | min' 2>/dev/null || echo -1)
    if awk "BEGIN{exit !(${MIN_SCORE} >= 0.0 && ${MAX_SCORE} <= 1.0)}"; then
        pass "scores ∈ [0.0, 1.0] (min=${MIN_SCORE}, max=${MAX_SCORE})"
    else
        fail "scores hors bornes : min=${MIN_SCORE}, max=${MAX_SCORE}"
    fi
else
    fail "Phase 3 SKIP (N=0 sur Phase 2)"
fi

# ── Phase 4 : scores décroissants ───────────────────────────────────────────
echo "=== Phase 4 : scores décroissants ==="
SCORES=$(echo "$SEARCH" | jq '[.items[].score]' 2>/dev/null || echo '[]')
DECREASING=$(echo "$SCORES" | jq 'to_entries | all(.value >= ((.[(.key|tonumber)+1] // -1)))' 2>/dev/null || echo false)
# Méthode plus simple : vérifier que sort DESC == array original
SORTED=$(echo "$SCORES" | jq 'sort | reverse')
if [ "$SCORES" = "$SORTED" ]; then
    pass "scores décroissants (composite ou reranker)"
else
    fail "scores non décroissants : ${SCORES}"
fi

# ── Phase 5 : query vide → items vide (pas de 500) ─────────────────────────
echo "=== Phase 5 : query vide ==="
EMPTY=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"","tenant_id":"main"}' \
    "${BASE}/api/v1/vault_search" 2>/dev/null | jq '.items | length' 2>/dev/null || echo -1)
if [ "$EMPTY" = "0" ]; then
    pass "query vide → 0 items (graceful)"
else
    fail "query vide → ${EMPTY} items (attendu 0, ou 500 caché)"
fi

# ── Phase 6 : query avec ponctuation → 200 ──────────────────────────────────
echo "=== Phase 6 : query ponctuation ==="
STATUS=$(curl -sf -o /dev/null -w "%{http_code}" -X POST \
    -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"alpha.12 multi-facteur","tenant_id":"main"}' \
    "${BASE}/api/v1/vault_search" 2>/dev/null || echo "ERR")
if [ "$STATUS" = "200" ]; then
    pass "query 'alpha.12 multi-facteur' → HTTP 200"
else
    fail "query ponctuation → HTTP ${STATUS}"
fi

# ── Phase 7 : query clé → ≥3 résultats ──────────────────────────────────────
echo "=== Phase 7 : query clé ==="
GD_N=$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"query":"gradatum search architecture","tenant_id":"main","limit":5}' \
    "${BASE}/api/v1/vault_search" 2>/dev/null | jq '.items | length' 2>/dev/null || echo 0)
if [ "$GD_N" -ge 3 ]; then
    pass "query 'gradatum search architecture' → ${GD_N} résultats (≥3)"
else
    fail "query clé → ${GD_N} résultats (attendu ≥3)"
fi

# ── Résumé ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Smoke alpha-12 : PASS=${PASS} FAIL=${FAIL} ==="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
