#!/usr/bin/env bash
#
# gen-fixture.sh — génère, HORS DÉPÔT, un répertoire d'état gradatum 2.4.0
# destiné à être monté en version par le binaire 2.5.0.
#
# Le dépôt ne reçoit que du texte (corpus markdown, manifeste, gabarits). Rien de
# binaire n'est commité : les bases (`index.db`, `queue.sqlite`, `vault/`) sont
# produites à l'exécution par le binaire 2.4.0 fourni par l'appelant.
#
# Trois gabarits texte sont lus dans fixture/ :
#   server.toml.tmpl   — configuration rendue après `init`, sans aucune clé
#   acl-preset.toml    — preset ACL déclarant les identités d'auteur du corpus
#   manifest.jsonl     — ordre d'écriture, auteurs attendus, comptes attendus,
#                        et les cartes de fonctionnalité à créer par le serveur
#
# Usage :
#   bash tests/upgrade/gen-fixture.sh \
#       --admin-bin  <chemin du gradatum-admin 2.4.0> \
#       --server-bin <chemin du gradatum-server 2.4.0> \
#       --worker-bin <chemin du gradatum-worker 2.4.0> \
#       --out <répertoire d'état, hors dépôt> \
#       [--port N] [--force]
#
# `--worker-bin` n'est pas décoratif : `gradatum-server` n'embarque aucun
# consommateur de file. Sans travailleur, les notes ne sont jamais matérialisées
# et la file ne se vide pas — la fixture produite serait vide.
#
# Codes de retour :
#   0  état produit
#   1  erreur d'exécution (binaire, configuration, API)
#   2  NON EXÉCUTÉ — au moins un des quatre ports nécessaires est occupé
#   3  refus d'argument (répertoire de sortie dans le dépôt, déjà peuplé, port interdit)
#
# Ce script ne consulte aucun chemin implicite : les binaires sont ceux passés en
# argument, jamais un binaire du parc.

set -euo pipefail

# Tout ce que ce script crée naît privé. Le répertoire d'état porte des secrets
# mintés (jetons d'API interne, clés) et des journaux : les processus du harnais
# tournent sous le même utilisateur, donc rien n'a besoin d'être lisible par un
# autre. Ce masque ne peut que resserrer les modes posés par `init`, jamais les
# élargir — et il couvre les journaux, qu'un futur chemin de code pourrait
# imprégner d'un jeton écrit sur stdout.
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd -P)"
readonly REPO_ROOT
readonly FIXTURE_DIR="$SCRIPT_DIR/fixture"

# Ports du parc interdisant toute écoute de la part de ce harnais.
readonly -a FORBIDDEN_PORTS=(8436 19090 19091 19092 19094 19190)

ADMIN_BIN=""
SERVER_BIN=""
WORKER_BIN=""
OUT=""
PORT=19290
FORCE=0

SERVER_PID=""
WORKER_PID=""

log() { printf '[gen-fixture] %s\n' "$*" >&2; }
die() { local rc="$1"; shift; printf '[gen-fixture] ERREUR: %s\n' "$*" >&2; exit "$rc"; }

usage() {
  cat >&2 <<'EOF'
Usage: bash tests/upgrade/gen-fixture.sh \
           --admin-bin <p> --server-bin <p> --worker-bin <p> \
           --out <répertoire hors dépôt> [--port N] [--force]

  --port N   port de l'API publique ; les trois suivants (N+1 métriques,
             N+2 API interne, N+3 /health travailleur) en sont dérivés.
             Défaut : 19290. Les quatre sont sondés avant tout démarrage ;
             un port occupé ⇒ code 2 (NON EXÉCUTÉ).
EOF
}

# ── Arguments ────────────────────────────────────────────────────────────────

while (($#)); do
  case "$1" in
    --admin-bin)  ADMIN_BIN="${2-}";  shift 2 ;;
    --server-bin) SERVER_BIN="${2-}"; shift 2 ;;
    --worker-bin) WORKER_BIN="${2-}"; shift 2 ;;
    --out)        OUT="${2-}";        shift 2 ;;
    --port)       PORT="${2-}";       shift 2 ;;
    --force)      FORCE=1;            shift ;;
    -h|--help)    usage; exit 0 ;;
    *)            die 3 "argument inconnu : $1" ;;
  esac
