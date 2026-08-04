#!/usr/bin/env bash
# Smoke acceptance test alpha.5 — 9 étapes + bonus RAM.
# Flow : health → api-key create → auth/exchange → vault_write → poll → vault_read
#        → audit JSONL → vault_downgrade → api-key revoke + 401
#
# Usage :
#   bash scripts/smoke-alpha-5.sh
#   SERVER=http://localhost:19090 ROOT=/var/lib/gradatum ADMIN_BIN=/usr/bin/gradatum-admin \
#     bash scripts/smoke-alpha-5.sh
#
# Mode dev local :
#   mkdir -p /tmp/dev-gradatum
#   cargo run --bin gradatum-admin -- init --root /tmp/dev-gradatum --preset hierarchical --non-interactive
#   # Éditer /tmp/dev-gradatum/config/server.toml : bind = "127.0.0.1:19092" si port 19090 occupé
#   cargo run --bin gradatum-server -- --config /tmp/dev-gradatum/config/server.toml &
#   SERVER=http://localhost:19092 ROOT=/tmp/dev-gradatum bash scripts/smoke-alpha-5.sh
#
# Pré-requis : jq curl (sudo optionnel pour audit JSONL systemd)
# Variables configurables :
#   SERVER     URL de base du serveur gradatum (défaut : http://localhost:19090)
#   ROOT       Répertoire racine Gradatum (défaut : /var/lib/gradatum)
#   ADMIN_BIN  Chemin vers gradatum-admin (défaut : /usr/bin/gradatum-admin)
#
# Note ACL (comportement connu alpha.5) :
#   L'ACL engine du serveur est initialisé avec le preset depuis config au démarrage.
#   Sur un déploiement systemd (Phase B), preset_path = /var/lib/gradatum/config/bearer.toml
#   inclut l'identité "main-agent". En dev, si le serveur ne charge pas le preset depuis
#   la config, vault_write/read/downgrade retournent 403 — comportement WARN-tolérant T9.
#   Le wiring AclEngine ← preset_path est documenté comme écart de wiring dans le rapport T9.
#
# VERDICT (corrigé lot I-003, 2026-07-30) — règle non négociable :
#   Un WARN ne contribue JAMAIS à un PASS. Un run comportant une étape SKIPPÉE est
#   INCOMPLETE, jamais PASS. Le verdict historique ne lisait que STEPS_FAIL : un 403
#   sur vault_write (étape 4) propageait JOB_ID="" et faisait « skipper » les étapes
#   5, 6 et 8 en WARN — bilan PASS=4 WARN=5 FAIL=0 → exit 0, alors que CINQ étapes
#   sur neuf n'avaient rien validé. Mesuré avant correction.
#   Exit 0 = PASS (9/9 vérifiées) · 1 = FAIL · 2 = INCOMPLETE / gate non exécuté.
#
# `set -e` volontairement ABSENT : le script doit dérouler ses 9 étapes et rendre un
#   bilan, pas s'arrêter à la première réponse HTTP non nominale (un 403 attendu est
#   une donnée du test, pas une panne du script). Le trou que `-e` couvrirait — une
#   mort silencieuse en milieu de course lue comme un succès — est fermé autrement :
#   le verdict compare le nombre d'étapes COMPTABILISÉES au nombre d'étapes ATTENDUES
#   (STEPS_TOTAL). Toute étape qui n'a produit ni PASS, ni WARN, ni FAIL, ni SKIP rend
#   le run INCOMPLETE. Un `-e` ajouté ici rendrait au contraire inatteignables les
#   gestionnaires d'erreur qui suivent chaque appel curl — c'est exactement le piège
#   qui a tué smoke-alpha-4 (I-002).
#
# Validated against design spec P2.0c-bis AUTH-T9 — 2026-05-07.
set -uo pipefail

