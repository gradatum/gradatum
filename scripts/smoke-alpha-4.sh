#!/usr/bin/env bash
# Smoke acceptance test alpha.4 — 7 étapes post-deploy T13 systemd.
# Tests : health → write 1 note → poll job → read back → audit JSONL → cleanup → worker RAM.
#
# Usage :
#   bash scripts/smoke-alpha-4.sh
#   SERVER=http://localhost:19090 GRADATUM_API_KEY=<api-key> bash scripts/smoke-alpha-4.sh
#   API_KEY_FILE=/etc/gradatum/gradatum-mcp.api-key bash scripts/smoke-alpha-4.sh
#
# Pré-requis : jq curl + UNE api-key lisible (voir « Authentification » ci-dessous).
#              sudo optionnel (audit JSONL owned by gradatum, systemctl show).
# Configuré via :
#   SERVER            URL de base du serveur gradatum (défaut : http://localhost:19090)
#   GRADATUM_API_KEY  api-key en clair (prioritaire — permet un run non privilégié)
#   API_KEY_FILE      fichier contenant l'api-key (défaut : /etc/gradatum/gradatum-mcp.api-key)
#
# AUTHENTIFICATION (corrigé lot I-002, 2026-07-30) — flux moderne, identique à
#   smoke-alpha-5.sh : api-key → POST /auth/exchange → JWT. L'extraction historique
#   `grep -oP 'token\s*=\s*"\K[^"]+' bearer.toml` était morte : bearer.toml n'a plus
#   de clé `token` (c'est un descripteur d'ACL en blocs [[consumer]]). Combinée à
#   `set -euo pipefail`, elle tuait le script AVANT son propre message d'erreur —
#   rc=1, log de 0 octet (mesuré). La variable BEARER_FILE n'est plus lue.
#
#   Le fichier api-key est en 0600 owner gradatum : un compte non privilégié ne peut
#   pas le lire. Deux voies documentées, toutes deux non privilégiées côté script :
#     - GRADATUM_API_KEY=<clé> en environnement (recommandé pour un tiers) ;
#     - un `sudo -n cat` tenté en dernier recours si le compte y a droit.
#   Aucune de ces voies ne peut être devinée : sans clé, le script sort 2
#   (« gate NON EXÉCUTÉ »), jamais 0.
#
# VERDICT (corrigé lot I-001/I-003) : un WARN ne contribue JAMAIS à un PASS, et un
#   run comportant une étape non exécutée est INCOMPLETE, jamais PASS.
#   Exit 0 = PASS (7/7 vérifiées) · 1 = FAIL · 2 = INCOMPLETE / gate non exécuté.
#
# Validated against design spec P2.0c — 2026-05-05. Auth corrigée 2026-07-30.
set -euo pipefail

