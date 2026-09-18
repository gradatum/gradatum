#!/usr/bin/env bash
# Post-deploy smoke test 2 — 9 steps + RAM bonus (credential lifecycle acceptance).
# Flow : health → api-key create → auth/exchange → vault_write → poll → vault_read
#        → audit JSONL → vault_downgrade → api-key revoke + 401
#
# Usage :
#   bash scripts/smoke-post-deploy-2.sh
#   SERVER=http://localhost:19090 ROOT=/var/lib/gradatum ADMIN_BIN=/usr/bin/gradatum-admin \
#     bash scripts/smoke-post-deploy-2.sh
#
# Local dev mode :
#   mkdir -p /tmp/dev-gradatum
#   cargo run --bin gradatum-admin -- init --root /tmp/dev-gradatum --preset hierarchical --non-interactive
#   # Éditer /tmp/dev-gradatum/config/server.toml : bind = "127.0.0.1:19092" si port 19090 occupé
#   cargo run --bin gradatum-server -- --config /tmp/dev-gradatum/config/server.toml &
#   SERVER=http://localhost:19092 ROOT=/tmp/dev-gradatum bash scripts/smoke-post-deploy-2.sh
#
# Requirements : jq + curl (sudo optional — audit JSONL under systemd)
# Configurable variables :
#   SERVER         gradatum server base URL (default: http://localhost:19090)
#   ROOT           gradatum root directory (default: /var/lib/gradatum)
#   ADMIN_BIN      path to gradatum-admin (default: /usr/bin/gradatum-admin)
#   SMOKE_IDENTITY ACL identity this script mints its own key for (default: smoke-dyn)
#   API_KEY_FILE   static post-deploy probe credential file — the one this script must NOT
#                  be the holder of, named in the diagnostics below
#                  (default: /etc/gradatum/probe-smoke-post-deploy.api-key)
#
# WRITE IDENTITY AND SECTION — this script mints its own key for the identity `smoke-dyn`,
#   which the ACL preset declares with the same bounded locus as `smoke` (write/read on the
#   tenant locus, read on the jobs locus). Key creation validates `--owner` against the
#   preset, so `--allow-unknown-identity` is neither needed nor used: an undeclared
#   identity is refused, and that refusal is the signal the wiring is incomplete.
#
#   Two identities, not one. The preset carries one consumer per identity, and one identity
#   carries exactly ONE ACTIVE KEY: the static post-deploy probe installed at API_KEY_FILE —
#   the credential `smoke-post-deploy-1.sh` authenticates with — is minted for `smoke`. Were
#   this script to claim `smoke` too, its step 2 would be refused while that probe is active,
#   which is the nominal deployed state, so the create → exchange → revoke cycle it exists to
#   exercise would be unreachable by construction. It mints under the second declared
#   identity, `smoke-dyn`, and leaves the probe untouched. Step 3 reads the JWT `sub` claim
#   back and compares it to that identity: the token in hand is provably the one asked for.
#
#   A `second active key` refusal at step 2 is consequently NOT the probe's doing. It means a
#   `smoke-dyn` key survived an earlier interrupted run, whose cleanup trap could not revoke
#   it. The script reports that as a non-executed step, never as a deployment fault — and
#   never as a pass.
#
#   The note carries the `[SMOKE]` title marker instead of a production category marker,
#   plus a `reference` section hint. `[SMOKE]` is not a canonical category, so the router
#   would otherwise fall through to the keyword heuristic; `reference` is the least
#   sensitive canonical section (the heuristic's own ambiguity fallback), and a valid
#   section hint is admitted directly by the curator — neither the heuristic nor the LLM
#   runs. The note therefore lands in `reference` deterministically, never in a section
#   that carries production knowledge.
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
# Identité porteuse de la clé forgée par ce smoke — source unique : `--owner` (étape 2) et
# le contrôle du claim `sub` du JWT (étape 3) lisent tous deux cette variable, sans quoi les
# deux pourraient diverger en silence. Doit être déclarée par le preset ACL.
SMOKE_IDENTITY="${SMOKE_IDENTITY:-smoke-dyn}"
API_KEY_FILE="${API_KEY_FILE:-/etc/gradatum/probe-smoke-post-deploy.api-key}"

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