# ── Configuration ──────────────────────────────────────────────────────────────
SERVER="${SERVER:-http://localhost:19090}"
ROOT="${ROOT:-/var/lib/gradatum}"
ADMIN_BIN="${ADMIN_BIN:-/usr/bin/gradatum-admin}"

# ── Couleurs ANSI (TTY-aware) ──────────────────────────────────────────────────
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

# ── Compteurs pour le résumé final ────────────────────────────────────────────
STEPS_TOTAL=9          # étapes annoncées par l'en-tête : le verdict s'y compare
STEPS_PASS=0
STEPS_WARN=0
STEPS_FAIL=0
STEPS_SKIP=0

# ── Helpers ────────────────────────────────────────────────────────────────────

step_pass() {
    local step="$1"
    local detail="$2"
    echo -e "    ${GREEN}OK${RESET} — $detail"
    STEPS_PASS=$(( STEPS_PASS + 1 ))
}

step_warn() {
    local detail="$1"
    echo -e "    ${YELLOW}WARN${RESET} — $detail"
    STEPS_WARN=$(( STEPS_WARN + 1 ))
}

step_fail() {
    local detail="$1"
    echo -e "    ${RED}FAIL${RESET} — $detail"
    STEPS_FAIL=$(( STEPS_FAIL + 1 ))
}

# Étape NON EXÉCUTÉE (dépendance amont absente, pré-requis manquant). Distincte du
# WARN : elle ne dit rien du système sous test. Ni l'une ni l'autre n'autorise un PASS.
step_skip() {
    local detail="$1"
    echo -e "    ${YELLOW}SKIP${RESET} — $detail"
    STEPS_SKIP=$(( STEPS_SKIP + 1 ))
}

# ── Variables globales collectées au fil des étapes ────────────────────────────
AK_SECRET=""          # secret API key (une seule fois)
AK_PREFIX=""          # préfixe ak_xxxxxxxx (8 chars)
JWT=""                # JWT issu de /auth/exchange
TTL_SECS=""           # expires_in
JOB_ID=""             # job_id retourné par vault_write
NOTE_ID=""            # note_id récupéré depuis le job (si worker câblé)

# ── Cleanup si erreur fatale ────────────────────────────────────────────────────
# Tente de révoquer la clé créée si le script sort prématurément.
_cleanup_on_exit() {
    local exit_code="$?"
    if [[ -n "$AK_PREFIX" && "$exit_code" -ne 0 ]]; then
        echo ""
        echo "--- cleanup on exit (exit=$exit_code) ---"
        if [[ -x "$ADMIN_BIN" ]]; then
            "$ADMIN_BIN" api-key revoke --root "$ROOT" --prefix "$AK_PREFIX" 2>/dev/null \
                && echo "    clé $AK_PREFIX révoquée (cleanup)" \
                || echo "    WARN: révocation cleanup échouée pour $AK_PREFIX"
        fi
    fi
}
trap '_cleanup_on_exit' EXIT

# ── Vérification pré-requis ────────────────────────────────────────────────────
for cmd in jq curl; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "ERREUR: pré-requis manquant : $cmd" >&2
        exit 1
    fi
done

if [[ ! -x "$ADMIN_BIN" ]]; then
    echo "ERREUR: gradatum-admin non trouvé ou non exécutable : $ADMIN_BIN" >&2
    echo "       Conseil : utiliser ADMIN_BIN=<chemin> pour pointer vers le binaire dev" >&2
    exit 1
fi

echo "=== smoke-alpha-5 : $SERVER ==="
echo "    ROOT=$ROOT  ADMIN_BIN=$ADMIN_BIN"
echo ""

# ── Étape 1 : health check ─────────────────────────────────────────────────────
echo "1/9 health check"
HEALTH_RESP=$(curl -fsS "$SERVER/health" 2>&1) || {
    step_fail "serveur inaccessible : $SERVER/health"
    echo ""
    echo "${RED}ABORT${RESET} — serveur non accessible, abandon du smoke"
    exit 1
}
HEALTH_STATUS=$(echo "$HEALTH_RESP" | jq -r '.status // empty' 2>/dev/null)
if [[ "$HEALTH_STATUS" == "ok" ]]; then
    HEALTH_VERSION=$(echo "$HEALTH_RESP" | jq -r '.version // "?"')
    step_pass "1" "status=ok version=$HEALTH_VERSION"