SERVER=${SERVER:-http://localhost:19090}
API_KEY_FILE=${API_KEY_FILE:-/etc/gradatum/gradatum-mcp.api-key}
STEPS_TOTAL=7

STEPS_PASS=0
STEPS_WARN=0
STEPS_FAIL=0
STEPS_SKIP=0

step_pass() { echo "    OK — $1";   STEPS_PASS=$(( STEPS_PASS + 1 )); }
step_warn() { echo "    WARN — $1"; STEPS_WARN=$(( STEPS_WARN + 1 )); }
step_fail() { echo "    FAIL — $1" >&2; STEPS_FAIL=$(( STEPS_FAIL + 1 )); }
step_skip() { echo "    SKIP — $1"; STEPS_SKIP=$(( STEPS_SKIP + 1 )); }

# ── Résolution de l'api-key ────────────────────────────────────────────────────
# Sort non-zéro sans tuer l'appelant : invoquée en contexte `if !`, où `set -e`
# est désactivé. C'est ce qui rend le message d'erreur ATTEIGNABLE.
resolve_api_key() {
    if [[ -n "${GRADATUM_API_KEY:-}" ]]; then
        printf '%s' "$GRADATUM_API_KEY"
        return 0
    fi
    if [[ -r "$API_KEY_FILE" ]]; then
        tr -d '\n' < "$API_KEY_FILE"
        return 0
    fi
    if [[ -e "$API_KEY_FILE" ]] && sudo -n true 2>/dev/null; then
        sudo -n cat "$API_KEY_FILE" 2>/dev/null | tr -d '\n'
        return 0
    fi
    return 1
}

for cmd in jq curl; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "ERREUR: pré-requis manquant : $cmd — gate NON EXÉCUTÉ, pas un PASS" >&2
        exit 2
    fi
done

if ! API_KEY=$(resolve_api_key) || [[ -z "$API_KEY" ]]; then
    echo "ERREUR: aucune api-key exploitable — gate NON EXÉCUTÉ, pas un PASS" >&2
    echo "        Essayé : \$GRADATUM_API_KEY, puis $API_KEY_FILE (lecture directe, puis sudo -n)." >&2
    echo "        Fournir la clé : GRADATUM_API_KEY=<clé> bash scripts/smoke-alpha-4.sh" >&2
    exit 2
fi

echo "=== smoke-alpha-4 : $SERVER ==="

# ── Échange api-key → JWT (flux moderne, cf. smoke-alpha-5.sh) ────────────────
if ! EXCHANGE_RESP=$(curl -fsS -X POST "$SERVER/auth/exchange" \
        -H "Authorization: Bearer $API_KEY" \
        -H "Content-Type: application/json" \
        --max-time 10 -d '{}' 2>&1); then
    echo "ERREUR: POST $SERVER/auth/exchange a échoué : $EXCHANGE_RESP" >&2
    echo "        gate NON EXÉCUTÉ, pas un PASS" >&2
    exit 2
fi
BEARER=$(printf '%s' "$EXCHANGE_RESP" | jq -r '.token // empty')
if [[ -z "$BEARER" ]]; then
    echo "ERREUR: JWT absent de la réponse /auth/exchange : $EXCHANGE_RESP" >&2
    echo "        gate NON EXÉCUTÉ, pas un PASS" >&2
    exit 2
fi

# ── Étape 1 : health check ────────────────────────────────────────────────────
echo "1/7 health check"
if HEALTH_RESP=$(curl -fsS --max-time 10 "$SERVER/health" 2>&1) \
        && printf '%s' "$HEALTH_RESP" | jq -e '.status == "ok"' >/dev/null 2>&1; then
    step_pass "status=ok"
else
    step_fail "health KO : $HEALTH_RESP"
fi

# ── Étape 2 : vault_write note synthétique ────────────────────────────────────
echo "2/7 vault_write synthetic note [DECISIONS] alpha.4 smoke test"
JOB_ID=""
if ! WRITE_RESP=$(curl -fsS -X POST "$SERVER/api/v1/vault_write" \
        -H "Authorization: Bearer $BEARER" \
        -H "Content-Type: application/json" \
        --max-time 15 \
        -d '{"title":"[DECISIONS] alpha.4 smoke test","body":"smoke body — test automatique T10 P2.0c","tenant_id":"main"}' 2>&1); then
    step_fail "vault_write a échoué : $WRITE_RESP"
else
    JOB_ID=$(printf '%s' "$WRITE_RESP" | jq -r '.job_id // empty')
    if [[ ! "$JOB_ID" =~ ^[0-9]+$ ]]; then
        step_fail "job_id invalide ou absent : $WRITE_RESP"
        JOB_ID=""
    else
        step_pass "job_id=$JOB_ID"
    fi
fi

# ── Étape 3 : poll statut job (attente max 30s) ───────────────────────────────
echo "3/7 poll job status (max 30s)"
NOTE_ID=""
if [[ -z "$JOB_ID" ]]; then
    step_skip "étape 3 non exécutée — pas de job_id (dépend de l'étape 2)"
else
    STATUS="pending"
    for i in $(seq 1 30); do
        JOB_DETAIL=$(curl -fsS --max-time 10 "$SERVER/api/v1/jobs/$JOB_ID" \
            -H "Authorization: Bearer $BEARER" 2>/dev/null) || JOB_DETAIL='{"status":"error"}'
        STATUS=$(printf '%s' "$JOB_DETAIL" | jq -r '.status // "error"')
        if [[ "$STATUS" == "done" ]]; then
            step_pass "status=done après ${i}s"
            NOTE_ID=$(printf '%s' "$JOB_DETAIL" | jq -r '.result.note_id // empty')
            break
        fi
        sleep 1
    done
    if [[ "$STATUS" != "done" ]]; then
        step_fail "job $JOB_ID non terminé après 30s (status=$STATUS)"
    fi
fi

# ── Étape 4 : vault_read — confirm note + section assignée ───────────────────
echo "4/7 vault_read confirm note + section assigned"
if [[ -z "$NOTE_ID" ]]; then
    step_skip "étape 4 non exécutée — note_id indisponible (dépend de l'étape 3)"
else
    if ! READ_RESP=$(curl -fsS -X POST "$SERVER/api/v1/vault_read" \
            -H "Authorization: Bearer $BEARER" \
            -H "Content-Type: application/json" \
            --max-time 10 \
            -d "{\"note_id\":\"$NOTE_ID\",\"tenant_id\":\"main\"}" 2>&1); then
        step_fail "vault_read a échoué pour note_id=$NOTE_ID : $READ_RESP"
    else
        SECTION=$(printf '%s' "$READ_RESP" | jq -r '.section // empty')
        if [[ "$SECTION" == "decisions" ]]; then
            step_pass "note_id=$NOTE_ID section=decisions"
        elif [[ -n "$SECTION" ]]; then
            # Le titre porte [DECISIONS] : une autre section signale une dérive du
            # curator. Non tranchable ici, donc WARN — et un WARN ne fait pas un PASS.
            step_warn "section attendue=decisions, obtenue=$SECTION (dérive curator à vérifier)"
        else
            step_fail "vault_read n'expose aucune section : $READ_RESP"
        fi
    fi
fi

# ── Étape 5 : audit JSONL dernière ligne ─────────────────────────────────────
echo "5/7 audit JSONL last line check"
TODAY=$(date -u +%Y-%m-%d)
AUDIT_FILE="/var/log/gradatum/audit.${TODAY}.jsonl"
if [[ ! -f "$AUDIT_FILE" ]]; then
    step_skip "fichier audit absent ($AUDIT_FILE) — étape non exécutée, pas un PASS"
else
    if [[ -r "$AUDIT_FILE" ]]; then
        AUDIT_LINE=$(tail -1 "$AUDIT_FILE")
    elif sudo -n true 2>/dev/null; then
        AUDIT_LINE=$(sudo -n tail -1 "$AUDIT_FILE")
    else
        AUDIT_LINE=""
    fi
    if [[ -z "$AUDIT_LINE" ]]; then
        step_skip "audit illisible ou vide ($AUDIT_FILE) — étape non exécutée"
    elif printf '%s' "$AUDIT_LINE" | jq -e '.event == "vault_write" and .outcome == "admitted"' >/dev/null 2>&1; then
        step_pass "event=vault_write outcome=admitted"
    else
        # outcome peut être "queued" si l'audit est émis avant le traitement worker (T7 pre-queue)
        OUTCOME=$(printf '%s' "$AUDIT_LINE" | jq -r '.outcome // "?"')
        EVENT=$(printf '%s' "$AUDIT_LINE" | jq -r '.event // "?"')
        step_warn "audit last line event=$EVENT outcome=$OUTCOME (attendu vault_write/admitted — voir comportement T7)"
    fi
fi

# ── Étape 6 : cleanup vault_downgrade ────────────────────────────────────────
echo "6/7 cleanup vault_downgrade note_id=${NOTE_ID:-<absent>}"
if [[ -z "$NOTE_ID" ]]; then
    step_skip "étape 6 non exécutée — note_id indisponible (rien à nettoyer)"
else
    DOWNGRADE_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
        -X POST "$SERVER/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $BEARER" \
        -H "Content-Type: application/json" \
        --max-time 10 \
        -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"smoke test cleanup alpha.4\",\"tenant_id\":\"main\"}") \
        || DOWNGRADE_CODE="000"
    if [[ "$DOWNGRADE_CODE" =~ ^(200|202|204)$ ]]; then
        step_pass "note downgradée (HTTP $DOWNGRADE_CODE)"
    else
        step_fail "vault_downgrade → HTTP $DOWNGRADE_CODE (note $NOTE_ID laissée en place)"
    fi