done

[[ -n "$ADMIN_BIN"  ]] || die 3 "--admin-bin est requis"
[[ -n "$SERVER_BIN" ]] || die 3 "--server-bin est requis"
[[ -n "$WORKER_BIN" ]] || die 3 "--worker-bin est requis"
[[ -n "$OUT"        ]] || die 3 "--out est requis"
[[ "$PORT" =~ ^[0-9]+$ ]] || die 3 "--port doit être un entier (reçu : $PORT)"
(( PORT >= 1024 && PORT <= 65535 - 3 )) || die 3 "--port hors plage utilisable : $PORT"

for bin in "$ADMIN_BIN" "$SERVER_BIN" "$WORKER_BIN"; do
  [[ -f "$bin" ]] || die 3 "binaire introuvable : $bin"
  [[ -x "$bin" ]] || die 3 "binaire non exécutable : $bin"
  resolved="$(readlink -f -- "$bin")"
  case "$(dirname -- "$resolved")" in
    /bin | /sbin | /usr/bin | /usr/sbin | /usr/local/bin | /usr/local/sbin)
      die 3 "refus d'un binaire installé dans un répertoire système ($resolved) : \
ce harnais ne doit jamais invoquer un binaire du parc" ;;
  esac
done

for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || die 1 "outil requis absent : $tool"
done