else
    step_fail "status inattendu : $HEALTH_RESP"
fi

# ── Étape 2 : api-key create ───────────────────────────────────────────────────
echo "2/9 gradatum-admin api-key create --owner smoke --scopes admin --tenant main"
# Le secret est affiché sur stdout, les logs sur stderr.
# Note : l'owner "smoke" crée un JWT sub="smoke". Si le preset ACL ne définit pas
# un consumer "smoke", vault_write/read retourneront 403 (comportement connu alpha.5).
# En déploiement systemd (Phase B), ajuster si nécessaire avec --owner main-agent (preset hierarchical).
AK_SECRET=$("$ADMIN_BIN" api-key create \
    --root "$ROOT" \
    --owner smoke \
    --scopes admin \
    --tenant main \
    --description "Smoke E2E acceptance test alpha.5" 2>/dev/null)
if [[ -z "$AK_SECRET" ]]; then
    step_fail "gradatum-admin api-key create a échoué ou retourné un secret vide"
    # Tentative de récupération via stderr pour diagnostic
    "$ADMIN_BIN" api-key create \
        --root "$ROOT" \
        --owner smoke \
        --scopes admin \
        --tenant main 2>&1 | head -5 >&2 || true
else
    # Extraire le préfixe (ak_ + 8 chars)
    if [[ "${#AK_SECRET}" -ge 11 ]]; then
        AK_PREFIX="${AK_SECRET:0:11}"
    else
        AK_PREFIX="$AK_SECRET"
    fi
    step_pass "2" "clé créée — préfixe=$AK_PREFIX (secret affiché UNE SEULE FOIS)"
fi

if [[ -z "$AK_SECRET" ]]; then
    echo ""
    echo "${RED}ABORT${RESET} — impossible de continuer sans API key"
    exit 1
fi

# ── Étape 3 : POST /auth/exchange → JWT ───────────────────────────────────────
echo "3/9 POST /auth/exchange Bearer $AK_PREFIX → JWT capture"
EXCHANGE_RESP=$(curl -fsS -X POST "$SERVER/auth/exchange" \
    -H "Authorization: Bearer $AK_SECRET" \
    -H "Content-Type: application/json" \
    -d '{}' 2>&1) || {
    step_fail "POST /auth/exchange a échoué (curl exit non-zero) : $EXCHANGE_RESP"
    echo ""
    echo "${RED}ABORT${RESET} — sans JWT, les étapes suivantes sont impossibles"
    exit 1
}

JWT=$(echo "$EXCHANGE_RESP" | jq -r '.token // empty')
TTL_SECS=$(echo "$EXCHANGE_RESP" | jq -r '.ttl_secs // empty')
SCOPES_JSON=$(echo "$EXCHANGE_RESP" | jq -c '.scopes // []')
TENANT_ID=$(echo "$EXCHANGE_RESP" | jq -r '.tenant_id // "?"')

if [[ -z "$JWT" || "$JWT" == "null" ]]; then
    step_fail "JWT absent dans la réponse : $EXCHANGE_RESP"
    echo ""
    echo "${RED}ABORT${RESET} — JWT invalide, étapes 4-9 impossibles"
    exit 1
fi

# Valider le format JWT (3 segments base64url séparés par '.')
JWT_PARTS=$(echo "$JWT" | tr '.' '\n' | wc -l)
if [[ "$JWT_PARTS" -ne 3 ]]; then
    step_fail "JWT malformé (attendu 3 segments, obtenu $JWT_PARTS)"