fi

# ── Étape 7 : RAM worker post-fastembed cold start (Gate1-réserve1) ──────────
echo "7/7 worker memory pic post-fastembed cold start (Gate1-réserve1)"
if ! systemctl is-active --quiet gradatum-worker 2>/dev/null; then
    step_skip "gradatum-worker non actif — étape non exécutée (deploy T13 requis)"
else
    MEM=$(systemctl show gradatum-worker -p MemoryCurrent --value 2>/dev/null || echo "N/A")
    if [[ "$MEM" =~ ^[0-9]+$ && "$MEM" != "18446744073709551615" ]]; then
        MEM_MB=$(( MEM / 1024 / 1024 ))
        echo "    MemoryCurrent = ${MEM_MB} MB (raw: $MEM)"
        # Seuil : bge-small-en-v1.5 ~153 MB RAM observé en bench T11 P2.0b.
        if (( MEM_MB > 600 )); then
            step_warn "RAM worker ${MEM_MB} MB > 600 MB — vérifier la config fastembed (Gate1-réserve1)"
        else
            step_pass "RAM worker ${MEM_MB} MB ≤ 600 MB"
        fi
    else
        step_skip "MemoryCurrent illisible ($MEM) — étape non exécutée (cgroup indisponible)"
    fi
fi

# ── Verdict ───────────────────────────────────────────────────────────────────
ACCOUNTED=$(( STEPS_PASS + STEPS_WARN + STEPS_FAIL + STEPS_SKIP ))
echo ""
echo "smoke-alpha-4 : PASS=$STEPS_PASS WARN=$STEPS_WARN FAIL=$STEPS_FAIL SKIP=$STEPS_SKIP (/$ACCOUNTED comptabilisées sur $STEPS_TOTAL)"
if [[ "$STEPS_FAIL" -gt 0 ]]; then
    echo "smoke-alpha-4 FAIL — $STEPS_FAIL étape(s) en échec" >&2
    exit 1
fi
if [[ "$ACCOUNTED" -ne "$STEPS_TOTAL" ]]; then
    echo "smoke-alpha-4 INCOMPLETE — $ACCOUNTED étapes comptabilisées sur $STEPS_TOTAL attendues" >&2
    exit 2
fi
if [[ "$STEPS_WARN" -gt 0 || "$STEPS_SKIP" -gt 0 ]]; then
    echo "smoke-alpha-4 INCOMPLETE — $STEPS_WARN WARN + $STEPS_SKIP non exécutée(s) : rien n'autorise un PASS" >&2
    exit 2
fi
echo "smoke-alpha-4 PASS — $STEPS_PASS/$STEPS_TOTAL étapes vérifiées"
exit 0
