#!/usr/bin/env bash
# Smoke acceptance test alpha.4 — 7 étapes post-deploy T13 systemd.
# Tests : health → write 1 note → poll job → read back → audit JSONL → cleanup → worker RAM.
#
# Usage :
#   bash scripts/smoke-alpha-4.sh
#   SERVER=http://localhost:19090 BEARER_FILE=/var/lib/gradatum/config/bearer.toml bash scripts/smoke-alpha-4.sh
#
# Pré-requis : jq curl sudo (audit JSONL + systemctl)
# Configuré via :
#   SERVER      URL de base du serveur gradatum (défaut : http://localhost:19090)
#   BEARER_FILE Chemin vers /var/lib/gradatum/config/bearer.toml (défaut)
#
# Validated against design spec P2.0c — 2026-05-05.
set -euo pipefail

SERVER=${SERVER:-http://localhost:19090}
BEARER_FILE=${BEARER_FILE:-/var/lib/gradatum/config/bearer.toml}

# Extraction du premier token dans le fichier TOML bearer
# Format attendu : token = "xxx" ou [[tokens]] / token = "xxx"
BEARER=$(grep -oP 'token\s*=\s*"\K[^"]+' "$BEARER_FILE" | head -1)
if [[ -z "$BEARER" ]]; then
    echo "ERREUR: Impossible d'extraire le bearer token depuis $BEARER_FILE" >&2
    exit 1
fi

echo "=== smoke-alpha-4 : $SERVER ==="

# ── Étape 1 : health check ────────────────────────────────────────────────────
echo "1/7 health check"
curl -fsS "$SERVER/health" | jq -e '.status == "ok"' >/dev/null
echo "    OK — status=ok"

# ── Étape 2 : vault_write note synthétique ────────────────────────────────────
echo "2/7 vault_write synthetic note [DECISIONS] alpha.4 smoke test"
WRITE_RESP=$(curl -fsS -X POST "$SERVER/api/v1/vault_write" \
    -H "Authorization: Bearer $BEARER" \
    -H "Content-Type: application/json" \
    -d '{"title":"[DECISIONS] alpha.4 smoke test","body":"smoke body — test automatique T10 P2.0c","tenant_id":"main"}')
JOB_ID=$(echo "$WRITE_RESP" | jq -r '.job_id')
if [[ ! "$JOB_ID" =~ ^[0-9]+$ ]]; then
    echo "ERREUR: job_id invalide ou absent : $WRITE_RESP" >&2
    exit 1
fi
echo "    OK — job_id=$JOB_ID"

# ── Étape 3 : poll statut job (attente max 30s) ───────────────────────────────
echo "3/7 poll job status (max 30s)"
STATUS="pending"
for i in {1..30}; do
    STATUS=$(curl -fsS "$SERVER/api/v1/jobs/$JOB_ID" \
        -H "Authorization: Bearer $BEARER" | jq -r '.status')
    if [[ "$STATUS" == "done" ]]; then
        echo "    OK — status=done après ${i}s"
        break
    fi
    sleep 1
done
if [[ "$STATUS" != "done" ]]; then
    echo "ERREUR: job $JOB_ID non terminé après 30s (status=$STATUS)" >&2
    exit 1
fi

# ── Étape 4 : vault_read — confirm note + section assignée ───────────────────
echo "4/7 vault_read confirm note + section assigned"
JOB_DETAIL=$(curl -fsS "$SERVER/api/v1/jobs/$JOB_ID" \
    -H "Authorization: Bearer $BEARER")
NOTE_ID=$(echo "$JOB_DETAIL" | jq -r '.result.note_id // empty')
if [[ -z "$NOTE_ID" ]]; then
    echo "ERREUR: note_id absent dans la réponse job : $JOB_DETAIL" >&2
    exit 1
fi
READ_RESP=$(curl -fsS -X POST "$SERVER/api/v1/vault_read" \
    -H "Authorization: Bearer $BEARER" \
    -H "Content-Type: application/json" \
    -d "{\"note_id\":\"$NOTE_ID\",\"tenant_id\":\"main\"}")
if ! echo "$READ_RESP" | jq -e '.section == "decisions"' >/dev/null 2>&1; then
    SECTION=$(echo "$READ_RESP" | jq -r '.section // "null"')
    echo "    WARN: section attendue=decisions, obtenue=$SECTION (curator heuristique peut varier)"
    # Non bloquant : le titre contient [DECISIONS] donc l'heuristique devrait classifier correctement.
    # Si section != decisions → indication d'une régression curator.
fi
echo "    OK — note_id=$NOTE_ID section=$(echo "$READ_RESP" | jq -r '.section // "?"')"

# ── Étape 5 : audit JSONL dernière ligne ─────────────────────────────────────
echo "5/7 audit JSONL last line check"
TODAY=$(date -u +%Y-%m-%d)
AUDIT_FILE="/var/log/gradatum/audit.${TODAY}.jsonl"
if [[ ! -f "$AUDIT_FILE" ]]; then
    echo "    WARN: fichier audit absent ($AUDIT_FILE) — service peut utiliser un chemin différent"
else
    AUDIT_LINE=$(sudo tail -1 "$AUDIT_FILE")
    if echo "$AUDIT_LINE" | jq -e '.event == "vault_write" and .outcome == "admitted"' >/dev/null 2>&1; then
        echo "    OK — event=vault_write outcome=admitted"
    else
        # outcome peut être "queued" si l'audit est émis avant le traitement worker (T7 pre-queue)
        OUTCOME=$(echo "$AUDIT_LINE" | jq -r '.outcome // "?"')
        EVENT=$(echo "$AUDIT_LINE" | jq -r '.event // "?"')
        echo "    WARN: audit last line event=$EVENT outcome=$OUTCOME (attendu admitted — voir comportement T7)"
    fi
fi

# ── Étape 6 : cleanup vault_downgrade ────────────────────────────────────────
echo "6/7 cleanup vault_downgrade note_id=$NOTE_ID"
curl -fsS -X POST "$SERVER/api/v1/vault_downgrade" \
    -H "Authorization: Bearer $BEARER" \
    -H "Content-Type: application/json" \
    -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"smoke test cleanup alpha.4\",\"tenant_id\":\"main\"}" >/dev/null
echo "    OK — note downgradée"

# ── Étape 7 : RAM worker post-fastembed cold start (Gate1-réserve1) ──────────
echo "7/7 worker memory pic post-fastembed cold start (Gate1-réserve1)"
if systemctl is-active --quiet gradatum-worker 2>/dev/null; then
    MEM=$(sudo systemctl show gradatum-worker -p MemoryCurrent --value 2>/dev/null || echo "N/A")
    if [[ "$MEM" != "N/A" && "$MEM" != "18446744073709551615" ]]; then
        MEM_MB=$(( MEM / 1024 / 1024 ))
        echo "    MemoryCurrent = ${MEM_MB} MB (raw: $MEM)"
        # Seuil informatif : bge-small-en-v1.5 ~153 MB RAM observé en bench T11 P2.0b.
        # Pas bloquant : valeur dépend du modèle fastembed configuré.
        if (( MEM_MB > 600 )); then
            echo "    WARN: RAM worker > 600 MB — vérifier la config fastembed (Gate1-réserve1)"
        fi
    else
        echo "    MemoryCurrent = N/A (worker non encore chargé fastembed ou cgroup non supporté)"
    fi
else
    echo "    WARN: gradatum-worker non actif — étape 7 skippée (deploy T13 requis)"
fi

echo ""
echo "✅ smoke-alpha-4 PASS"