else
    step_pass "3" "JWT capturé — ttl=${TTL_SECS}s scopes=$SCOPES_JSON tenant=$TENANT_ID"
fi

# ── Étape 4 : vault_write avec JWT → job_id ────────────────────────────────────
# Fichiers de réponse en mktemp, jamais un chemin fixe /tmp/smoke_*.json : sur un hôte
# multi-comptes il est pré-positionnable, et si curl n'écrit rien le `cat` relisait la
# réponse du RUN PRÉCÉDENT — une contamination inter-exécution invisible.
RESP_DIR=$(mktemp -d -t smoke-alpha-5-XXXXXX)
trap 'rm -rf "$RESP_DIR"; _cleanup_on_exit' EXIT

echo "4/9 vault_write avec JWT → job_id"
WRITE_HTTP_CODE=$(curl -sS -o "$RESP_DIR/write.json" -w "%{http_code}" \
    -X POST "$SERVER/api/v1/vault_write" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    -d '{"title":"[DECISIONS] alpha.5 smoke test","body":"smoke body — test automatique smoke-alpha-5 Path 2 auth","tenant_id":"main"}' 2>/dev/null)
WRITE_BODY=$(cat "$RESP_DIR/write.json" 2>/dev/null || echo "")

if [[ "$WRITE_HTTP_CODE" == "202" ]]; then
    JOB_ID=$(echo "$WRITE_BODY" | jq -r '.job_id // empty')
    POLL_URL=$(echo "$WRITE_BODY" | jq -r '.poll_url // ""')
    if [[ -n "$JOB_ID" ]]; then
        step_pass "4" "job_id=$JOB_ID poll_url=$POLL_URL"
    else
        step_fail "HTTP 202 mais job_id absent dans la réponse : $WRITE_BODY"
    fi
elif [[ "$WRITE_HTTP_CODE" == "403" ]]; then
    # Cause réelle et connue : l'ACL engine ne charge pas le preset (écart de wiring
    # AclEngine ← preset_path, rapport T9). Ce n'est PAS un motif de tolérance : le flux
    # annoncé par l'en-tête n'a pas été validé. On le compte comme étape NON EXÉCUTÉE et
    # le verdict final tombera en INCOMPLETE. Desserrer la condition pour verdir serait
    # précisément le défaut que ce script est censé détecter.
    step_skip "vault_write → 403 Forbidden — flux non validé (wiring ACL ← preset_path non câblé, écart T9)"
    JOB_ID=""
elif [[ "$WRITE_HTTP_CODE" == "401" ]]; then
    # Une authentification cassée n'est pas tolérable : le JWT vient d'être émis par
    # /auth/exchange à l'étape 3, un 401 ici est une contradiction du serveur.
    step_fail "vault_write → 401 Unauthorized alors que le JWT vient d'être émis (étape 3)"
    JOB_ID=""
else
    step_fail "vault_write → HTTP $WRITE_HTTP_CODE inattendu : $WRITE_BODY"
    JOB_ID=""
fi

# ── Étape 5 : poll job → done (retry max 30s) ─────────────────────────────────
echo "5/9 poll job status (max 30s)"
if [[ -z "$JOB_ID" ]]; then
    step_skip "étape 5 non exécutée — pas de job_id (dépend de l'étape 4)"
else
    JOB_STATUS="pending"
    POLL_DONE=0
    for i in $(seq 1 30); do
        JOB_RESP=$(curl -fsS "$SERVER/api/v1/jobs/$JOB_ID" \
            -H "Authorization: Bearer $JWT" 2>/dev/null || echo '{"status":"error"}')
        JOB_STATUS=$(echo "$JOB_RESP" | jq -r '.status // "error"')
        if [[ "$JOB_STATUS" == "done" ]]; then
            step_pass "5" "status=done après ${i}s"
            POLL_DONE=1
            # Récupérer note_id si présent dans le résultat du job
            NOTE_ID=$(echo "$JOB_RESP" | jq -r '.result.note_id // empty' 2>/dev/null || echo "")
            break
        fi
        sleep 1
    done
    if [[ "$POLL_DONE" -eq 0 ]]; then
        # En alpha.5, le worker stub retourne "pending" indéfiniment.
        # C'est un comportement stub connu (jobs.rs retourne toujours "pending" en T3 stub).
        if [[ "$JOB_STATUS" == "pending" ]]; then
            step_skip "poll timeout 30s — job reste 'pending', transition non observée (worker stub T3)"
        else
            step_fail "poll timeout 30s — statut inattendu : $JOB_STATUS"
        fi
    fi