# Le répertoire de sortie ne doit jamais tomber dans le dépôt (R-E5 : aucune
# base générée n'est versionnée).
mkdir -p -- "$OUT" || die 1 "création impossible : $OUT"
OUT="$(cd -- "$OUT" && pwd -P)"
case "$OUT" in
  "$REPO_ROOT" | "$REPO_ROOT"/*)
    die 3 "le répertoire de sortie est dans le dépôt ($OUT) — il doit vivre hors de $REPO_ROOT" ;;
esac
if [[ -n "$(ls -A -- "$OUT" 2>/dev/null)" ]]; then
  if (( FORCE )); then
    log "répertoire de sortie non vide — purgé (--force) : $OUT"
    find "$OUT" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
  else
    die 3 "répertoire de sortie déjà peuplé : $OUT (utiliser --force pour le purger)"
  fi
fi

# ── Ports ────────────────────────────────────────────────────────────────────
#
# Quatre écouteurs sont nécessaires : API publique, métriques, API interne
# serveur↔travailleur, /health du travailleur. Tous sont sondés AVANT le premier
# démarrage : un port occupé est une non-exécution (code 2), jamais un échec
# masqué par un service qui écoute ailleurs.

readonly API_PORT="$PORT"
readonly METRICS_PORT=$((PORT + 1))
readonly INTERNAL_PORT=$((PORT + 2))
readonly HEALTH_PORT=$((PORT + 3))

port_in_use() { ( : >"/dev/tcp/127.0.0.1/$1" ) 2>/dev/null; }

for p in "$API_PORT" "$METRICS_PORT" "$INTERNAL_PORT" "$HEALTH_PORT"; do
  for forbidden in "${FORBIDDEN_PORTS[@]}"; do
    [[ "$p" != "$forbidden" ]] || die 3 "port $p interdit : réservé au parc"
  done
done
for p in "$API_PORT" "$METRICS_PORT" "$INTERNAL_PORT" "$HEALTH_PORT"; do
  if port_in_use "$p"; then
    log "port occupé : $p"
    die 2 "NON EXÉCUTÉ — port $p déjà en écoute sur 127.0.0.1 ; relancer avec --port <autre>"
  fi
done
log "ports retenus : api=$API_PORT métriques=$METRICS_PORT interne=$INTERNAL_PORT santé=$HEALTH_PORT"

# ── Arrêt des processus (par PID capturé — jamais de motif) ──────────────────

stop_pid() {
  local pid="$1" name="$2"
  [[ -n "$pid" ]] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  kill -TERM "$pid" 2>/dev/null || true
  local deadline=$((SECONDS + 20))
  while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 1; done
  if kill -0 "$pid" 2>/dev/null; then
    log "$name (pid $pid) ne répond pas au TERM — KILL"
    kill -KILL "$pid" 2>/dev/null || true
  fi
  log "$name arrêté (pid $pid)"
}

cleanup() {
  local rc=$?
  stop_pid "$WORKER_PID" worker
  stop_pid "$SERVER_PID" serveur
  return "$rc"
}
trap cleanup EXIT

# ── Initialisation du répertoire racine ──────────────────────────────────────

mkdir -p -- "$OUT/logs"
readonly SERVER_LOG="$OUT/logs/server.log"
readonly WORKER_LOG="$OUT/logs/worker.log"

log "init du répertoire racine : $OUT"
"$ADMIN_BIN" init --root "$OUT" --bind "127.0.0.1:$API_PORT" \
  --preset "$FIXTURE_DIR/acl-preset.toml" --non-interactive \
  >"$OUT/logs/init.out" 2>&1 \
  || die 1 "gradatum-admin init a échoué (voir $OUT/logs/init.out)"

readonly GEN_CONFIG="$OUT/config/server.toml"

# ── Gabarit ──────────────────────────────────────────────────────────────────
#
# Les deux jetons d'API interne sont mintés par `init` et vivent dans le
# server.toml généré. Le gabarit commité n'en porte aucun : on les relit ici et
# on les réinjecte, si bien que le dépôt ne contient jamais de clé.

section_value() {
  sed -n "/^\[$2\]/,/^\[/p" "$1" | sed -n "s/^$3 = \"\(.*\)\"\$/\1/p" | head -n 1
}

INTERNAL_TOKEN="$(section_value "$GEN_CONFIG" internal_api token)"
ADMIN_TOKEN="$(section_value "$GEN_CONFIG" internal_api admin_token)"
[[ -n "$INTERNAL_TOKEN" ]] || die 1 "jeton d'API interne introuvable dans $GEN_CONFIG"
[[ -n "$ADMIN_TOKEN"    ]] || die 1 "jeton d'administration introuvable dans $GEN_CONFIG"

escape_sed() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

log "rendu du gabarit server.toml.tmpl"
sed \
  -e "s|@@ROOT@@|$(escape_sed "$OUT")|g" \
  -e "s|@@API_PORT@@|$API_PORT|g" \
  -e "s|@@METRICS_PORT@@|$METRICS_PORT|g" \
  -e "s|@@INTERNAL_PORT@@|$INTERNAL_PORT|g" \
  -e "s|@@HEALTH_PORT@@|$HEALTH_PORT|g" \
  -e "s|@@INTERNAL_TOKEN@@|$(escape_sed "$INTERNAL_TOKEN")|g" \
  -e "s|@@ADMIN_TOKEN@@|$(escape_sed "$ADMIN_TOKEN")|g" \
  "$FIXTURE_DIR/server.toml.tmpl" >"$GEN_CONFIG"
# Le fichier rendu porte les deux jetons d'API interne : mode propriétaire
# explicite, sans dépendre du seul masque hérité.
chmod 600 -- "$GEN_CONFIG"

# ── Démarrage du serveur ─────────────────────────────────────────────────────

log "démarrage du serveur 2.4.0"
"$SERVER_BIN" --config "$GEN_CONFIG" >>"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
log "serveur pid=$SERVER_PID (journal : $SERVER_LOG)"

BASE_URL="http://127.0.0.1:$API_PORT"
HEALTH_BODY=""
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  if HEALTH_BODY="$(curl -fsS --max-time 2 "$BASE_URL/health" 2>/dev/null)"; then
    break
  fi
  HEALTH_BODY=""
  kill -0 "$SERVER_PID" 2>/dev/null || die 1 "le serveur est mort avant d'être prêt (voir $SERVER_LOG)"
  sleep 1
done
[[ -n "$HEALTH_BODY" ]] || die 1 "le serveur n'a pas répondu sur $BASE_URL/health en 60 s"

BUILD_SHA="$(printf '%s' "$HEALTH_BODY" | jq -r '.build_sha // empty')"
[[ -n "$BUILD_SHA" ]] || die 1 "build_sha absent de /health : $HEALTH_BODY"
log "serveur prêt — build_sha=$BUILD_SHA"

# ── Jetons, un par identité d'auteur ─────────────────────────────────────────
#
# La règle R2 veut que l'auteur d'une note soit le sujet du porteur authentifié,
# jamais un champ de la requête : deux auteurs distincts dans l'état produit
# exigent donc deux identités déclarées (voir fixture/acl-preset.toml) et deux
# jetons. Les secrets sont mintés dans le répertoire de sortie et n'en sortent
# pas ; le dépôt n'en porte aucun.

gen_secret() {  # $1 = identité — écrit le secret (nu) sur stdout
  local out
  out="$("$ADMIN_BIN" api-key create --root "$OUT" --owner "$1" --scopes admin \
    --tenant main --description "fixture $1" 2>>"$OUT/logs/api-key.log")" || return 1
  # Seule la ligne qui EST le secret est retenue : la commande peut préfacer sa
  # sortie, et un secret capturé avec du bavardage autour ne vaudrait rien.
  printf '%s\n' "$out" | sed -n 's/^\(ak_[0-9a-f]\{8,\}\)$/\1/p' | head -n 1
}

exchange() {  # $1 = secret — écrit le jeton sur stdout
  curl -fsS -X POST "$BASE_URL/auth/exchange" \
    -H "Authorization: Bearer $1" \
    -H "Content-Type: application/json" \
    -d '{}' | jq -r '.token // empty'
}

MANIFEST="$FIXTURE_DIR/manifest.jsonl"
[[ -f "$MANIFEST" ]] || die 1 "manifeste absent : $MANIFEST"

declare -A JWT_OF=()
first_author=""
while IFS= read -r author; do
  [[ -n "$author" ]] || continue
  secret="$(gen_secret "$author")" \
    || die 1 "création de clé refusée pour l'identité $author"
  [[ -n "$secret" ]] || die 1 "secret vide pour l'identité $author"
  token="$(exchange "$secret")" || die 1 "échange refusé pour l'identité $author"
  [[ -n "$token" ]] || die 1 "jeton vide pour l'identité $author"
  JWT_OF["$author"]="$token"
  [[ -n "$first_author" ]] || first_author="$author"
  log "identité $author : clé mintée, jeton obtenu"
done < <(jq -r 'select(.type == "write" or .type == "feature_card") | .author' "$MANIFEST" | sort -u)

[[ -n "$first_author" ]] || die 1 "aucun auteur dans le manifeste"
(( ${#JWT_OF[@]} >= 2 )) \
  || die 1 "moins de deux identités d'auteur dans le manifeste (${#JWT_OF[@]})"

# Porteur du plan de contrôle (file de travaux, relevés) : la première identité,
# qui porte le périmètre `admin`.
JWT="${JWT_OF[$first_author]}"

# ── Démarrage du travailleur ─────────────────────────────────────────────────

log "démarrage du travailleur 2.4.0"
GRADATUM_INTERNAL_URL="http://127.0.0.1:$INTERNAL_PORT" \
GRADATUM_INTERNAL_TOKEN="$INTERNAL_TOKEN" \
  "$WORKER_BIN" --config "$GEN_CONFIG" --db "$OUT/db/queue.sqlite" \
  >>"$WORKER_LOG" 2>&1 &
WORKER_PID=$!
log "travailleur pid=$WORKER_PID (journal : $WORKER_LOG)"

# ── Écriture du corpus ───────────────────────────────────────────────────────

count_jobs() {  # $1 = statut
  curl -fsS --max-time 5 "$BASE_URL/api/v1/jobs?status=$1&limit=200" \
    -H "Authorization: Bearer $JWT" | jq -r '.items | length'
}

# Le corps de requête ne porte JAMAIS `author` : R2 veut que l'auteur soit le
# sujet du porteur. L'auteur attendu du manifeste sélectionne donc le jeton, il
# ne s'énonce pas — d'où une identité déclarée par auteur.
post_write() {  # $1 = ligne du manifeste
  local line="$1"
  local file title section author note_id payload token
  file="$(jq -r '.file' <<<"$line")"
  title="$(jq -r '.title' <<<"$line")"
  section="$(jq -r '.section' <<<"$line")"
  author="$(jq -r '.author' <<<"$line")"
  note_id="$(jq -r '.note_id' <<<"$line")"
  token="${JWT_OF[$author]:-}"
  [[ -n "$token" ]] || die 1 "aucun jeton pour l'auteur $author ($file)"
  [[ -f "$FIXTURE_DIR/$file" ]] || die 1 "corps de note absent : $FIXTURE_DIR/$file"
  payload="$(jq -n \
    --arg t "$title" \
    --arg b "$(cat "$FIXTURE_DIR/$file")" \
    --arg s "$section" \
    --arg id "$note_id" \
    --argjson tags "$(jq -c '.tags' <<<"$line")" \
    '{title: $t, body: $b, section_hint: $s, tags: $tags,
      tenant_id: "main", note_id: $id}')"
  local http_body
  http_body="$(curl -sS -X POST "$BASE_URL/api/v1/vault_write" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" \
    -d "$payload")" \
    || die 1 "vault_write injoignable pour $note_id ($file)"
  jq -e 'has("job_id")' >/dev/null 2>&1 <<<"$http_body" \
    || die 1 "vault_write refusé pour $note_id ($file) : $http_body"
  printf '%s' "$http_body" | jq -r '.job_id'
}

# Attend qu'un travail d'écriture atteigne un état terminal. Nécessaire AVANT de
# poser une carte de travail : le serveur résout `[[track:...]]` à l'enqueue, et
# la cible n'existe dans l'index qu'une fois la carte de structure curée.
wait_job_done() {  # $1 = job_id, $2 = étiquette
  local job_id="$1" label="$2" status deadline
  deadline=$((SECONDS + 120))
  while (( SECONDS < deadline )); do
    status="$(curl -fsS --max-time 5 "$BASE_URL/api/v1/jobs/$job_id/v2" \
      -H "Authorization: Bearer $JWT" | jq -r '.lifecycle.status // empty' 2>/dev/null || true)"
    case "$status" in
      Done) return 0 ;;
      Failed | DLQ | Cancelled | Conflict)
        die 1 "$label : travail $job_id terminé en $status" ;;
    esac
    sleep 2
  done
  die 1 "$label : travail $job_id non terminé après 120 s (dernier état : ${status:-inconnu})"
}

write_count=0
while IFS= read -r line; do
  [[ "$(jq -r '.type' <<<"$line")" == "write" ]] || continue
  job_id="$(post_write "$line")"
  write_count=$((write_count + 1))
  # Barrière : une carte project-map doit être indexée avant la suivante, sans
  # quoi la résolution de `[[track:...]]` échoue en 400 à l'enqueue.
  if [[ "$(jq -r '.section' <<<"$line")" == "project-map" ]]; then
    wait_job_done "$job_id" "carte project-map $(jq -r '.file' <<<"$line")"
    log "carte indexée : $(jq -r '.file' <<<"$line")"
  fi
done <"$MANIFEST"
log "corpus posé : $write_count notes"
(( write_count > 0 )) || die 1 "aucune note écrite — manifeste illisible ?"

# ── Vidage de la file ────────────────────────────────────────────────────────

log "attente du vidage de la file (≤ 300 s)"
deadline=$((SECONDS + 300))
while (( SECONDS < deadline )); do
  pending="$(count_jobs pending 2>/dev/null || printf 'x')"
  running="$(count_jobs running 2>/dev/null || printf 'x')"
  if [[ "$pending" == "0" && "$running" == "0" ]]; then break; fi
  kill -0 "$WORKER_PID" 2>/dev/null || die 1 "le travailleur est mort (voir $WORKER_LOG)"
  sleep 2
done
[[ "${pending:-x}" == "0" && "${running:-x}" == "0" ]] \
  || die 1 "file non vidée après 300 s (pending=$pending running=$running)"
log "file vidée"

# ── Cartes de fonctionnalité — identité allouée par le serveur ───────────────
#
# L'axe « export de cartes » du harnais de montée compare le relevé
# `project-map export-features` octet à octet entre l'état d'avant et l'état
# monté. Un état sans aucune carte de fonctionnalité rendrait cet axe VACUE :
# deux exports vides sont identiques, la parité serait vraie sans rien couvrir.
# Ces cartes rendent l'axe opérant — c'est leur unique raison d'être ici.
#
# Le numéro d'une carte est son IDENTITÉ : le serveur l'alloue, l'appelant ne le
# fournit jamais (une écriture externe qui porterait ce rôle est refusée). Le
# corps ne porte donc que les cinq rôles non-identitaires, et la route de
# création fait le reste. Chaque travail est attendu jusqu'à son état terminal
# avant de passer au suivant : une carte créée mais non curée ne serait pas dans
# l'index, donc pas dans l'export.

feature_count=0
feature_list=""
while IFS= read -r line; do
  [[ "$(jq -r '.type' <<<"$line")" == "feature_card" ]] || continue
  f_file="$(jq -r '.file' <<<"$line")"
  f_author="$(jq -r '.author' <<<"$line")"
  f_title="$(jq -r '.title' <<<"$line")"
  f_token="${JWT_OF[$f_author]:-}"
  [[ -n "$f_token" ]] || die 1 "aucun jeton pour l'auteur $f_author ($f_file)"
  [[ -f "$FIXTURE_DIR/$f_file" ]] || die 1 "corps de carte absent : $FIXTURE_DIR/$f_file"
  f_resp="$(curl -sS -X POST "$BASE_URL/api/v1/project-map/create-feature" \
    -H "Authorization: Bearer $f_token" \
    -H "Content-Type: application/json" \
    -d "$(jq -n \
          --arg t "$f_title" \
          --arg b "$(cat "$FIXTURE_DIR/$f_file")" \
          --argjson tags "$(jq -c '.tags' <<<"$line")" \
          '{title: $t, body: $b, tags: $tags, tenant_id: "main"}')")" \
    || die 1 "create-feature injoignable ($f_file)"
  f_job="$(jq -r '.job_id // empty' <<<"$f_resp")"
  f_feature="$(jq -r '.feature // empty' <<<"$f_resp")"
  f_number="$(jq -r '.number // empty' <<<"$f_resp")"
  [[ -n "$f_job" && -n "$f_feature" ]] \
    || die 1 "create-feature refusé ($f_file) : $f_resp"
  wait_job_done "$f_job" "carte de fonctionnalité $f_file"
  feature_count=$((feature_count + 1))
  feature_list="$feature_list $f_feature"
  log "carte de fonctionnalité créée : $f_feature (numéro $f_number) — $f_file"
done <"$MANIFEST"
(( feature_count >= 2 )) \
  || die 1 "moins de 2 cartes de fonctionnalité dans le manifeste ($feature_count)"
notes_total=$((write_count + feature_count))
log "cartes de fonctionnalité posées :$feature_list"

# La fixture se vérifie ELLE-MÊME sur la commande que le harnais compare : une
# carte créée mais absente de l'export ferait échouer la montée pour une raison
# qu'il vaut mieux voir ici, à la production de l'état.
"$ADMIN_BIN" project-map export-features --root "$OUT" --vault main --include-dropped \
  >"$OUT/logs/export-features.json" 2>"$OUT/logs/export-features.err" \
  || die 1 "export-features a échoué sur l'état produit (voir $OUT/logs/export-features.err)"
exported="$(jq 'length' "$OUT/logs/export-features.json" 2>/dev/null || printf 'x')"
[[ "$exported" =~ ^[0-9]+$ ]] \
  || die 1 "export-features illisible : $OUT/logs/export-features.json"
(( exported >= feature_count )) \
  || die 1 "export-features rend $exported carte(s) pour $feature_count carte(s) créée(s)"
log "export des cartes : $exported entrée(s) —$(jq -r '[.[].feature] | map(" " + .) | add // ""' \
  "$OUT/logs/export-features.json")"

# ── Une entrée en file morte ─────────────────────────────────────────────────

dlq_line="$(grep -m 1 '"type": "dlq_job"' "$MANIFEST" || true)"
[[ -n "$dlq_line" ]] || die 1 "aucune ligne dlq_job dans le manifeste"
dlq_note_id="$(jq -r '.note_id' <<<"$dlq_line")"

log "pose du travail voué à la file morte (Embed sur $dlq_note_id)"
curl -fsS -X POST "$BASE_URL/api/v1/jobs" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: gen-fixture-dlq-1" \
  -d "$(jq -n --arg id "$dlq_note_id" \
        '{spec: {kind: {type: "Embed", data: {note_id: $id, tenant_id: "main", force_regenerate: false}}}}')" \
  >/dev/null || die 1 "POST /api/v1/jobs (Embed) refusé"

log "attente de la promotion en file morte (≤ 180 s)"
deadline=$((SECONDS + 180))
dlq_count=0
while (( SECONDS < deadline )); do
  dlq_count="$(count_jobs dlq 2>/dev/null || printf '0')"
  (( dlq_count >= 1 )) && break
  kill -0 "$WORKER_PID" 2>/dev/null || die 1 "le travailleur est mort (voir $WORKER_LOG)"
  sleep 2
done
(( dlq_count >= 1 )) || die 1 "aucun travail en file morte après 180 s"
log "file morte peuplée : $dlq_count entrée(s)"

# ── Arrêt du travailleur, puis pose du travail restant en file ───────────────

stop_pid "$WORKER_PID" worker
WORKER_PID=""

pending_posted=0
while IFS= read -r line; do
  [[ "$(jq -r '.type' <<<"$line")" == "pending_job" ]] || continue
  seq="$(jq -r '.seq' <<<"$line")"
  note_id="$(jq -r '.note_id' <<<"$line")"
  spec="$(jq -n \
    --arg id "$note_id" \
    --arg t "$(jq -r '.title' <<<"$line")" \
    --arg b "$(cat "$FIXTURE_DIR/$(jq -r '.file' <<<"$line")")" \
    --arg a "$(jq -r '.author' <<<"$line")" \
    --arg s "$(jq -r '.section' <<<"$line")" \
    --argjson tags "$(jq -c '.tags' <<<"$line")" \
    '{spec: {kind: {type: "Curate", data: {
        note_id: $id, tenant_id: "main", title: $t, body: $b,
        author: $a, tags: $tags, section_hint: $s}}}}')"
  curl -fsS -X POST "$BASE_URL/api/v1/jobs" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    -H "Idempotency-Key: gen-fixture-pending-$seq" \
    -d "$spec" >/dev/null || die 1 "POST /api/v1/jobs (Curate $seq) refusé"
  pending_posted=$((pending_posted + 1))
done <"$MANIFEST"
log "travaux laissés en file : $pending_posted"
(( pending_posted >= 5 )) || die 1 "moins de 5 travaux en file ($pending_posted)"

final_pending="$(count_jobs pending)"
[[ "$final_pending" == "$pending_posted" ]] \
  || die 1 "attendu $pending_posted travaux en attente, lu $final_pending"

# ── Arrêt du serveur et relevé final ─────────────────────────────────────────

stop_pid "$SERVER_PID" serveur
SERVER_PID=""

log "état produit dans $OUT"
log "  build_sha (générateur) : $BUILD_SHA"
log "  notes posées           : $notes_total (dont $feature_count carte(s) de fonctionnalité)"
log "  travaux en attente     : $final_pending"
log "  travaux en file morte  : $dlq_count"
printf 'gen-fixture: build_sha=%s notes=%s cartes=%s pending=%s dlq=%s out=%s\n' \
  "$BUILD_SHA" "$notes_total" "$feature_count" "$final_pending" "$dlq_count" "$OUT"