echo "=== smoke-post-deploy-2 : $SERVER ==="
echo "    ROOT=$ROOT  ADMIN_BIN=$ADMIN_BIN  API_KEY_FILE=$API_KEY_FILE"
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
echo "2/9 gradatum-admin api-key create --owner $SMOKE_IDENTITY --scopes admin --tenant main"
# Le secret sort sur stdout, les logs sur stderr : deux flux capturés séparément, sans
# quoi le diagnostic ci-dessous n'aurait rien à lire. Pas de seconde tentative de
# création : `api-key create` a un effet de bord, un retry aveugle en créerait un second.
AK_ERR_FILE=$(mktemp -t smoke-post-deploy-2-ak-XXXXXX)
AK_SECRET=$("$ADMIN_BIN" api-key create \
    --root "$ROOT" \
    --owner "$SMOKE_IDENTITY" \
    --scopes admin \
    --tenant main \
    --description "Post-deploy smoke test 2" 2>"$AK_ERR_FILE")
AK_ERR=$(head -c 2000 "$AK_ERR_FILE" 2>/dev/null)
rm -f "$AK_ERR_FILE"
if [[ -z "$AK_SECRET" ]]; then
    if grep -qi "second active key" <<<"$AK_ERR"; then
        # R1 — une identité ne porte qu'une clé active. L'identité de ce smoke est `smoke-dyn`,
        # réservée à lui : la sonde statique installée à $API_KEY_FILE porte l'identité
        # `smoke`, distincte, et n'est donc PAS la cause d'un refus ici. Le détenteur est une
        # clé `smoke-dyn` restée active — résidu d'un run antérieur interrompu avant que le
        # trap de cleanup ne la révoque (ou clé posée par un opérateur). Pré-requis de banc,
        # pas une panne du déploiement : étape NON EXÉCUTÉE → verdict INCOMPLETE (exit 2).
        step_skip "api-key create refusé — l'identité '$SMOKE_IDENTITY' porte déjà une clé active (règle R1). Clé résiduelle d'un run antérieur : la révoquer (gradatum-admin api-key revoke --root $ROOT --prefix ak_…), puis relancer. Pré-requis de banc manquant, pas une panne."
        echo ""
        ACCOUNTED=$(( STEPS_PASS + STEPS_WARN + STEPS_FAIL + STEPS_SKIP ))
        echo "RÉSULTAT smoke-post-deploy-2 : PASS=$STEPS_PASS  WARN=$STEPS_WARN  FAIL=$STEPS_FAIL  SKIP=$STEPS_SKIP (/$ACCOUNTED comptabilisées sur $STEPS_TOTAL attendues)"
        echo -e "${RED}smoke-post-deploy-2 INCOMPLETE${RESET} — étape 2 non exécutée, les étapes 3-9 en dépendent"
        exit 2
    fi
    step_fail "gradatum-admin api-key create a échoué ou retourné un secret vide : $(tr '\n' ' ' <<<"$AK_ERR" | head -c 300)"
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
# Contrôle d'identité : le claim `sub` porte l'identité propriétaire de la clé
# (`JwtService::sign(sub = key.owner)`), donc le token est celui de $SMOKE_IDENTITY — ou il
# ne l'est pas. Décodage du payload (segment 2) : base64url → base64 standard, translittéré
# puis paddé à un multiple de 4, sans quoi `base64 -d` refuse un payload non paddé.
JWT_PAYLOAD=$(printf '%s' "$JWT" | cut -d. -f2 | tr '_-' '/+')
case $(( ${#JWT_PAYLOAD} % 4 )) in
    2) JWT_PAYLOAD="${JWT_PAYLOAD}==" ;;
    3) JWT_PAYLOAD="${JWT_PAYLOAD}=" ;;
esac
JWT_SUB=$(printf '%s' "$JWT_PAYLOAD" | base64 -d 2>/dev/null | jq -r '.sub // empty' 2>/dev/null)

if [[ "$JWT_PARTS" -ne 3 ]]; then
    step_fail "JWT malformé (attendu 3 segments, obtenu $JWT_PARTS)"
elif [[ "$JWT_SUB" != "$SMOKE_IDENTITY" ]]; then
    # Identité non prouvée : payload illisible, `sub` absent, ou autre identité que celle
    # demandée à l'étape 2. Le flux n'a alors pas validé ce que ce script annonce.
    step_fail "JWT sub='${JWT_SUB:-<illisible>}' ≠ '$SMOKE_IDENTITY' — la clé en main n'est pas celle de l'identité attendue"
else
    step_pass "3" "JWT capturé — sub=$JWT_SUB ttl=${TTL_SECS}s scopes=$SCOPES_JSON tenant=$TENANT_ID"
fi

# ── Étape 4 : vault_write avec JWT → job_id ────────────────────────────────────
# Fichiers de réponse en mktemp, jamais un chemin fixe /tmp/smoke_*.json : sur un hôte
# multi-comptes il est pré-positionnable, et si curl n'écrit rien le `cat` relisait la
# réponse du RUN PRÉCÉDENT — une contamination inter-exécution invisible.
RESP_DIR=$(mktemp -d -t smoke-post-deploy-2-XXXXXX)
trap 'rm -rf "$RESP_DIR"; _cleanup_on_exit' EXIT

# `section_hint: reference` est un hint VALIDE : le curateur l'admet directement, sans
# heuristique ni LLM (chemin « hint fort »). La section cesse de dépendre du contenu ou
# d'un backend — elle est choisie, pas devinée. Locus ACL de l'écriture : `<tenant>/main`,
# indépendant de la section visée.
echo "4/9 vault_write avec JWT → job_id"
WRITE_HTTP_CODE=$(curl -sS -o "$RESP_DIR/write.json" -w "%{http_code}" \
    -X POST "$SERVER/api/v1/vault_write" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    -d '{"title":"[SMOKE] post-deploy smoke 2","body":"Synthetic note written by the post-deploy smoke test. No content.","section_hint":"reference","tenant_id":"main"}' 2>/dev/null)
WRITE_BODY=$(cat "$RESP_DIR/write.json" 2>/dev/null || echo "")

if [[ "$WRITE_HTTP_CODE" == "202" ]]; then
    JOB_ID=$(echo "$WRITE_BODY" | jq -r '.job_id // empty')
    POLL_URL=$(echo "$WRITE_BODY" | jq -r '.poll_url // ""')
    # `note_id` est pré-alloué à l'enqueue : lisible immédiatement, sans attendre le job.
    NOTE_ID=$(echo "$WRITE_BODY" | jq -r '.note_id // empty')
    if [[ -n "$JOB_ID" ]]; then
        step_pass "4" "job_id=$JOB_ID note_id=$NOTE_ID poll_url=$POLL_URL"
    else
        step_fail "HTTP 202 mais job_id absent dans la réponse : $WRITE_BODY"
    fi
elif [[ "$WRITE_HTTP_CODE" == "403" ]]; then
    # 403 : l'identité déclarée n'est pas autorisée sur le locus évalué (`<tenant>/main`).
    # Ce n'est PAS un motif de tolérance : le flux annoncé par l'en-tête n'a pas été
    # validé. On le compte comme étape NON EXÉCUTÉE et le verdict final tombera en
    # INCOMPLETE. Desserrer la condition pour verdir serait précisément le défaut que ce
    # script est censé détecter.
    step_skip "vault_write → 403 Forbidden — flux non validé (grant de locus absent pour l'identité déclarée)"
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

# ── Étape 5 : poll job → done (max 30s ; échec terminal = arrêt immédiat)
echo "5/9 poll job status (max 30s)"
if [[ -z "$JOB_ID" ]]; then
    step_skip "étape 5 non exécutée — pas de job_id (dépend de l'étape 4)"
else
    JOB_STATUS="Pending"
    POLL_DONE=0
    POLL_ELAPSED=0
    for i in $(seq 1 30); do
        POLL_ELAPSED=$i
        # Endpoint v2 : /api/v1/jobs/{ulid}/v2. Le v1 (/api/v1/jobs/{id}) n'existe plus —
        # il attendait un i64 et rejetait un ULID. Statut sous .lifecycle.status, valeurs
        # capitalisées (Done = succès ; DLQ/Cancelled/Conflict = terminal en échec).
        JOB_RESP=$(curl -fsS --max-time 10 "$SERVER/api/v1/jobs/$JOB_ID/v2" \
            -H "Authorization: Bearer $JWT" 2>/dev/null || echo '{"lifecycle":{"status":"Error"}}')
        JOB_STATUS=$(echo "$JOB_RESP" | jq -r '.lifecycle.status // "Error"')
        if [[ "$JOB_STATUS" == "Done" ]]; then
            step_pass "5" "status=Done après ${i}s"
            POLL_DONE=1
            break
        fi
        # État terminal en échec : le job n'évoluera plus, attendre les 30 s restantes ne
        # produirait aucune information. Sortie immédiate — le verdict, lui, ne change pas.
        if [[ "$JOB_STATUS" == "DLQ" || "$JOB_STATUS" == "Cancelled" || "$JOB_STATUS" == "Conflict" ]]; then
            break
        fi
        sleep 1
    done
    if [[ "$POLL_DONE" -eq 0 ]]; then
        # Job non terminal après 30s : la transition n'a pas été observée, donc RIEN n'a
        # été validé. Non exécutée → INCOMPLETE, jamais un PASS.
        if [[ "$JOB_STATUS" == "Pending" || "$JOB_STATUS" == "Running" || "$JOB_STATUS" == "Waiting" ]]; then
            step_skip "poll timeout 30s — job encore '$JOB_STATUS', transition terminale non observée"
        else
            step_fail "poll interrompu après ${POLL_ELAPSED}s — statut '$JOB_STATUS' (attendu Done)"
        fi
    fi
fi

# ── Étape 6 : vault_read → confirm note ───────────────────────────────────────
echo "6/9 vault_read → confirm note"
if [[ -z "$NOTE_ID" ]]; then
    # note_id est pré-alloué et renvoyé par l'écriture (étape 4) : il manque seulement si
    # l'écriture elle-même n'a pas abouti.
    step_skip "étape 6 non exécutée — note_id indisponible (dépend de l'étape 4)"
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
        # Events auth WARN-tolerant (l'événement de succès d'échange n'est pas câblé).
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
    # POST /vault_downgrade est SYNCHRONE depuis le retrait du moteur legacy : le handler
    # rend `Json<VaultDowngradeResponse>`, soit 200 + corps. La variante asynchrone par
    # queue (202) n'existe plus ; l'accepter laissait passer l'assertion sur un contrat
    # disparu. Le corps est vérifié, pas seulement le code : un 200 dont le statut n'est
    # pas `downgraded` n'est pas un nettoyage. Même verdict que smoke-post-deploy-1.sh.
    DOWNGRADE_OUT=$(curl -sS -w $'\n%{http_code}' \
        -X POST "$SERVER/api/v1/vault_downgrade" \
        -H "Authorization: Bearer $JWT" \
        -H "Content-Type: application/json" \
        -d "{\"note_id\":\"$NOTE_ID\",\"reason\":\"post-deploy smoke cleanup\",\"tenant_id\":\"main\"}" 2>/dev/null) \
        || DOWNGRADE_OUT="000"
    DOWNGRADE_HTTP_CODE=${DOWNGRADE_OUT##*$'\n'}
    DOWNGRADE_BODY=${DOWNGRADE_OUT%$'\n'*}
    if [[ "$DOWNGRADE_HTTP_CODE" == "200" ]] \
        && printf '%s' "$DOWNGRADE_BODY" | jq -e '.status == "downgraded"' >/dev/null 2>&1; then
        step_pass "8" "note downgradée (HTTP 200, status=downgraded)"
    elif [[ "$DOWNGRADE_HTTP_CODE" == "403" ]]; then
        step_skip "vault_downgrade → 403 — nettoyage non validé (même cause ACL que les étapes 4+6)"
    elif [[ "$DOWNGRADE_HTTP_CODE" == "404" ]]; then
        step_skip "vault_downgrade → 404 (note absente : rien n'a pu être nettoyé)"
    else
        step_fail "vault_downgrade → HTTP $DOWNGRADE_HTTP_CODE inattendu (attendu 200 + status=downgraded)"
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
echo "RÉSULTAT smoke-post-deploy-2 : PASS=$STEPS_PASS  WARN=$STEPS_WARN  FAIL=$STEPS_FAIL  SKIP=$STEPS_SKIP" \
     " (/$ACCOUNTED comptabilisées sur $STEPS_TOTAL attendues)"
if [[ "$STEPS_FAIL" -gt 0 ]]; then
    echo -e "${RED}smoke-post-deploy-2 FAIL${RESET} — $STEPS_FAIL étape(s) en échec dur"
    exit 1
fi
if [[ "$ACCOUNTED" -ne "$STEPS_TOTAL" ]]; then
    # Ni PASS, ni WARN, ni FAIL, ni SKIP sur une étape annoncée : le script s'est
    # interrompu en route. Sans `set -e`, c'est le seul témoin d'une mort silencieuse.
    echo -e "${RED}smoke-post-deploy-2 INCOMPLETE${RESET} — $ACCOUNTED étapes comptabilisées sur $STEPS_TOTAL attendues"
    exit 2
fi
if [[ "$STEPS_WARN" -gt 0 || "$STEPS_SKIP" -gt 0 ]]; then
    echo -e "${YELLOW}smoke-post-deploy-2 INCOMPLETE${RESET} — $STEPS_WARN WARN + $STEPS_SKIP étape(s) non exécutée(s) :"
    echo "    aucune de ces étapes n'a validé quoi que ce soit ; un WARN ne fait pas un PASS."
    exit 2
fi
echo -e "${GREEN}smoke-post-deploy-2 PASS${RESET} — $STEPS_PASS/$STEPS_TOTAL étapes vérifiées"
exit 0