fi

# ── Étape 6 : vault_read → confirm note ───────────────────────────────────────
echo "6/9 vault_read → confirm note"
if [[ -z "$NOTE_ID" ]]; then
    # En alpha.5 avec worker stub, note_id n'est jamais disponible via poll.
    step_skip "étape 6 non exécutée — note_id indisponible (dépend des étapes 4-5)"
else
    READ_HTTP_CODE=$(curl -sS -o "$RESP_DIR/read.json" -w "%{http_code}" \
        -X POST "$SERVER/api/v1/vault_read" \
        -H "Authorization: Bearer $JWT" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$NOTE_ID\",\"tenant_id\":\"main\"}" 2>/dev/null)
    READ_BODY=$(cat "$RESP_DIR/read.json" 2>/dev/null || echo "")

    if [[ "$READ_HTTP_CODE" == "200" ]]; then
        NOTE_PATH=$(echo "$READ_BODY" | jq -r '.path // "?"')
        NOTE_SIZE=$(echo "$READ_BODY" | jq -r '.size_bytes // "?"')
        step_pass "6" "note lue — path=$NOTE_PATH size=${NOTE_SIZE}B"
    elif [[ "$READ_HTTP_CODE" == "404" ]]; then
        # La note vient d'être écrite à l'étape 4 : ne pas la retrouver n'est pas
        # « attendu », c'est un défaut de relecture. Non tolérable.
        step_fail "vault_read → 404 NoteNotFound sur la note écrite à l'étape 4 (note_id=$NOTE_ID)"
    elif [[ "$READ_HTTP_CODE" == "403" ]]; then
        step_skip "vault_read → 403 Forbidden — relecture non validée (même cause ACL que l'étape 4)"
    else
        step_fail "vault_read → HTTP $READ_HTTP_CODE inattendu : $READ_BODY"
    fi
fi

# ── Étape 7 : audit JSONL last line (warn-tolerant) ────────────────────────────
echo "7/9 audit JSONL last line check (warn-tolerant — JsonlFileSink stub Phase 2.1)"
TODAY=$(date -u +%Y-%m-%d)
AUDIT_FILE="$ROOT/audit/audit.${TODAY}.jsonl"
# Chemins alternatifs selon le layout de déploiement
AUDIT_FILE_ALT_1="/var/log/gradatum/audit.${TODAY}.jsonl"
AUDIT_FILE_ALT_2="$ROOT/log/audit.${TODAY}.jsonl"

FOUND_AUDIT=""
for af in "$AUDIT_FILE" "$AUDIT_FILE_ALT_1" "$AUDIT_FILE_ALT_2"; do
    if [[ -f "$af" ]]; then
        FOUND_AUDIT="$af"
        break
    fi
done

if [[ -z "$FOUND_AUDIT" ]]; then
    # JsonlFileSink non câblé bout-en-bout (D6 spec V2) : aucune ligne à lire, donc
    # RIEN n'a été vérifié. Étape non exécutée — jamais un PASS.
    step_skip "fichier audit JSONL absent (cherché : $AUDIT_FILE, $AUDIT_FILE_ALT_1, $AUDIT_FILE_ALT_2)"
else
    # Lecture avec sudo si possible (fichier peut être owned by gradatum)
    if sudo -n test -r "$FOUND_AUDIT" 2>/dev/null; then
        AUDIT_LINE=$(sudo tail -1 "$FOUND_AUDIT" 2>/dev/null || echo "")
    else
        AUDIT_LINE=$(tail -1 "$FOUND_AUDIT" 2>/dev/null || echo "")
    fi

    if [[ -z "$AUDIT_LINE" ]]; then
        step_skip "fichier audit présent mais vide ou illisible ($FOUND_AUDIT) — aucune ligne lue"
    else
        # Vérification P1-4 spec V2 : events vault STRICT (vault_write/vault_downgrade).
        # Events auth WARN-tolerant (auth_exchange_success non câblé alpha.5).
        AUDIT_EVENT=$(echo "$AUDIT_LINE" | jq -r '.event // "?"' 2>/dev/null || echo "?")
        AUDIT_OUTCOME=$(echo "$AUDIT_LINE" | jq -r '.outcome // "?"' 2>/dev/null || echo "?")
        if echo "$AUDIT_LINE" | jq -e '.event and .outcome' >/dev/null 2>&1; then
            if [[ "$AUDIT_EVENT" =~ ^(vault_write|vault_downgrade|worker_curate)$ ]]; then
                step_pass "7" "audit last event=$AUDIT_EVENT outcome=$AUDIT_OUTCOME"
            else
                step_warn "audit last event=$AUDIT_EVENT outcome=$AUDIT_OUTCOME (attendu vault_write ou worker_curate)"
            fi
        else
            step_warn "audit last line malformée ou incomplète : $AUDIT_LINE"
        fi
    fi
fi

# ── Étape 8 : cleanup vault_downgrade ─────────────────────────────────────────
echo "8/9 cleanup vault_downgrade"
if [[ -z "$NOTE_ID" ]]; then
    step_skip "étape 8 non exécutée — note_id indisponible (dépend des étapes 4-6)"
else
    DOWNGRADE_HTTP_CODE=$(curl -sS -o /dev/null -w "%{http_code}" \
        -X POST "$SERVER/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $JWT" \
        -H "Content-Type: application/json" \
        -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"smoke test cleanup alpha.5\",\"tenant_id\":\"main\"}" 2>/dev/null)
    if [[ "$DOWNGRADE_HTTP_CODE" == "202" ]]; then
        step_pass "8" "note downgradée (job enqueued)"
    elif [[ "$DOWNGRADE_HTTP_CODE" == "403" ]]; then
        step_skip "vault_downgrade → 403 — nettoyage non validé (même cause ACL que les étapes 4+6)"
    elif [[ "$DOWNGRADE_HTTP_CODE" == "404" ]]; then
        step_skip "vault_downgrade → 404 (note absente : rien n'a pu être nettoyé)"
    else
        step_fail "vault_downgrade → HTTP $DOWNGRADE_HTTP_CODE inattendu"
    fi
fi

# ── Étape 9 : revoke api-key + retry exchange → 401 attendu ───────────────────
echo "9/9 gradatum-admin api-key revoke $AK_PREFIX + retry exchange → 401"
REVOKE_OK=0
REVOKE_OUT=$("$ADMIN_BIN" api-key revoke \
    --root "$ROOT" \
    --prefix "$AK_PREFIX" 2>&1)
REVOKE_EXIT=$?

if [[ "$REVOKE_EXIT" -eq 0 ]]; then
    echo "    clé $AK_PREFIX révoquée"
    REVOKE_OK=1
    # Supprimer le trap (clé déjà révoquée)
    trap - EXIT
    AK_PREFIX=""
else
    step_fail "gradatum-admin api-key revoke a échoué (exit=$REVOKE_EXIT) : $REVOKE_OUT"
fi

if [[ "$REVOKE_OK" -eq 1 ]]; then
    # Retry exchange avec la clé révoquée → doit obtenir 401
    RETRY_HTTP_CODE=$(curl -sS -o /dev/null -w "%{http_code}" \
        -X POST "$SERVER/auth/exchange" \
        -H "Authorization: Bearer $AK_SECRET" \
        -H "Content-Type: application/json" \
        -d '{}' 2>/dev/null)

    if [[ "$RETRY_HTTP_CODE" == "401" ]]; then
        step_pass "9" "clé révoquée + retry exchange → 401 Unauthorized (comportement correct)"
    elif [[ "$RETRY_HTTP_CODE" == "400" ]]; then
        # 400 peut arriver si le serveur recharge les clés depuis la DB et refuse avant 401
        step_warn "retry exchange → 400 (attendu 401 — vérifier comportement révocation)"
    else
        step_fail "retry exchange → HTTP $RETRY_HTTP_CODE (attendu 401 après révocation)"
    fi
fi

# ── Bonus : RAM worker check (Gate1-réserve1) ──────────────────────────────────
echo "RAM/bonus worker memory check (Gate1-réserve1)"
if systemctl is-active --quiet gradatum-worker 2>/dev/null; then
    MEM=$(sudo systemctl show gradatum-worker -p MemoryCurrent --value 2>/dev/null || echo "N/A")
    if [[ "$MEM" != "N/A" && "$MEM" != "18446744073709551615" ]]; then
        MEM_MB=$(( MEM / 1024 / 1024 ))
        echo "    MemoryCurrent = ${MEM_MB} MB"
        if (( MEM_MB > 600 )); then
            # BONUS hors des 9 étapes : il ne touche AUCUN compteur d'étape, sans quoi
            # le bilan cesserait d'être comparable à STEPS_TOTAL.
            echo "    WARN: RAM worker > 600 MB — vérifier la config fastembed (Gate1-réserve1)"
        else
            echo "    OK — RAM within threshold (< 600 MB)"
        fi
    else
        echo "    INFO: MemoryCurrent = N/A (cgroup non disponible ou worker en cours de démarrage)"
    fi
else
    echo "    INFO: gradatum-worker non actif — bonus RAM skippé (deploy systemd Phase B requis)"
fi

# ── Résumé final ───────────────────────────────────────────────────────────────
echo ""
echo "================================================="
ACCOUNTED=$(( STEPS_PASS + STEPS_WARN + STEPS_FAIL + STEPS_SKIP ))
echo "RÉSULTAT smoke-alpha-5 : PASS=$STEPS_PASS  WARN=$STEPS_WARN  FAIL=$STEPS_FAIL  SKIP=$STEPS_SKIP" \
     " (/$ACCOUNTED comptabilisées sur $STEPS_TOTAL attendues)"
if [[ "$STEPS_FAIL" -gt 0 ]]; then
    echo -e "${RED}smoke-alpha-5 FAIL${RESET} — $STEPS_FAIL étape(s) en échec dur"
    exit 1
fi
if [[ "$ACCOUNTED" -ne "$STEPS_TOTAL" ]]; then
    # Ni PASS, ni WARN, ni FAIL, ni SKIP sur une étape annoncée : le script s'est
    # interrompu en route. Sans `set -e`, c'est le seul témoin d'une mort silencieuse.
    echo -e "${RED}smoke-alpha-5 INCOMPLETE${RESET} — $ACCOUNTED étapes comptabilisées sur $STEPS_TOTAL attendues"
    exit 2
fi
if [[ "$STEPS_WARN" -gt 0 || "$STEPS_SKIP" -gt 0 ]]; then
    echo -e "${YELLOW}smoke-alpha-5 INCOMPLETE${RESET} — $STEPS_WARN WARN + $STEPS_SKIP étape(s) non exécutée(s) :"
    echo "    aucune de ces étapes n'a validé quoi que ce soit ; un WARN ne fait pas un PASS."
    exit 2
fi
echo -e "${GREEN}smoke-alpha-5 PASS${RESET} — $STEPS_PASS/$STEPS_TOTAL étapes vérifiées"
exit 0
