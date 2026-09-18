#!/usr/bin/env bash
#
# run-upgrade-test.sh — harnais de montée de version 2.4.0 → 2.5.0.
#
# Il monte un état gradatum produit par la version PRÉCÉDENTE avec la version
# COURANTE, et compare quatre relevés à tolérance nulle : migrations, listing,
# recherche, export de cartes, comptes. Le retour arrière (redémarrage du
# binaire 2.4.0 sur le répertoire MONTÉ) est exercé, pas supposé.
#
# Usage :
#   bash tests/upgrade/run-upgrade-test.sh \
#       --state <répertoire d'état 2.4.0> \
#       --admin-bin-240 <chemin> --server-bin-240 <chemin> \
#       --admin-bin-250 <chemin> --server-bin-250 <chemin> \
#       [--port N] [--out <répertoire>] \
#       [--worker-bin-250 <chemin>] [--fault-inject none|delete-note]
#
#   --state            état produit par `gen-fixture.sh` (hors dépôt).
#   --admin-bin-240    `gradatum-admin` 2.4.0 — export des cartes du relevé AVANT.
#   --server-bin-240   `gradatum-server` 2.4.0 — le serveur AVANT et le retour arrière.
#   --admin-bin-250    `gradatum-admin` 2.5.0 — export des cartes des relevés APRÈS.
#   --server-bin-250   `gradatum-server` 2.5.0 — le serveur MONTÉ.
#   --port N           port d'API. Défaut : celui inscrit dans la configuration de
#                      l'état (`<state>/config/server.toml`), qui fait foi — les
#                      trois ports dérivés (métriques N+1, API interne N+2,
#                      /health travailleur N+3) en découlent. Une valeur fournie
#                      qui CONTREDIT la configuration est un refus (code 2) : le
#                      harnais n'écoute jamais sur un port qui ne serait pas
#                      celui de l'état mesuré.
#   --out              répertoire des mesures (hors dépôt). Défaut :
#                      `<état>/../mesures-montee`.
#   --worker-bin-250   OPTIONNEL, hors commande CI. Sans lui, l'axe F-XX (ruling
#                      E4) est exercé jusqu'à l'enfilement : l'allocation du
#                      numéro et l'injection du rôle sont prouvées sur le travail
#                      enfilé, la MATÉRIALISATION de la carte ne l'est pas (elle
#                      exige un consommateur de file). Avec lui, la carte est
#                      matérialisée et l'assertion porte sur sa présence.
#   --fault-inject     DIAGNOSTIC, hors commande CI. `delete-note` retire une note
#                      de l'index entre le relevé AVANT et le relevé APRÈS : c'est
#                      la preuve de rouge du brief (Step 5-ii), et elle doit
#                      nommer la note. Jamais 0 dans ce mode.
#
# La clé (e) « export de cartes » est comparée selon le SENS de la comparaison,
# et la force retenue est écrite dans chaque rapport de parité :
#   montée (2.4.0 → 2.5.0)   force `additif` — l'export est un RAPPORT, son
#                            dialecte peut GAGNER des clés. Chaque entrée du
#                            relevé AVANT doit être égale à l'entrée du relevé
#                            MONTÉ projetée sur les clés du dialecte AVANT (même
#                            nombre de cartes, même ordre) ; une clé AVANT qui
#                            disparaît ou change de valeur est un écart, et une
#                            clé ajoutée est relevée puis exigée CITÉE dans le
#                            guide de montée — sans guide lisible, la clé n'est
#                            pas mesurée du tout (code 2).
#   retour arrière (2.4.0)   force `strict` — c'est le MÊME binaire des deux
#                            côtés : octet à octet (sha256 + cmp), rien ne bouge.
#
# Codes de retour :
#   0  parité prouvée sur toutes les clés bloquantes, retour arrière exercé
#   1  divergence — chaque écart est NOMMÉ avec ses deux valeurs
#   2  NON EXÉCUTÉ — binaire absent, port occupé, état illisible, outil requis
#      absent, `--port` en contradiction avec la configuration de l'état,
#      réalignement du preset ACL refusé (formulaire non automatisable).
#      ⚠ Un `2` n'est JAMAIS un vert : c'est une mesure qui n'a pas eu lieu.
#      ⚠ Une incapacité de l'OUTILLAGE sort en `2`, jamais en `1` : le `1` est
#      réservé à une divergence MESURÉE entre l'AVANT et l'APRÈS.
#
# Aucune mesure n'est seulement affichée : tout est écrit sous `--out`.
#
# Provenance : le `build_sha` et la `version` sont LUS sur `/health` de chaque
# instance démarrée, jamais déduits d'un chemin de binaire.

set -euo pipefail

# L'état porte des secrets mintés et la configuration les relit : rien de ce que
# ce harnais crée ne doit être lisible par un autre utilisateur.
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd -P)"
readonly REPO_ROOT

# Ports du parc : ce harnais n'écoute jamais sur un port du parc, et ne sonde
# que ceux de l'état mesuré.
readonly -a FORBIDDEN_PORTS=(8436 19090 19091 19092 19094 19190)

# Le manifeste de la fixture porte les attendus de l'état mesuré, dont le nombre
# de cartes de fonctionnalité que l'export DOIT rendre. Il est LU — sans lui, le
# seuil de non-vacuité de l'export serait postulé, et un seuil postulé ne
# distingue pas « l'export est intact » de « l'export est vide ».
readonly MANIFEST="$SCRIPT_DIR/fixture/manifest.jsonl"

STATE=""
ADMIN_240=""
SERVER_240=""
ADMIN_250=""
SERVER_250=""
WORKER_250=""
PORT=""
OUT=""
FAULT_INJECT="none"

SERVER_PID=""
WORKER_PID=""
RUN_RC=0
PHASE_START=0
# Relevés d'export pris, et combien d'entre eux n'ont PAS satisfait l'assertion
# de non-vacuité. Comptés à part du verdict : ils disent sur quoi il a porté.
EXPORT_RELEVES=0
EXPORT_ECHECS=0

log()  { printf '[montee] %s\n' "$*" >&2; }
die()  { local rc="$1"; shift; printf '[montee] NON EXÉCUTÉ: %s\n' "$*" >&2; exit "$rc"; }
bad()  { printf '[montee] ERREUR: %s\n' "$*" >&2; }

usage() {
  sed -n '/^# Usage :/,/^# Provenance/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
}

# ── Arguments ────────────────────────────────────────────────────────────────

while (($#)); do
  case "$1" in
    --state)          STATE="${2-}";       shift 2 ;;
    --admin-bin-240)  ADMIN_240="${2-}";   shift 2 ;;
    --server-bin-240) SERVER_240="${2-}";  shift 2 ;;
    --admin-bin-250)  ADMIN_250="${2-}";   shift 2 ;;
    --server-bin-250) SERVER_250="${2-}";  shift 2 ;;
    --worker-bin-250) WORKER_250="${2-}";  shift 2 ;;
    --port)           PORT="${2-}";        shift 2 ;;
    --out)            OUT="${2-}";         shift 2 ;;
    --fault-inject)   FAULT_INJECT="${2-}"; shift 2 ;;
    -h|--help)        usage; exit 0 ;;
    *)                die 2 "argument inconnu : $1" ;;
  esac
done

[[ -n "$STATE"      ]] || die 2 "--state est requis"
[[ -n "$ADMIN_240"  ]] || die 2 "--admin-bin-240 est requis"
[[ -n "$SERVER_240" ]] || die 2 "--server-bin-240 est requis"
[[ -n "$ADMIN_250"  ]] || die 2 "--admin-bin-250 est requis"
[[ -n "$SERVER_250" ]] || die 2 "--server-bin-250 est requis"
case "$FAULT_INJECT" in
  none|delete-note) ;;
  *) die 2 "--fault-inject doit valoir none ou delete-note (reçu : $FAULT_INJECT)" ;;
esac

# ── Outils requis ────────────────────────────────────────────────────────────
#
# `python3` n'est pas décoratif : c'est lui qui sonde les ports par BIND réel
# (une connexion ne prouve pas qu'un port est libre) et qui lit les comptes de
# tables et les migrations. Son absence est une non-exécution, pas un échec.

for tool in curl jq python3 cmp sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die 2 "outil requis absent : $tool"
done

[[ -f "$MANIFEST" ]] || die 2 "manifeste de la fixture introuvable : $MANIFEST"
FEATURE_CARDS_EXPECTED="$(jq -r 'select(.type == "expected_counts") | .feature_cards // empty' \
  "$MANIFEST" 2>/dev/null | tail -n 1)"
[[ "$FEATURE_CARDS_EXPECTED" =~ ^[0-9]+$ ]] \
  || die 2 "manifeste sans compte « feature_cards » exploitable : $MANIFEST"
(( FEATURE_CARDS_EXPECTED > 0 )) \
  || die 2 "le manifeste déclare $FEATURE_CARDS_EXPECTED carte(s) de fonctionnalité — \
l'axe « export de cartes » serait alors vacue des deux côtés (parité vraie qui ne couvre rien) ; \
la fixture doit en porter au moins une"
readonly FEATURE_CARDS_EXPECTED

# ── Binaires ─────────────────────────────────────────────────────────────────

for pair in "admin 2.4.0:$ADMIN_240" "serveur 2.4.0:$SERVER_240" \
            "admin 2.5.0:$ADMIN_250" "serveur 2.5.0:$SERVER_250"; do
  label="${pair%%:*}"; bin="${pair#*:}"
  [[ -f "$bin" ]] || die 2 "binaire $label introuvable : $bin"
  [[ -x "$bin" ]] || die 2 "binaire $label non exécutable : $bin"
  resolved="$(readlink -f -- "$bin")"
  case "$(dirname -- "$resolved")" in
    /bin | /sbin | /usr/bin | /usr/sbin | /usr/local/bin | /usr/local/sbin)
      die 2 "refus d'un binaire installé dans un répertoire système ($resolved) : \
ce harnais ne doit jamais invoquer un binaire du parc" ;;
  esac
done
if [[ -n "$WORKER_250" ]]; then
  [[ -x "$WORKER_250" ]] || die 2 "binaire travailleur 2.5.0 non exécutable : $WORKER_250"
fi

# ── État ─────────────────────────────────────────────────────────────────────
#
# L'état est lu, jamais modifié — sauf par l'axe F-XX (ruling E4) et le mode
# `--fault-inject`, tous deux exécutés APRÈS les quatre relevés comparables.

[[ -d "$STATE" ]] || die 2 "état illisible : $STATE (répertoire absent)"
STATE="$(cd -- "$STATE" && pwd -P)"
readonly SERVER_CONFIG="$STATE/config/server.toml"
for f in "$SERVER_CONFIG" "$STATE/db/queue.sqlite" "$STATE/vault/.gradatum/index.db"; do
  [[ -f "$f" ]] || die 2 "état illisible : $f absent — l'état doit venir de gen-fixture.sh"
done

# L'identité du porteur : la clé bootstrap écrite par `init`, dont le sujet est
# `main-agent` (read_patterns = ["**"]). Aucune clé n'est mintée ici : une clé
# créée modifierait l'état entre deux relevés.
API_KEY_FILE="$STATE/config/main-agent.apikey.txt"
if [[ ! -f "$API_KEY_FILE" ]]; then
  shopt -s nullglob
  candidates=("$STATE"/config/*.apikey.txt)
  shopt -u nullglob
  (( ${#candidates[@]} == 1 )) || die 2 "clé bootstrap introuvable : \
$API_KEY_FILE absent, et ${#candidates[@]} clé(s) *.apikey.txt dans $STATE/config"
  API_KEY_FILE="${candidates[0]}"
fi
API_KEY="$(tr -d '\r\n' <"$API_KEY_FILE")"
[[ -n "$API_KEY" ]] || die 2 "clé bootstrap vide : $API_KEY_FILE"

section_value() {  # $1 = fichier toml, $2 = section, $3 = clé
  sed -n "/^\[$2\]/,/^\[/p" "$1" | sed -n "s/^$3 = \"\(.*\)\"\$/\1/p" | head -n 1
}

CONFIG_BIND="$(section_value "$SERVER_CONFIG" server bind)"
CONFIG_API_PORT="${CONFIG_BIND##*:}"
[[ "$CONFIG_API_PORT" =~ ^[0-9]+$ ]] || die 2 "port d'API illisible dans $SERVER_CONFIG : bind='$CONFIG_BIND'"

# Le port de la configuration fait foi. Un `--port` qui le contredit décrit une
# autre instance que celle de l'état : c'est une non-exécution, nommée.
if [[ -n "$PORT" ]]; then
  [[ "$PORT" =~ ^[0-9]+$ ]] || die 2 "--port doit être un entier (reçu : $PORT)"
  if [[ "$PORT" != "$CONFIG_API_PORT" ]]; then
    die 2 "--port $PORT contredit la configuration de l'état ($CONFIG_BIND) — \
le port de l'état fait foi ; relancer avec --port $CONFIG_API_PORT ou sans --port"
  fi
else
  PORT="$CONFIG_API_PORT"
fi

readonly API_PORT="$PORT"
readonly METRICS_PORT=$((PORT + 1))
readonly INTERNAL_PORT=$((PORT + 2))
readonly WORKER_HEALTH_PORT=$((PORT + 3))
readonly BASE_URL="http://127.0.0.1:$API_PORT"
readonly INTERNAL_URL="http://127.0.0.1:$INTERNAL_PORT"
INTERNAL_TOKEN="$(section_value "$SERVER_CONFIG" internal_api token)"

for p in "$API_PORT" "$METRICS_PORT" "$INTERNAL_PORT" "$WORKER_HEALTH_PORT"; do
  for forbidden in "${FORBIDDEN_PORTS[@]}"; do
    [[ "$p" != "$forbidden" ]] || die 2 "port $p interdit : réservé au parc"
  done
done

# ── Sonde de port par BIND RÉEL, avant toute autre action ────────────────────
#
# `( : >/dev/tcp/...)` est une CONNEXION : elle ne prouve pas qu'un port est
# libre, seulement qu'il n'accepte pas. Le bind, lui, dit si l'écouteur
# nécessaire peut réellement être posé. C'est la sonde du brief (R2-3).

bind_probe() {  # $1 = port — rc 0 si le bind a réussi
  python3 - "$1" <<'PY'
import socket, sys
port = int(sys.argv[1])
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    s.bind(("127.0.0.1", port))
except OSError as e:
    print("bind refusé sur %d : %s" % (port, e), file=sys.stderr)
    sys.exit(1)
finally:
    s.close()
PY
}

for p in "$API_PORT" "$METRICS_PORT" "$INTERNAL_PORT" "$WORKER_HEALTH_PORT"; do
  bind_probe "$p" || die 2 "NON EXÉCUTÉ — port $p déjà en écoute sur 127.0.0.1 \
(sonde par bind, avant toute autre action) ; relancer avec --port <autre>"
done
log "ports sondés par bind et libres : api=$API_PORT métriques=$METRICS_PORT \
interne=$INTERNAL_PORT santé-travailleur=$WORKER_HEALTH_PORT"

# ── Répertoire des mesures ───────────────────────────────────────────────────

if [[ -z "$OUT" ]]; then
  OUT="$(dirname -- "$STATE")/mesures-montee"
fi
mkdir -p -- "$OUT" || die 2 "création impossible : $OUT"
OUT="$(cd -- "$OUT" && pwd -P)"
case "$OUT" in
  "$REPO_ROOT" | "$REPO_ROOT"/*)
    die 2 "le répertoire des mesures est dans le dépôt ($OUT) — \
les mesures ne se committent pas" ;;
esac
readonly OUT

fresh_dir() {  # $1 = sous-répertoire de $OUT — vidé, puis RENDU sur stdout
  case "$1" in
    "$OUT"/*) [[ "$1" != "$OUT" ]] || die 2 "fresh_dir: chemin refusé ($1)" ;;
    *) die 2 "fresh_dir: chemin hors de \$out ($1)" ;;
  esac
  rm -rf -- "$1"
  mkdir -p -- "$1"
  # Le chemin est rendu : les appelants l'écrivent `P1="$(fresh_dir …)"` — un
  # répertoire vide serait pris pour une variable non renseignée, et le relevé
  # tomberait dans `/`.
  printf '%s\n' "$1"
}
run_log() { printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" >>"$OUT/run.log"; }

# `$GRADATUM_UPGRADE_PHASES` : lue par le step de récapitulatif du job CI. Le
# harnais y consigne la durée RÉELLE de chaque phase — c'est la mesure du Step 2
# du brief, et la seule source de ces durées.
PHASES_FILE="${GRADATUM_UPGRADE_PHASES:-$OUT/phases.txt}"
phase_begin() { PHASE_START=$SECONDS; }
phase_end() {  # $1 = nom
  local d=$((SECONDS - PHASE_START))
  printf 'phase=%s duree=%ss\n' "$1" "$d" >>"$PHASES_FILE" 2>/dev/null || true
  log "phase $1 : ${d}s"
  run_log "phase=$1 duree=${d}s"
}

# ── Arrêt des processus — par PID capturé, jamais par motif ──────────────────

stop_pid() {  # $1 = pid, $2 = nom
  local pid="${1:-}" name="${2:-proc}"
  [[ -n "$pid" ]] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  kill -TERM "$pid" 2>/dev/null || true
  local deadline=$((SECONDS + 20))
  while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 1; done
  if kill -0 "$pid" 2>/dev/null; then
    log "$name (pid $pid) ne répond pas au TERM — KILL"
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
  log "$name arrêté (pid $pid)"
}

# `trap` est le seul appelant : l'analyse d'accessibilité de shellcheck ne le
# compte pas comme un appel, d'où l'aveu explicite — la fonction EST atteinte,
# sur EXIT, et c'est tout l'intérêt.
# shellcheck disable=SC2407
cleanup() {
  local rc=$?
  stop_pid "$WORKER_PID" travailleur
  stop_pid "$SERVER_PID" serveur
  return "$rc"
}
trap cleanup EXIT

# ── Démarrage d'un serveur, relevé du démarrage ──────────────────────────────

start_server() {  # $1 = binaire, $2 = répertoire de phase, $3 = étiquette
  local bin="$1" dir="$2" label="$3"
  local log_file="$dir/server.log"
  mkdir -p -- "$dir"
  : >"$dir/startup-rc.txt"
  : >"$log_file"

  SERVER_PID=""
  "$bin" --config "$SERVER_CONFIG" >>"$log_file" 2>&1 &
  SERVER_PID=$!
  local t0=$SECONDS deadline=$((SECONDS + 60)) ready=0 rc=0

  while (( SECONDS < deadline )); do
    if curl -fsS --max-time 2 "$BASE_URL/health" >"$dir/health-first.json" 2>/dev/null; then
      ready=1
      break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      rc=0; wait "$SERVER_PID" 2>/dev/null || rc=$?
      printf 'refus=oui rc=%s motif=processus sorti avant toute reponse sur /health\n' "$rc" \
        >"$dir/startup-rc.txt"
      sed -n '1,50p' "$log_file" >"$dir/server-head-50.txt"
      SERVER_PID=""
      log "$label : REFUS DE DÉMARRAGE (rc=$rc) — consigné, pas contourné"
      return 1
    fi
    sleep 1
  done

  sed -n '1,50p' "$log_file" >"$dir/server-head-50.txt"
  if (( ! ready )); then
    printf 'refus=oui rc=timeout motif=aucune reponse sur /health en 60 s, processus vivant\n' \
      >"$dir/startup-rc.txt"
    stop_pid "$SERVER_PID" "$label"; SERVER_PID=""
    log "$label : REFUS DE DÉMARRAGE (60 s sans réponse) — consigné, pas contourné"
    return 1
  fi

  local el=$((SECONDS - t0))
  local ver sha
  ver="$(jq -r '.version  // empty' "$dir/health-first.json" 2>/dev/null || true)"
  sha="$(jq -r '.build_sha // empty' "$dir/health-first.json" 2>/dev/null || true)"
  printf 'refus=non readiness_s=%s version=%s build_sha=%s\n' "$el" "$ver" "$sha" \
    >"$dir/startup-rc.txt"
  # La provenance est LUE sur /health, jamais déduite d'un chemin.
  printf '%s\n' "$sha" >"$dir/build_sha.txt"
  printf '%s\n' "$ver" >"$dir/version.txt"
  log "$label : démarré en ${el}s — version=$ver build_sha=$sha"
  return 0
}

stop_server() {
  stop_pid "$SERVER_PID" serveur
  SERVER_PID=""
}

# ── Authentification ─────────────────────────────────────────────────────────
#
# L'échange clé→jeton ne crée rien dans l'index : les quatre relevés restent
# comparables. Aucune clé n'est mintée (cela écrirait dans l'état).

exchange() {
  curl -fsS -X POST "$BASE_URL/auth/exchange" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{}' | jq -r '.token // empty'
}

# ── Preset ACL : chemin lu, contrôle, garde du dialecte ─────────────────────
#
# ⚠️ La migration de configuration du preset ACL n'appartient PLUS à ce couple de
# versions. Le champ `sees_personal_classified` a été retiré du schéma en 2.4.0
# (procédure et raison d'être : `docs/UPGRADING-2.3.1-to-2.4.0.md` §« The ACL
# preset field removal »). Un état produit par 2.4.0 porte donc DÉJÀ le dialecte
# cible, et le chemin nominal de ce harnais est « preset déjà conforme ».
#
# Ce qui reste ici, et pourquoi ce n'est pas du code mort :
#
#   1. le CONTRÔLE (2a) est joué quand même, avec le binaire qui monte
#      (`gradatum-admin acl validate --preset`) : il prouve que le preset de
#      l'état parse sous le parseur de la version cible. Un preset qui ne parse
#      pas laisse le serveur DÉMARRER et servir DENY-ALL — toute lecture refusée
#      en 403 — sans qu'aucun démarrage ne rate. C'est le mode d'échec le plus
#      dangereux de ce produit, et il se contrôle hors serveur ;
#
#   2. la GARDE (2b/2c) s'arme si l'état porte encore la clé retirée — c'est-à-
#      dire s'il vient d'une version ANTÉRIEURE à 2.4.0. Le harnais ne suppose
#      alors rien : il MESURE la conséquence du non-réalignement, puis applique
#      la procédure documentée et la contrôle. C'est ce qui permet de pointer ce
#      harnais sur un état hérité sans qu'il rende un vert par vacuité.
#
# Le chemin qui compte pour 2.4.0 → 2.5.0 est le premier ; le second est un
# filet, et il est NOMMÉ comme tel dans les relevés.

# Le chemin du preset est LU dans la configuration de l'état (`[acl] preset_path`),
# jamais supposé : c'est le chemin que le serveur lit réellement au démarrage.
preset_path() {
  local p=""
  p="$(section_value "$STATE/config/server.toml" acl preset_path 2>/dev/null || true)"
  [[ -n "$p" ]] || p="$STATE/config/bearer.toml"
  printf '%s\n' "$p"
}

# Le preset porte-t-il encore la clé retirée ? Sur un état 2.4.0, NON — le
# dialecte cible est déjà celui de la version qui a produit l'état. Un OUI
# désigne un état antérieur à 2.4.0, que la garde 2b/2c prend en charge.
preset_has_removed_key() {  # $1 = preset
  grep -qE '^[[:space:]]*sees_personal_classified[[:space:]]*=' "$1"
}

# Contrôle hors serveur, lecture seule, à travers le parseur du serveur cible :
# `gradatum-admin acl validate --preset <f>` (guide, §« The control command »).
# C'est le signal go/no-go du guide. Fonction POSITIVE : rend le rc du contrôle.
acl_validate() {  # $1 = admin, $2 = preset, $3 = préfixe de sortie -> rc du contrôle
  local rc=0
  "$1" acl validate --preset "$2" >"$3.out" 2>"$3.err" || rc=$?
  printf '%s\n' "$rc" >"$3.rc"
  {
    printf 'preset=%s\nrc=%s\n' "$2" "$rc"
    printf 'stdout=%s\n' "$(tr '\n' ' ' <"$3.out")"
    printf 'stderr=%s\n' "$(tr '\n' ' ' <"$3.err")"
  } >"$3.txt"
  return "$rc"
}

# Réalignement documenté — `docs/UPGRADING-2.3.1-to-2.4.0.md`, §« The ACL preset
# field removal », étapes 1 et 2, appliquées à la lettre :
#   1. la clé sees_personal_classified est RETIRÉE de chaque bloc `[[consumer]]` ;
#   2. pour chaque consommateur qui la portait à `false` — explicitement OU par
#      défaut, le défaut d'avant 2.4.0 étant `false` — les quatre négations de
#      motif sont PRÉPENDues à `read_patterns` ET à `write_patterns`.
# Un consommateur à `true` ne reçoit aucune négation (guide, étape 3).
#
# Cette fonction sert la GARDE, pas le chemin nominal : elle n'est appelée que si
# l'état porte encore la clé retirée, donc s'il est antérieur à 2.4.0.
# Le remplacement n'est pas « au jugé » : le fichier produit est ensuite soumis à
# `acl validate`, et le réalisé est comparé au source en `diff`.
preset_realign() {  # $1 = preset source, $2 = preset destination
  python3 - "$1" "$2" <<'PY'
import re, sys

src, dst = sys.argv[1], sys.argv[2]
NEGATIONS = [
    "!personal-classified",
    "!personal-classified/**",
    "!**/personal-classified",
    "!**/personal-classified/**",
]
lines = open(src, encoding="utf-8").read().split("\n")
out = []
block = None  # None = préambule, avant le premier [[...]]


def flush(block):
    if block is None:
        return
    # La clé portait-elle `true` ? (absente ou `false` => négations, guide étape 2)
    allows_personal_classified = False
    for line in block:
        m = re.match(r"\s*sees_personal_classified\s*=\s*(true|false)\s*$", line)
        if m:
            allows_personal_classified = (m.group(1) == "true")
    for line in block:
        if re.match(r"\s*sees_personal_classified\s*=", line):
            continue  # 1. la clé retirée disparaît
        if re.match(r"^\s*(read_patterns|write_patterns)\s*=\s*\[", line) and "]" not in line:
            # Formulaire multi-ligne : REFUSÉ plutôt que mutilé en silence — le
            # harnais préfère une non-exécution nommée à un preset à moitié migré.
            sys.exit(3)
        m = re.match(r"^(\s*)(read_patterns|write_patterns)\s*=\s*\[(.*)\]\s*$", line)
        if m and not allows_personal_classified:  # 2. négations EN TÊTE
            indent, key, inner = m.group(1), m.group(2), m.group(3).strip()
            head = ", ".join('"%s"' % n for n in NEGATIONS)
            out.append("%s%s = [%s]" % (indent, key, head + (", " + inner if inner else "")))
        else:
            out.append(line)


for line in lines:
    if line.strip().startswith("[["):
        flush(block)
        block = [line]
    elif block is None:
        out.append(line)
    else:
        block.append(line)
flush(block)
open(dst, "w", encoding="utf-8").write("\n".join(out))
PY
}

# Marqueurs de dégradation du preset, lus DANS LE JOURNAL du serveur — jamais
# déduits d'autre chose. C'est ce qui distingue « la montée a démarré » de « la
# montée a démarré ET sert DENY-ALL ».
preset_degrade_markers() {  # $1 = journal
  grep -E 'DENY-ALL|ACL preset unreadable|no consumer of the ACL preset|without declared identity' \
    "$1" 2>/dev/null || true
}

# Une lecture, avec son CODE HTTP. Le corps d'un refus d'autorisation est vide :
# c'est donc le code qui porte le fait, et il est consigné tel quel.
read_probe() {  # $1 = préfixe de sortie, $2 = jeton -> code HTTP sur stdout
  local code
  code="$(curl -sS --max-time 15 -o "$1.body" -w '%{http_code}' \
    -X POST "$BASE_URL/api/v1/vault_list" \
    -H "Authorization: Bearer $2" -H 'Content-Type: application/json' \
    -d '{"tenant_id":"main","limit":5}' 2>"$1.curlerr" || echo 000)"
  printf '%s\n' "$code" >"$1.code"
  printf 'http=%s corps=%s\n' "$code" \
    "$(head -c 300 "$1.body" 2>/dev/null | tr '\n' ' ')" >"$1.txt"
  printf '%s\n' "$code"
}

# ── Relevés ──────────────────────────────────────────────────────────────────

# Les 5 requêtes littérales sont VERSIONNÉES ICI, pas dérivées du corpus : un
# jeton-repère unique par note, pris dans 5 sections distinctes. Une requête dont
# le résultat serait vide des deux côtés ne prouverait rien — `assert-parity.sh`
# le dit au lieu de compter la parité comme acquise.
readonly -a SEARCH_QUERIES=(
  zenith      # decisions       (01)
  granite     # architecture    (07)
  azurite     # debug           (15)
  olivine     # lessons-learned (28)
  peridot     # retrospectives  (29)
)

api_post() {  # $1 = chemin, $2 = corps json, $3 = jeton -> corps de réponse
  curl -fsS --max-time 20 -X POST "$BASE_URL$1" \
    -H "Authorization: Bearer $3" \
    -H "Content-Type: application/json" \
    -d "$2"
}

capture_vault_list() {  # $1 = dir, $2 = jeton
  local dir="$1" jwt="$2" cursor="" page=0 body pages
  # Les pages sont accumulées dans un fichier (une entrée-tableau JSON compact par
  # ligne) puis fusionnées par `jq -s 'add'` : le corps cumulé ne transite JAMAIS en
  # argument de ligne de commande. Le `jq -n --argjson a "$merged" …` d'avant passait
  # tout le cumul en argument et dépassait ARG_MAX sur un corpus réel (~14 800 notes).
  pages="$(mktemp "$dir/vault_list.pages.XXXXXX")"
  while :; do
    body="$(api_post /api/v1/vault_list \
      "$(jq -n --arg c "$cursor" '{tenant_id:"main", limit:200} + (if $c == "" then {} else {cursor:$c} end)')" \
      "$jwt")" || { bad "vault_list injoignable (page $page)"; rm -f "$pages"; return 1; }
    jq -c '.entries // []' <<<"$body" >>"$pages"
    cursor="$(jq -r '.next_cursor // empty' <<<"$body")"
    page=$((page + 1))
    [[ -n "$cursor" ]] || break
    (( page < 50 )) || { bad "vault_list : pagination non terminée après 50 pages"; rm -f "$pages"; return 1; }
  done
  jq -s 'add // []' "$pages" >"$dir/vault_list.json"
  rm -f "$pages"
  jq -r '.[].path' "$dir/vault_list.json" >"$dir/vault_list.paths.txt"
  sort -- "$dir/vault_list.paths.txt" >"$dir/vault_list.sorted.txt"
  printf '%s\n' "$(jq -r 'length' "$dir/vault_list.json")" >"$dir/vault_list.count"
}

capture_searches() {  # $1 = dir, $2 = jeton
  local dir="$1" jwt="$2" i=0 q body
  : >"$dir/queries.txt"
  for q in "${SEARCH_QUERIES[@]}"; do
    i=$((i + 1))
    printf '%s\n' "$q" >>"$dir/queries.txt"
    body="$(api_post /api/v1/vault_search "$(jq -n --arg q "$q" '{query:$q, limit:50, include_scores:false}')" "$jwt")" \
      || { bad "vault_search injoignable (requête '$q')"; return 1; }
    printf '%s' "$body" >"$dir/search-$i.json"
    # Lu DEPUIS LE FICHIER : passer le corps à jq en argument le ferait
    # interpréter comme un nom de fichier. Les trois formes de clé sont
    # tolérées (`items` est celle que rend cette version de l'API).
    jq -r '[(.results // .hits // .items // [])[] | .path] | .[]' \
      "$dir/search-$i.json" >"$dir/search-$i.paths.txt"
  done
}

# Ce verdict-ci n'est porté ni par le retour de la fonction, ni par `RUN_RC` sur
# le moment : un échec est compté dans `EXPORT_ECHECS`, et c'est le verdict FINAL
# qui le replie dans le code de retour. Le poser dans `RUN_RC` dès le Step 1
# aurait sauté la montée (2d est gardé par `RUN_RC == 0`) : un export vide aurait
# fait disparaître les trois autres relevés, en masquant ce que le harnais est là
# pour mesurer.
capture_export() {  # $1 = dir, $2 = admin
  local dir="$1" admin="$2" rc=0 n="indisponible"
  : >"$dir/export-features.err"
  "$admin" project-map export-features --root "$STATE" --vault main --include-dropped \
    >"$dir/export-features.json" 2>"$dir/export-features.err" || rc=$?
  printf '%s\n' "$rc" >"$dir/export-features.rc"
  # Les diagnostics de dérivation vont sur stderr : ils sont conservés, jamais
  # fondus dans la sortie comparée octet à octet.
  if (( rc == 0 )); then
    n="$(jq 'length' "$dir/export-features.json" 2>/dev/null || printf 'illisible')"
  fi
  printf '%s\n' "$n" >"$dir/export-features.count"
  EXPORT_RELEVES=$((EXPORT_RELEVES + 1))

  # NON-VACUITÉ — l'axe « export de cartes » compare deux relevés octet à octet.
  # Deux exports VIDES sont identiques : la comparaison serait satisfaite sans
  # rien couvrir. Le seuil est LU dans le manifeste de la fixture (>= au nombre
  # de cartes de fonctionnalité qu'elle a créées), jamais postulé. Un export vide
  # des deux côtés est donc un ÉCHEC, jamais une parité.
  if (( rc != 0 )); then
    bad "export-features a rendu rc=$rc (voir $dir/export-features.err)"
  elif [[ ! "$n" =~ ^[0-9]+$ ]]; then
    bad "export-features illisible en JSON (count='$n' — voir $dir/export-features.err)"
  elif (( n < FEATURE_CARDS_EXPECTED )); then
    bad "export-features rend $n carte(s) pour $FEATURE_CARDS_EXPECTED attendue(s) par le \
manifeste — la comparaison octet à octet porterait sur un export vide ou tronqué : \
parité vacue, jamais un vert"
  else
    log "  export-features : $n carte(s) (≥ $FEATURE_CARDS_EXPECTED attendue(s))"
    return 0
  fi
  EXPORT_ECHECS=$((EXPORT_ECHECS + 1))
  return 0
}

# Comptes de tables, migrations, digest de file : lus en LECTURE SEULE, jamais
# par `sqlite3` en écriture — un relevé ne modifie pas ce qu'il mesure.
capture_db() {  # $1 = dir
  python3 - "$STATE" "$1" <<'PY'
import json, os, sqlite3, sys

state, out = sys.argv[1], sys.argv[2]
DBS = [("index.db", os.path.join(state, "vault", ".gradatum", "index.db")),
       ("queue.sqlite", os.path.join(state, "db", "queue.sqlite"))]

def ro(path):
    return sqlite3.connect("file:%s?mode=ro" % path, uri=True)

counts = {}
for label, path in DBS:
    c = ro(path)
    counts[label] = {}
    for (t,) in c.execute(
            "select name from sqlite_master where type='table' order by name"):
        try:
            counts[label][t] = c.execute('select count(*) from "%s"' % t).fetchone()[0]
        except sqlite3.Error as e:
            counts[label][t] = "ILLISIBLE:%s" % e
    c.close()
with open(os.path.join(out, "counts.json"), "w") as f:
    json.dump(counts, f, indent=2, sort_keys=True)

# _sqlx_migrations : version, description, installed_on, checksum(hex), success,
# execution_time. `installed_on` est comparé : une migration RÉAPPLIQUÉE le
# change, là où une comparaison réduite à (version, checksum, success) la
# laisserait passer pour identique.
q = ro(os.path.join(state, "db", "queue.sqlite"))
with open(os.path.join(out, "sqlx_migrations.tsv"), "w") as f:
    f.write("version\tdescription\tinstalled_on\tchecksum\tsuccess\texecution_time\n")
    for r in q.execute("select version, description, installed_on, checksum, success, "
                       "execution_time from _sqlx_migrations order by version"):
        ver, desc, inst, cks, ok_, etime = r
        f.write("%s\t%s\t%s\t%s\t%s\t%s\n"
                % (ver, desc, inst, cks.hex() if isinstance(cks, (bytes, bytearray)) else cks,
                   ok_, etime))
    jobs = list(q.execute("select id, status, kind from gradatum_jobs order by id"))
    by = list(q.execute("select status, count(*) from gradatum_jobs "
                        "group by status order by status"))
q.close()
with open(os.path.join(out, "jobs.digest.txt"), "w") as f:
    for jid, status, kind in jobs:
        f.write("%s\t%s\t%s\n" % (jid, status, kind))
with open(os.path.join(out, "jobs_by_status.txt"), "w") as f:
    for status, n in by:
        f.write("%s=%s\n" % (status, n))

i = ro(os.path.join(state, "vault", ".gradatum", "index.db"))
with open(os.path.join(out, "schema_migrations.tsv"), "w") as f:
    f.write("version\tapplied_at\n")
    for r in i.execute("select version, applied_at from _schema_migrations order by version"):
        f.write("%s\t%s\n" % r)
i.close()
PY
}

# `/health` : le plancher de 60 s est un PLANCHER, pas une attente aveugle — si
# `healthy` arrive avant, on le consigne et on cesse d'attendre (attendre après
# la réponse ne prouverait rien de plus). S'il n'arrive pas, le temps réel est
# consigné comme « jamais » : c'est un RÉSULTAT mesuré, pas un échec du harnais.
health_watch() {  # $1 = dir, $2 = plancher en secondes
  local dir="$1" floor="$2" t0=$SECONDS prev="" el=0 body st first="never"
  : >"$dir/health-timeline.txt"
  while (( el < 120 )); do
    if body="$(curl -fsS --max-time 3 "$BASE_URL/health" 2>/dev/null)"; then
      st="$(jq -r '.status // "?"' <<<"$body")"
    else
      st="injoignable"
    fi
    el=$((SECONDS - t0))
    if [[ "$st" != "$prev" ]]; then
      printf 't=+%ss status=%s\n' "$el" "$st" >>"$dir/health-timeline.txt"
      prev="$st"
    fi
    if [[ "$st" == "healthy" && "$first" == "never" ]]; then
      first="$el"
      break
    fi
    (( el >= floor )) && break
    sleep 2
  done
  printf '%s\n' "$first" >"$dir/first-healthy-secs.txt"
  curl -fsS --max-time 3 "$BASE_URL/health" >"$dir/health.json" 2>/dev/null || : >"$dir/health.json"
  curl -fsS --max-time 3 "http://127.0.0.1:$METRICS_PORT/health/detailed" \
    >"$dir/health-detailed.json" 2>/dev/null || : >"$dir/health-detailed.json"
  # `first` vaut un nombre de secondes OU le mot `never` : les deux s'affichent
  # tels quels, sans accoler d'unité à un mot.
  if [[ "$first" == "never" ]]; then
    log "  /health : premier « healthy » = JAMAIS (plancher ${floor}s, \
observé $((SECONDS - t0))s ; statuts : $(tr '\n' ' ' <"$dir/health-timeline.txt"))"
  else
    log "  /health : premier « healthy » = ${first}s (plancher ${floor}s, \
observé $((SECONDS - t0))s ; statuts : $(tr '\n' ' ' <"$dir/health-timeline.txt"))"
  fi
}

# ── Un relevé complet ────────────────────────────────────────────────────────

measure() {  # $1 = dir, $2 = admin, $3 = jeton, $4 = plancher /health (ou "" )
  local dir="$1" admin="$2" jwt="$3" floor="${4:-}"
  capture_vault_list "$dir" "$jwt" || return 1
  capture_searches   "$dir" "$jwt" || return 1
  capture_export     "$dir" "$admin"
  capture_db         "$dir"
  if [[ -n "$floor" ]]; then
    health_watch "$dir" "$floor"
  else
    curl -fsS --max-time 3 "$BASE_URL/health" >"$dir/health.json" 2>/dev/null \
      || : >"$dir/health.json"
  fi
  log "  relevé $dir : $(wc -l <"$dir/vault_list.paths.txt") note(s) listée(s), \
$(jq -r '.["index.db"].notes' "$dir/counts.json") note(s) en base, \
$(jq -r '.["queue.sqlite"].gradatum_jobs' "$dir/counts.json") travail(aux) en file"
}

# ── Écriture du verdict ──────────────────────────────────────────────────────

verdict_write() {  # $1 = code, $2 = texte
  printf '%s\n' "$2" >"$OUT/verdict.txt"
  printf 'rc=%s %s\n' "$1" "$2" >>"$OUT/verdict.txt"
  log "VERDICT rc=$1 — $2"
}

# ═════════════════════════════════════════════════════════════════════════════
#  Exécution
# ═════════════════════════════════════════════════════════════════════════════

mkdir -p -- "$OUT"
run_log "début — état=$STATE port=$API_PORT out=$OUT fault_inject=$FAULT_INJECT"
log "état : $STATE"
log "mesures : $OUT"

# ── P0 : provenance des binaires ─────────────────────────────────────────────
phase_begin
{
  printf '{"state":"%s","port":%s,"api_port":%s,"metrics_port":%s,"internal_port":%s,' \
    "$STATE" "$API_PORT" "$API_PORT" "$METRICS_PORT" "$INTERNAL_PORT"
  printf '"worker_health_port":%s,"fault_inject":"%s","started_utc":"%s",' \
    "$WORKER_HEALTH_PORT" "$FAULT_INJECT" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '"binaries":{'
  first=1
  for pair in "admin-240:$ADMIN_240" "server-240:$SERVER_240" \
              "admin-250:$ADMIN_250" "server-250:$SERVER_250"; do
    (( first )) || printf ','
    first=0
    printf '"%s":{"path":"%s","sha256":"%s"}' "${pair%%:*}" "${pair#*:}" \
      "$(sha256sum <"${pair#*:}" | cut -d' ' -f1)"
  done
  if [[ -n "$WORKER_250" ]]; then
    printf ',"worker-250":{"path":"%s","sha256":"%s"}' "$WORKER_250" \
      "$(sha256sum <"$WORKER_250" | cut -d' ' -f1)"
  fi
  printf '}}\n'
} >"$OUT/meta.json"
phase_end "p0-metadonnees"

# ── Step 1 : mesure AVANT, binaire 2.4.0 ─────────────────────────────────────
phase_begin
P1="$(fresh_dir "$OUT/1-avant-240")"
log "Step 1 — mesure AVANT (2.4.0)"
start_server "$SERVER_240" "$P1" "serveur 2.4.0 (AVANT)" \
  || die 2 "le serveur 2.4.0 n'a pas démarré sur l'état — voir $P1/server.log et $P1/startup-rc.txt"
JWT_240="$(exchange)" || { stop_server; die 2 "échange clé→jeton refusé par le serveur 2.4.0"; }
[[ -n "$JWT_240" ]] || { stop_server; die 2 "jeton vide rendu par /auth/exchange (2.4.0)"; }
measure "$P1" "$ADMIN_240" "$JWT_240" "" \
  || { stop_server; die 2 "relevé AVANT incomplet — voir $OUT/run.log"; }
stop_server
phase_end "step1-avant-240"

# Injection de faute (diagnostic seulement) : entre les deux mesures.
if [[ "$FAULT_INJECT" == "delete-note" ]]; then
  phase_begin
  python3 - "$STATE" "$P1" "$OUT/fault-injection.txt" <<'PY'
import os, sqlite3, sys
state, ref, out = sys.argv[1], sys.argv[2], sys.argv[3]
# La note retirée est choisie DÉTERMINISTEMENT : la dernière du listing trié du
# relevé AVANT. Le choix ne dépend pas de ce que le harnais « sait » déjà.
paths = [l.strip() for l in open(os.path.join(ref, "vault_list.sorted.txt")) if l.strip()]
if not paths:
    print("aucune note à retirer", file=sys.stderr); sys.exit(1)
target = paths[-1]
section, _, ulid = target.partition("/")
md = os.path.join(state, "vault", "main", ulid + ".md")
removed_md = False
if os.path.exists(md):
    os.remove(md); removed_md = True
c = sqlite3.connect(os.path.join(state, "vault", ".gradatum", "index.db"))
row = c.execute("select id, section from notes where id = ?", (ulid,)).fetchone()
c.execute("delete from notes where id = ?", (ulid,))
c.execute("delete from note_index where note_id = ?", (ulid,))
c.commit(); c.close()
with open(out, "w") as f:
    f.write("cible=%s md_supprime=%s ligne_notes_supprimee=%s (avant=%s)\n"
            % (target, removed_md, row is not None, row))
print("injection: %s" % target)
PY
  rc_inject=$?
  printf 'fault-inject=delete-note rc=%s\n' "$rc_inject" >>"$OUT/fault-injection.txt"
  (( rc_inject == 0 )) || die 2 "injection de faute impossible (rc=$rc_inject)"
  log "injection de faute : $(cat "$OUT/fault-injection.txt")"
  phase_end "step1bis-injection"
fi

# ── Step 2 : montée 2.5.0 — contrôle du preset AVANT de monter ───────────────
#
# Quatre temps NOMMÉS parce que ce sont quatre faits distincts :
#   2a  contrôle du preset de l'état, avec le binaire 2.5.0 — parse-t-il sous le
#       parseur de la version cible ? Un preset qui ne parse pas laisse le
#       serveur démarrer et servir DENY-ALL sans qu'aucun démarrage ne rate ;
#   2b  GARDE seulement — montée sur un preset portant encore la clé retirée
#       (état antérieur à 2.4.0) : le DENY-ALL est mesuré, ni vert ni divergence ;
#   2c  GARDE seulement — réalignement documenté du preset, puis contrôle ;
#   2d  montée sur le preset de l'état → c'est l'état que le Step 3 mesure.
#
# Sur le couple 2.4.0 → 2.5.0, le preset de l'état est DÉJÀ au dialecte cible :
# 2b et 2c ne s'arment pas, 2a et 2d font tout le travail. C'est un chemin
# explicite et consigné (`preset-deja-conforme`), pas une absence silencieuse.
phase_begin
P2H="$(fresh_dir "$OUT/2-montee-preset-herite")"
P2C="$(fresh_dir "$OUT/2-controle-preset")"
P2="$(fresh_dir "$OUT/2-montee-250")"
P3="$(fresh_dir "$OUT/3-apres-250")"
MOUNT_OK=0
PRESET="$(preset_path)"
printf '%s\n' "$PRESET" >"$P2C/preset-path.txt"
log "Step 2 — montée 2.5.0 sur le même répertoire, même port (preset ACL : $PRESET)"

if [[ ! -f "$PRESET" ]]; then
  die 2 "preset ACL introuvable : $PRESET (lu depuis [acl] preset_path de l'état)"
fi

# 2a — contrôle pré-déploiement. Le contrôle se fait avec le binaire qui MONTE,
# parce que c'est SON parseur qui décidera : le preset doit charger sur la
# version cible, pas seulement sur celle qui l'a produit.
rc_va=0
ACL_VALIDATE_OK=1
acl_validate "$ADMIN_250" "$PRESET" "$P2C/validate-herite" || rc_va=$?
if (( rc_va == 0 )); then
  log "  2a contrôle du preset de l'état : VALIDE (rc=0) — $(cat "$P2C/validate-herite.out")"
elif grep -qi 'unrecognized subcommand' "$P2C/validate-herite.err" 2>/dev/null; then
  # La sous-commande de contrôle est entrée dans le produit en 2.4.0 : elle est
  # donc présente sur les deux binaires de ce couple. Ce cas ne se présente que
  # si l'appelant fournit un admin ANTÉRIEUR (rejeu sur un état plus ancien), et
  # il se lit alors comme une LIMITE DE CAPACITÉ du binaire fourni, jamais comme
  # un preset valide ou refusé. Elle ne fait pas échouer la course — mais elle
  # est consignée, et le filet reste le journal de 2d, qui refuse toute montée
  # dont le preset n'a pas chargé.
  ACL_VALIDATE_OK=0
  printf 'controle=INDISPONIBLE — ce gradatum-admin ne porte pas la sous-commande « acl validate »\n' \
    >>"$P2C/validate-herite.txt"
  log "  2a contrôle du preset de l'état : INDISPONIBLE — « acl validate » absente de ce gradatum-admin"
else
  log "  2a contrôle du preset de l'état : REFUSÉ (rc=$rc_va) — le signal go/no-go du guide est ROUGE"
  log "     $(cat "$P2C/validate-herite.err")"
fi

if preset_has_removed_key "$PRESET"; then
  PRESET_CHEMIN="garde (preset hors dialecte cible : la clé retirée est encore là)"
  DENY_ALL_MESURE=0
  cp -- "$PRESET" "$P2C/preset-herite.toml"

  # 2b — la montée telle qu'un opérateur la ferait SANS lire le guide : le preset
  # hérité est laissé en place. Le serveur démarre (c'est le fait dangereux), et
  # toute lecture est refusée. Relevé et nommé, jamais compté comme un vert.
  log "  2b GARDE — montée sur preset PORTANT ENCORE LA CLÉ RETIRÉE (état antérieur à 2.4.0) ; la conséquence du non-réalignement, mesurée"
  if start_server "$SERVER_250" "$P2H" "serveur 2.5.0 (preset HÉRITÉ)"; then
    jwt_h="$(exchange || true)"
    code_h="$(read_probe "$P2H/lecture" "$jwt_h")"
    preset_degrade_markers "$P2H/server.log" >"$P2H/journal-marqueurs.txt"
    DENY_ALL_MESURE=0
    {
      printf 'preset=preset ANTÉRIEUR à 2.4.0 — porte encore la clé retirée sees_personal_classified\n'
      printf 'controle_acl_validate=rc %s (attendu non nul : le guide l'\''annonce)\n' "$rc_va"
      printf 'demarrage=le serveur DÉMARRE (refus=non) — c'\''est le fait dangereux, pas le contraire\n'
      printf 'lecture_http=%s\n' "$code_h"
      printf 'issu_du_journal=%s\n' "$P2H/journal-marqueurs.txt"
      printf 'influence_rc=aucune — c'\''est un RÉSULTAT consigné, ni un vert ni une divergence de données\n'
      printf 'remède=guide §« The ACL preset field removal (F-297) », étapes 1-2 — appliqué en 2c\n'
    } >"$P2H/conséquence-non-réalignement.txt"
    # Le verdict du 2b est DÉRIVÉ des faits mesurés (marqueurs du journal ET code
    # de lecture), jamais de l'hypothèse « ce doit être un 403 » : une montée
    # sans dégradation (binaire identique des deux côtés, par exemple) doit
    # pouvoir le dire, et une dégradation qui ne refuse pas la lecture est un
    # ÉCART qui doit se voir.
    if [[ -s "$P2H/journal-marqueurs.txt" ]]; then
      if [[ "$code_h" == "403" ]]; then
        DENY_ALL_MESURE=1
        printf 'verdict=DÉGRADATION CONFIRMÉE — moteur vide (DENY-ALL) : toute lecture est refusée\n' \
          >>"$P2H/conséquence-non-réalignement.txt"
        log "     montée sur preset hérité : le serveur DÉMARRE et sert DENY-ALL (lecture HTTP $code_h) — consigné en 2-montee-preset-herite/"
      else
        printf 'verdict=ÉCART — le journal annonce la dégradation mais la lecture rend HTTP %s\n' "$code_h" \
          >>"$P2H/conséquence-non-réalignement.txt"
        RUN_RC=1
        bad "2b : le journal annonce DENY-ALL mais la lecture rend HTTP $code_h — écart nommé"
      fi
    elif [[ "$code_h" == "200" ]]; then
      printf 'verdict=NON MESURÉ — aucune dégradation : ce binaire ACCEPTE le preset hérité tel quel.\n' \
        >>"$P2H/conséquence-non-réalignement.txt"
      printf 'consequence_du_non_realignement=non reproductible sur cette configuration de binaires\n' \
        >>"$P2H/conséquence-non-réalignement.txt"
      log "     montée sur preset hérité : aucune dégradation (lecture HTTP $code_h) — la conséquence du non-réalignement n'est pas reproductible ici"
    else
      printf 'verdict=ÉCART — aucune dégradation au journal, mais la lecture rend HTTP %s\n' "$code_h" \
        >>"$P2H/conséquence-non-réalignement.txt"
      RUN_RC=1
      bad "2b : aucune dégradation au journal mais lecture HTTP $code_h — écart nommé"
    fi
  else
    DENY_ALL_MESURE=0
    printf 'NON MESURÉ — le serveur 2.5.0 n'\''a pas démarré sur le preset hérité (%s).\n' \
      "$(cat "$P2H/startup-rc.txt" 2>/dev/null || echo 'motif indisponible')" \
      >"$P2H/conséquence-non-réalignement.txt"
    log "  2b NON MESURÉ — le serveur 2.5.0 n'a pas démarré sur le preset hérité"
  fi
  stop_server

  # 2c — réalignement documenté, puis contrôle du preset réaligné.
  log "  2c réalignement documenté du preset (guide §F-297, étapes 1-2)"
  rc_re=0
  preset_realign "$P2C/preset-herite.toml" "$P2C/preset-realigne.toml" || rc_re=$?
  if (( rc_re != 0 )); then
    # INCAPACITÉ DE L'OUTILLAGE, pas une divergence : `preset_realign` est une
    # transformation pure (python3), elle n'inspecte aucun des deux états
    # mesurés. Un refus ici (formulaire `read_patterns`/`write_patterns`
    # multi-ligne, python3 défaillant) veut dire que la séquence n'a pas pu être
    # exécutée — donc `2`, JAMAIS `1`. Un `1` ferait lire « état divergent » là
    # où rien n'a divergé ; l'effet sur le gate est le même (tout code non nul
    # halte) mais le message doit décrire le fait.
    die 2 "réalignement du preset REFUSÉ (rc=$rc_re) — formulaire de preset non \
pris en charge ; le preset de l'état n'a PAS été modifié ($P2C/preset-herite.toml) : \
la montée n'a pas été tentée, aucune mesure de parité n'a été prise"
  fi
  diff -u -- "$P2C/preset-herite.toml" "$P2C/preset-realigne.toml" \
    >"$P2C/realignement.diff" 2>&1 || true
  printf 'négations_préfixées=4 par liste ; clé retirée : sees_personal_classified\n' \
    >"$P2C/realignement.txt"
  # ÉCRITURE ASSUMÉE dans l'état : le réalignement EST la migration de
  # configuration que la montée exige. Elle ne touche aucune des quatre
  # surfaces mesurées (index, file, vault, export) — seul `config/bearer.toml`
  # change, et l'original est conservé octet à octet ci-dessus.
  cp -- "$P2C/preset-realigne.toml" "$PRESET"
  printf 'preset_reecrit=%s\noriginal_conserve=%s\n' "$PRESET" "$P2C/preset-herite.toml" \
    >>"$P2C/realignement.txt"
  if (( ACL_VALIDATE_OK )); then
    rc_vr=0
    acl_validate "$ADMIN_250" "$PRESET" "$P2C/validate-realigne" || rc_vr=$?
    if (( rc_vr == 0 )); then
      log "     contrôle du preset RÉALIGNÉ : VALIDE (rc=0) — $(cat "$P2C/validate-realigne.out")"
    else
      RUN_RC=1
      bad "contrôle du preset RÉALIGNÉ : REFUSÉ (rc=$rc_vr) — $(cat "$P2C/validate-realigne.err")"
    fi
  else
    {
      printf 'controle=NON REJOUÉ — la sous-commande « acl validate » est absente de ce gradatum-admin\n'
      printf 'verdict=NON MESURÉ — le réalignement n a pas pu être contrôlé hors serveur.\n'
      printf 'filet=le journal de la montée 2d est analysé : tout marqueur de dégradation du preset\n'
      printf 'y fait échouer la course (RUN_RC=1). Le contrôle hors serveur n est pas le seul filet.\n'
    } >"$P2C/validate-realigne.txt"
    log "     contrôle du preset RÉALIGNÉ : NON MESURÉ (sous-commande absente de ce binaire)"
  fi
else
  PRESET_CHEMIN="deja-conforme (chemin nominal de ce couple)"
  DENY_ALL_MESURE=0
  {
    printf 'preset=celui de l état, déjà conforme au dialecte de la version cible\n'
    printf 'fait=aucune occurrence de la clé retirée (retrait consommé en 2.4.0 ; la clé est\n'
    printf 'sortie du schéma des entrées de consommateur, pas de celui de la version qui monte)\n'
    printf 'NON MESURÉ — la conséquence du non-réalignement n est pas mesurable sur cet état :\n'
    printf 'il n y a rien à réaligner. Le harnais ne simule pas ce cas.\n'
    printf 'filet=le garde 2b/2c reste armé et s armerait sur un état antérieur à 2.4.0\n'
  } >"$P2C/preset-deja-conforme.txt"
  log "  2a-2c : preset de l'état déjà conforme (rc=0) — 2b non mesuré, 2c sans objet (chemin déjà-conforme consigné)"
fi

# 2d — la montée sur le preset de l'état (déjà conforme ici, réaligné si 2c s'est
# armé) : c'est l'état que le Step 3 mesure.
if [[ "$RUN_RC" == "0" ]]; then
  if start_server "$SERVER_250" "$P2" "serveur 2.5.0 (MONTÉE)"; then
    MOUNT_OK=1
    preset_degrade_markers "$P2/server.log" >"$P2/journal-marqueurs.txt"
    if [[ -s "$P2/journal-marqueurs.txt" ]]; then
      RUN_RC=1
      bad "MONTÉE 2.5.0 : le journal porte encore un marqueur de dégradation du preset — \
le réalignement n'a pas suffi. Voir $P2/journal-marqueurs.txt"
    fi
  else
    # Un refus de démarrage est un RÉSULTAT : il est nommé, écrit, et ne masque
    # pas la suite — le retour arrière reste exercé juste après.
    bad "MONTÉE 2.5.0 : refus de démarrage — $(cat "$P2/startup-rc.txt")"
    RUN_RC=1
    cp -- "$P2/startup-rc.txt" "$P3/startup-rc.txt" 2>/dev/null || true
  fi
else
  printf 'NON EXÉCUTÉ — le réalignement du preset a échoué : la montée sur le preset\n\nde l'\''état n'\''a pas été tentée (aucun état non conforme n'\''a été monté en 2.5.0).\n' \
    >"$P2/startup-rc.txt"
  bad "MONTÉE 2.5.0 sur le preset de l'état : NON TENTÉE (le réalignement a échoué)"
fi
phase_end "step2-montee-250"

# ── Step 3 : mesure APRÈS + /health (plancher 60 s) ──────────────────────────
phase_begin
if (( MOUNT_OK )); then
  log "Step 3 — mesure APRÈS (2.5.0), /health avec plancher de 60 s"
  JWT_250="$(exchange)" || { RUN_RC=1; bad "échange clé→jeton refusé par le serveur 2.5.0"; JWT_250=""; }
  if [[ -n "$JWT_250" ]]; then
    measure "$P3" "$ADMIN_250" "$JWT_250" 60 \
      || { RUN_RC=1; bad "relevé APRÈS incomplet — voir $OUT/run.log"; }
    cp -- "$P2/build_sha.txt" "$P3/build_sha.txt" 2>/dev/null || true
    cp -- "$P2/version.txt"   "$P3/version.txt"   2>/dev/null || true
    cp -- "$P2/server.log"    "$P3/server.log"    2>/dev/null || true
    cp -- "$P2/server-head-50.txt" "$P3/server-head-50.txt" 2>/dev/null || true
  else
    bad "mesure APRÈS impossible : pas de jeton sur le serveur MONTÉ"
  fi
else
  log "Step 3 — NON MESURÉ : la montée 2.5.0 a refusé de démarrer (résultat consigné)"
fi
stop_server
phase_end "step3-apres-250"

# ── Step 3bis : retour arrière — redémarrage 2.4.0 sur le répertoire monté ───
phase_begin
P4="$(fresh_dir "$OUT/4-retour-240")"
log "Step 3bis — retour arrière : redémarrage 2.4.0 sur le répertoire MONTÉ"
ROLLBACK_OK=0
if start_server "$SERVER_240" "$P4" "serveur 2.4.0 (RETOUR ARRIÈRE)"; then
  JWT_RB="$(exchange)" || { RUN_RC=1; bad "échange clé→jeton refusé après retour arrière"; JWT_RB=""; }
  if [[ -n "$JWT_RB" ]]; then
    measure "$P4" "$ADMIN_240" "$JWT_RB" "" \
      || { RUN_RC=1; bad "quatrième mesure incomplète — voir $OUT/run.log"; }
    ROLLBACK_OK=1
  fi
else
  RUN_RC=1
  bad "RETOUR ARRIÈRE : refus de démarrage du 2.4.0 sur le répertoire monté — \
$(cat "$P4/startup-rc.txt")"
fi
stop_server

# 3bis-0 — RÉVERSIBILITÉ DU PRESET DE L'ÉTAT. Le retour arrière ci-dessus a tourné
# sur l'état MONTÉ, dont le preset est celui de l'état (déjà conforme au dialecte
# cible ; réécrit en 2c SEULEMENT si le garde s'est armé — état antérieur à 2.4.0).
# La question opérationnelle est donc : le retour arrière du BINAIRE exige-t-il
# aussi un retour arrière de la CONFIGURATION ? Elle est tranchée par la mesure,
# pas par une intuition.
{
  if (( ROLLBACK_OK )); then
    printf 'mesure=le 2.4.0 démarre ET lit le vault sur le preset de l état monté (%s note(s) listée(s))\n' \
      "$(wc -l <"$P4/vault_list.paths.txt" 2>/dev/null || echo 0)"
    printf 'conclusion=le preset de l état est compatible 2.4.0 : le retour arrière du binaire\n'
    printf 'ne requiert PAS de retour arrière de la configuration. Une seule voie de retour.\n'
    printf 'raison (source, non mesurée) : en 2.4.0 sees_personal_classified porte #[serde(default)],\n'
    printf 'et la négation ! y existe déjà (décision DenyExplicit, deny-wins).\n'
  else
    printf 'mesure=le 2.4.0 N'\''A PAS redémarré sur le preset de l'\''état (%s)\n' \
      "$(cat "$P4/startup-rc.txt" 2>/dev/null || echo 'motif indisponible')"
    printf 'conclusion=ATTENTION : le retour arrière du binaire seul NE SUFFIT PAS sur cet état —\n'
    printf 'le retour arrière doit aussi restaurer la configuration (preset de 2.4.0).\n'
    printf 'configuration_de_2.4.0_conservee=%s\n' "$P2C/preset-herite.toml"
  fi
} >"$OUT/3bis-reversibilite-preset.txt"
log "Step 3bis-0 réversibilité : $(head -1 "$OUT/3bis-reversibilite-preset.txt")"
phase_end "step3bis-retour-240"

# ── Cas nommé `PrivacyRescan` (F-160) — POINT D'EXTENSION, ruling E6 ──────────
#
# Le brief (Step 3bis) demande d'enfiler, depuis 2.5.0, un `Job` de la variante
# introduite par F-160 (`PrivacyRescan`), puis de redémarrer 2.4.0 et d'observer.
#
# ⚠ CETTE VARIANTE N'EXISTE PAS DANS CET ARBRE. Le ruling E6 en fait le
# périmètre de la Task 2bis, après fusion de F-160, et interdit de la SIMULER.
# La fonction ci-dessous est donc un POINT D'EXTENSION NOMMÉ : elle DÉTECTE la
# dépendance, rend « NON EXÉCUTÉ » avec le motif, et n'influence JAMAIS le code
# de retour — un cas non exécuté n'est ni un vert ni une divergence.
phase_begin
rollback_named_case() {
  local dir="$OUT/5-cas-nomme-privacy-rescan" report="$OUT/5-cas-nomme-privacy-rescan.txt"
  mkdir -p -- "$dir"
  local motif
  # Surfaces scannées : les sources Rust du dépôt, hors `target/`.
  local found
  found="$(grep -rl 'PrivacyRescan' --include='*.rs' \
             "$REPO_ROOT/crates" "$REPO_ROOT/tests" 2>/dev/null | head -n 5 || true)"
  if [[ -z "$found" ]]; then
    motif="NON EXÉCUTÉ, cas non disponible dans cet arbre — la variante \`Job::PrivacyRescan\` \
introduite par F-160 est absente des sources scannées (\$REPO_ROOT/crates, \$REPO_ROOT/tests, \
motif 'PrivacyRescan' sur *.rs). Le ruling E6 en fait le périmètre de la Task 2bis, après \
fusion de F-160 : ce harnais expose le point d'extension, il ne le simule pas."
  else
    motif="NON EXÉCUTÉ — la variante \`PrivacyRescan\` est PRÉSENTE dans cet arbre, mais la mise \
en œuvre du cas nommé (enfiler un job de cette variante depuis 2.5.0, redémarrer 2.4.0, \
observer rc / message / sort de la file) reste le périmètre de la Task 2bis (ruling E6). \
Point d'extension : \`rollback_named_case\` dans tests/upgrade/run-upgrade-test.sh. \
Sources détectées : $found"
  fi
  {
    printf 'cas=PrivacyRescan (variante Job introduite par F-160)\n'
    printf 'statut=NON EXÉCUTÉ\n'
    printf 'motif=%s\n' "$motif"
    printf 'influence_rc=aucune\n'
    printf 'point_extension=rollback_named_case (tests/upgrade/run-upgrade-test.sh)\n'
    printf 'repli_prevu=Task 2bis, après fusion de F-160 (ruling E6)\n'
  } >"$report"
  log "cas nommé PrivacyRescan : NON EXÉCUTÉ (point d'extension, ruling E6)"
  log "  motif : $motif"
}
rollback_named_case
phase_end "step3ter-cas-nomme"

# ── Step 4 : parité ──────────────────────────────────────────────────────────

# La force de la clé (e) dépend du SENS de la comparaison, et le harnais le dit :
#
#   montée (2.4.0 → 2.5.0) — `additif` : l'export est un RAPPORT, son dialecte
#     peut s'élargir d'une version à l'autre ; chaque entrée AVANT est comparée à
#     l'entrée MONTÉE projetée sur le dialecte AVANT, les clés ajoutées sont
#     relevées (`cles_ajoutees=…`) et doivent être citées dans le guide de montée,
#     qui est donc passé ici. Une clé AVANT disparue ou modifiée reste un écart.
#   retour arrière (2.4.0 → 2.4.0) — `strict` : le même binaire des deux côtés,
#     donc rien ne doit bouger, pas même un octet.
assert_phase() {  # $1 = dir avant, $2 = dir après, $3 = étiquette, $4 = fichier, $5 = force
  local rc=0
  local force="${5:-strict}"
  if [[ "$force" == "additif" ]]; then
    bash "$SCRIPT_DIR/assert-parity.sh" --before "$1" --after "$2" \
      --label "$3" --out "$4" --export-mode additif \
      --export-doc "$REPO_ROOT/docs/UPGRADING-2.4.0-to-2.5.0.md" || rc=$?
  else
    bash "$SCRIPT_DIR/assert-parity.sh" --before "$1" --after "$2" \
      --label "$3" --out "$4" --export-mode strict || rc=$?
  fi
  if (( rc == 0 )); then
    log "parité $3 : ÉTABLIE (clé (e) : $force)"
  elif (( rc == 1 )); then
    log "parité $3 : DIVERGENCE (voir $4)"
    RUN_RC=1
  else
    # Une mesure absente n'est pas une non-exécution DU HARNAIS (le harnais a
    # tourné) : c'est l'absence d'un relevé qu'il devait produire — ou une clé
    # (e) non mesurable — donc un écart nommé, jamais un vert et jamais un `2`
    # qui masquerait la cause.
    log "parité $3 : NON CONCLUE — relevé absent ou clé (e) non mesurable (voir $4)"
    RUN_RC=1
  fi
  return 0
}

phase_begin
P3_RECORDED=0
[[ -f "$P3/vault_list.paths.txt" ]] && P3_RECORDED=1
if (( P3_RECORDED )); then
  assert_phase "$P1" "$P3" "AVANT 2.4.0 → APRÈS 2.5.0 (la montée)" "$OUT/parity-1v3.txt" additif
else
  printf '%s\n' "NON CONCLUE — le relevé APRÈS (3-apres-250) n'existe pas : la montée 2.5.0 a \
refusé de démarrer ($(cat "$P2/startup-rc.txt" 2>/dev/null || echo 'motif indisponible')). \
C'est un RÉSULTAT, consigné sans être contourné." >"$OUT/parity-1v3.txt"
  log "parité AVANT→APRÈS : NON CONCLUE — montée refusée"
  RUN_RC=1
fi

if (( ROLLBACK_OK )); then
  assert_phase "$P1" "$P4" "AVANT 2.4.0 → RETOUR ARRIÈRE 2.4.0 sur l'état monté" \
    "$OUT/parity-1v4.txt" strict
else
  printf '%s\n' "NON CONCLUE — le quatrième relevé (4-retour-240) n'existe pas : le binaire \
2.4.0 n'a pas redémarré sur le répertoire monté \
($(cat "$P4/startup-rc.txt" 2>/dev/null || echo 'motif indisponible'))." >"$OUT/parity-1v4.txt"
  log "parité AVANT→RETOUR : NON CONCLUE — retour arrière refusé"
  RUN_RC=1
fi
phase_end "step4-parite"

# ── Axe F-XX (ruling E4) — APRÈS les quatre relevés, car il ÉCRIT dans l'état ─
#
# La fixture porte ses PROPRES cartes `[[feature:F-XX]]` (au moins deux, créées
# par la route serveur dans `gen-fixture.sh`) : c'est d'elles que vit l'axe
# « export de cartes », et non de celle-ci. L'axe échappe donc par construction
# aux quatre relevés comparables, puisqu'il en ajoute une APRÈS eux — l'exécuter
# plus tôt aurait pollué ce que la parité compare.
#
# Le numéro qu'il alloue suit ceux de la fixture : c'est ce qui rend la
# référence connue, et donc vérifiable. La comparaison porte sur le relevé AVANT
# (le plus grand numéro exporté par la fixture), jamais sur un numéro postulé.
phase_begin
FX_DIR="$(fresh_dir "$OUT/6-axe-f-xx")"
FX_RC=0
log "Axe F-XX — create-feature sur le serveur 2.5.0 (ruling E4)"
{
  printf 'axe=F-XX (carte feature, numéro alloué par le serveur)\n'
  printf 'regle=assertion sur la PRÉSENCE de la carte et son rôle [[feature:F-N]] ; \
J lu dans la réponse, JAMAIS asserté sur sa valeur\n'
} >"$FX_DIR/verdict.txt"

if start_server "$SERVER_250" "$FX_DIR" "serveur 2.5.0 (AXE F-XX)"; then
  JWT_FX="$(exchange)" || { FX_RC=1; bad "axe F-XX : échange clé→jeton refusé"; JWT_FX=""; }
  if [[ -n "$JWT_FX" ]]; then
    FX_BODY="$(printf '%s' '[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[release:planned]] [[version:gradatum/backlog]]

Carte produite par le harnais de montee, apres la montee 2.5.0.')"
    FX_RESP="$(api_post /api/v1/project-map/create-feature \
      "$(jq -n --arg b "$FX_BODY" '{title:"Carte de montee (harnais)", body:$b}')" "$JWT_FX")" \
      || { FX_RESP=""; FX_RC=1; bad "axe F-XX : create-feature injoignable"; }
    printf '%s' "$FX_RESP" >"$FX_DIR/create-feature.json"
    if [[ -n "$FX_RESP" ]]; then
      FX_FEATURE="$(jq -r '.feature // empty' <<<"$FX_RESP")"
      FX_NUMBER="$(jq -r '.number  // empty' <<<"$FX_RESP")"
      FX_JOB="$(jq -r '.job_id   // empty' <<<"$FX_RESP")"
      FX_NOTE="$(jq -r '.note_id  // empty' <<<"$FX_RESP")"
      {
        printf 'reponse_http=create-feature\n'
        printf 'feature=%s\nnumber=%s\njob_id=%s\nnote_id=%s\n' \
          "$FX_FEATURE" "$FX_NUMBER" "$FX_JOB" "$FX_NOTE"
      } >>"$FX_DIR/verdict.txt"

      if [[ "$FX_FEATURE" =~ ^F-[0-9]+$ ]]; then
        printf 'assertion_feature=ok (forme F-<entier>)\n' >>"$FX_DIR/verdict.txt"
      else
        printf 'assertion_feature=ÉCHEC (feature="%s" ne respecte pas ^F-[0-9]+$)\n' \
          "$FX_FEATURE" >>"$FX_DIR/verdict.txt"
        FX_RC=1
      fi
      # Le libellé porte le numéro en décimal ZÉRO-REMPLI — « F-01 » pour
      # number=1, observé — donc l'assertion porte sur la VALEUR, jamais sur la
      # forme du rembourrage (et jamais sur la valeur du numéro, ruling E4).
      fx_digits=""
      if [[ "$FX_FEATURE" =~ ^F-([0-9]+)$ ]]; then fx_digits="${BASH_REMATCH[1]}"; fi
      if [[ -n "$fx_digits" && "$FX_NUMBER" =~ ^[0-9]+$ ]] \
         && (( 10#$fx_digits == 10#$FX_NUMBER )); then
        printf 'assertion_coherence=ok (feature="%s" == "F-" + number=%s, rembourrage ignoré)\n' \
          "$FX_FEATURE" "$FX_NUMBER" >>"$FX_DIR/verdict.txt"
      else
        printf 'assertion_coherence=ÉCHEC (feature="%s" number="%s" — le libellé ne porte pas ce numéro)\n' \
          "$FX_FEATURE" "$FX_NUMBER" >>"$FX_DIR/verdict.txt"
        FX_RC=1
      fi

      # Le compteur de cartes a-t-il SURVÉCU à la montée ? Le numéro alloué ici
      # suit ceux que la fixture avait déjà consommés : s'il retombait dessous,
      # la montée aurait remis le compteur à zéro — et deux cartes porteraient le
      # même numéro, ce que l'identité d'une carte interdit. La référence est le
      # plus grand numéro EXPORTÉ par la fixture (relevé AVANT), lu et non
      # postulé : sur une base réelle les numéros sont arbitraires, ici ils sont
      # le résultat de la fixture.
      ref_max="$(jq -r '[.[].feature | sub("^F-"; "") | tonumber] | max // empty' \
        "$P1/export-features.json" 2>/dev/null || printf '')"
      if [[ "$ref_max" =~ ^[0-9]+$ && "$FX_NUMBER" =~ ^[0-9]+$ ]]; then
        if (( 10#$FX_NUMBER > 10#$ref_max )); then
          printf 'assertion_numero_superieur=ok (numéro %s > %s, plus grand numéro exporté par la fixture)\n' \
            "$FX_NUMBER" "$ref_max" >>"$FX_DIR/verdict.txt"
        else
          printf 'assertion_numero_superieur=ÉCHEC (numéro %s ≤ %s — le compteur de cartes a reculé à la montée)\n' \
            "$FX_NUMBER" "$ref_max" >>"$FX_DIR/verdict.txt"
          FX_RC=1
        fi
      else
        printf 'assertion_numero_superieur=NON CONCLUE (plus grand numéro de la fixture : %s ; \
numéro alloué : %s)\n' "${ref_max:-indisponible}" "${FX_NUMBER:-indisponible}" >>"$FX_DIR/verdict.txt"
        FX_RC=1
      fi

      # L'assertion de rôle porte sur le travail ENFILÉ : `create-feature` écrit
      # la note par un `Curate` dont le corps porte le rôle injecté. C'est la
      # preuve la plus forte disponible SANS consommateur de file — et celle qui
      # reste en CI, où aucun travailleur 2.5.0 n'est construit.
      if [[ -n "$FX_JOB" ]]; then
        api_post "/api/v1/jobs/$FX_JOB/v2" '{}' "$JWT_FX" >"$FX_DIR/job-v2.json" 2>/dev/null \
          || curl -fsS --max-time 20 "$BASE_URL/api/v1/jobs/$FX_JOB/v2" \
               -H "Authorization: Bearer $JWT_FX" >"$FX_DIR/job-v2.json" 2>/dev/null || true
        jq -r '.spec.kind.data.body // .spec.kind.data // empty' "$FX_DIR/job-v2.json" \
          >"$FX_DIR/job-enqueued-body.txt" 2>/dev/null || : >"$FX_DIR/job-enqueued-body.txt"
        if grep -qF "[[feature:$FX_FEATURE]]" "$FX_DIR/job-enqueued-body.txt"; then
          printf 'assertion_role_enfile=ok (le corps enfilé porte [[feature:%s]])\n' \
            "$FX_FEATURE" >>"$FX_DIR/verdict.txt"
        else
          printf 'assertion_role_enfile=ÉCHEC ([[feature:%s]] absent du corps enfilé, voir \
job-enqueued-body.txt)\n' "$FX_FEATURE" >>"$FX_DIR/verdict.txt"
          FX_RC=1
        fi
      else
        printf 'assertion_role_enfile=NON CONCLUE (aucun job_id dans la réponse)\n' \
          >>"$FX_DIR/verdict.txt"
        FX_RC=1
      fi

      # Matérialisation : elle exige un consommateur de file. Sans lui, la carte
      # n'est PAS dans l'index — le harnais le dit, il ne le suppose pas.
      if [[ -n "$WORKER_250" ]]; then
        log "axe F-XX : matérialisation demandée (--worker-bin-250 fourni)"
        GRADATUM_INTERNAL_URL="$INTERNAL_URL" \
        GRADATUM_INTERNAL_TOKEN="$INTERNAL_TOKEN" \
          "$WORKER_250" --config "$SERVER_CONFIG" --db "$STATE/db/queue.sqlite" \
          >>"$FX_DIR/worker.log" 2>&1 &
        WORKER_PID=$!
        st=""; deadline=$((SECONDS + 120))
        while (( SECONDS < deadline )); do
          st="$(curl -fsS --max-time 5 "$BASE_URL/api/v1/jobs/$FX_JOB/v2" \
                 -H "Authorization: Bearer $JWT_FX" | jq -r '.lifecycle.status // empty' 2>/dev/null || true)"
          case "$st" in Done) break ;; Failed|DLQ|Cancelled|Conflict) break ;; esac
          sleep 2
        done
        stop_pid "$WORKER_PID" travailleur; WORKER_PID=""
        capture_vault_list "$FX_DIR" "$JWT_FX" || true
        if grep -qxF "project-map/$FX_NOTE" "$FX_DIR/vault_list.paths.txt" 2>/dev/null; then
          printf 'assertion_materialisation=ok (project-map/%s présent dans vault_list, statut=%s)\n' \
            "$FX_NOTE" "$st" >>"$FX_DIR/verdict.txt"
        else
          printf 'assertion_materialisation=ÉCHEC (project-map/%s absent de vault_list, statut=%s)\n' \
            "$FX_NOTE" "$st" >>"$FX_DIR/verdict.txt"
          FX_RC=1
        fi
        capture_export "$FX_DIR" "$ADMIN_250"
        printf 'export_features_apres=%s carte(s)\n' \
          "$(jq 'length' "$FX_DIR/export-features.json" 2>/dev/null || echo '?')" \
          >>"$FX_DIR/verdict.txt"
      else
        printf '%s\n' "assertion_materialisation=NON ÉTABLIE — aucun travailleur 2.5.0 fourni \
(--worker-bin-250) : l'axe est exercé jusqu'à l'ENFILEMENT (allocation du numéro + injection du \
rôle, prouvées ci-dessus), pas jusqu'à la carte indexée. La commande CI gelée ne construit pas \
de travailleur 2.5.0 ; l'ajouter est un changement de .forgejo/workflows/ci.yml (build \
-p gradatum-worker côté HEAD + --worker-bin-250 sur la ligne du harnais) et de la ligne couplée \
de RELEASE-MANIFEST.yaml." >>"$FX_DIR/verdict.txt"
        log "axe F-XX : exercé jusqu'à l'enfilement ; matérialisation NON ÉTABLIE (pas de travailleur 2.5.0)"
      fi
    else
      printf 'reponse_http=aucune (create-feature n a pas repondu)\n' >>"$FX_DIR/verdict.txt"
    fi
  fi
  stop_server
else
  printf 'statut=NON EXÉCUTÉ — le serveur 2.5.0 n a pas demarre pour l axe F-XX (%s)\n' \
    "$(cat "$FX_DIR/startup-rc.txt" 2>/dev/null || echo 'motif indisponible')" \
    >>"$FX_DIR/verdict.txt"
  FX_RC=1
fi
if (( FX_RC != 0 )); then
  RUN_RC=1
  log "axe F-XX : ÉCHEC — voir $FX_DIR/verdict.txt"
else
  log "axe F-XX : exercé — voir $FX_DIR/verdict.txt"
fi
phase_end "step4bis-axe-fxx"

# ── Verdict ──────────────────────────────────────────────────────────────────

# Le verdict de non-vacuité de l'export (Step 1) est replié ici, et non dans
# `RUN_RC` au moment où il est constaté : posé trop tôt il aurait sauté la montée
# (2d est gardé par `RUN_RC == 0`), donc supprimé les trois autres relevés — un
# export vide aurait masqué ce que le harnais est là pour mesurer.
# Les clés que 2.5.0 ajoute au dialecte de l'export sont LUES dans le rapport de
# parité de la montée, jamais postulées : c'est la trace de ce qui a été relevé,
# et de ce que le guide de montée doit citer.
EXPORT_CLES_AJOUTEES="$(sed -n 's/.*cles_ajoutees=\([^ ]*\).*/\1/p' \
  "$OUT/parity-1v3.txt" 2>/dev/null | head -n 1)"
[[ -n "$EXPORT_CLES_AJOUTEES" ]] || EXPORT_CLES_AJOUTEES="indisponible"

EXPORT_DETAIL=""
if (( EXPORT_ECHECS > 0 )); then
  RUN_RC=1
  EXPORT_DETAIL=" ; $EXPORT_ECHECS relevé(s) d'export sur $EXPORT_RELEVES ne \
satisfont pas l'assertion de non-vacuité (export vide, tronqué ou illisible) — une \
parité octet à octet sur un export vide ne couvre rien"
fi

if (( RUN_RC == 0 )); then
  verdict_write 0 "PARITÉ PROUVÉE — la montée 2.5.0, sur un preset ACL conforme au dialecte \
cible (contrôlé en 2a), n'a bougé aucune clé bloquante ; le retour arrière 2.4.0 sur l'état \
monté est exercé et IDENTIQUE AU BIT près au relevé AVANT (clé (e) en mode strict) ; les \
$EXPORT_RELEVES relevés d'export de cartes portent chacun au moins $FEATURE_CARDS_EXPECTED \
carte(s) — l'axe « export de cartes » compare, sur un état peuplé, chaque entrée du dialecte \
AVANT à sa projection dans le relevé MONTÉ (règle additive), et relève les clés que 2.5.0 \
ajoute : cles_ajoutees=$EXPORT_CLES_AJOUTEES, citées dans le guide de montée."
else
  verdict_write 1 "DIVERGENCE — au moins un écart est nommé dans les rapports de parité \
(parity-1v3.txt, parity-1v4.txt), les relevés de démarrage (2-montee-250/, 4-retour-240/) \
ou le verdict de l'axe F-XX (6-axe-f-xx/verdict.txt)$EXPORT_DETAIL."
fi

{
  printf 'out=%s\n' "$OUT"
  printf 'state=%s\n' "$STATE"
  printf 'api_port=%s\n' "$API_PORT"
  printf 'montage_ok=%s\n' "$MOUNT_OK"
  printf 'retour_arriere_ok=%s\n' "$ROLLBACK_OK"
  printf 'premier_healthy_s=%s\n' "$(cat "$P3/first-healthy-secs.txt" 2>/dev/null || echo 'n/a')"
  printf 'cas_nomme_privacy_rescan=NON EXECUTE (point d extension, ruling E6)\n'
  printf 'preset_acl=%s\n' "${PRESET:-indisponible}"
  printf 'preset_chemin=%s\n' "${PRESET_CHEMIN:-n/a}"
  printf 'preset_controle_initial_rc=%s\n' \
    "$(cat "$P2C/validate-herite.rc" 2>/dev/null || echo 'n/a')"
  printf 'preset_controle_acl_validate=%s\n' \
    "$(if (( ${ACL_VALIDATE_OK:-0} )); then echo 'disponible'; else echo 'INDISPONIBLE (binaire fourni sans acl validate)'; fi)"
  printf 'preset_realigne_rc=%s\n' \
    "$(cat "$P2C/validate-realigne.rc" 2>/dev/null || echo 'n/a (sans objet)')"
  printf 'preset_deny_all_mesure=%s\n' "${DENY_ALL_MESURE:-0}"
  printf 'export_features_min_attendu=%s\n' "$FEATURE_CARDS_EXPECTED"
  printf 'export_features_releves=%s\n' "$EXPORT_RELEVES"
  printf 'export_features_releves_non_conformes=%s\n' "$EXPORT_ECHECS"
  printf 'export_features_cles_ajoutees=%s\n' "$EXPORT_CLES_AJOUTEES"
  printf 'preset_reversibilite_240=%s\n' "$(
    if (( ${ROLLBACK_OK:-0} )); then echo 'compatible (retour binaire seul suffisant)';
    else echo 'INCOMPATIBLE (retour de configuration requis)'; fi)"
} >>"$OUT/verdict.txt"

run_log "fin — rc=$RUN_RC"
cat "$OUT/verdict.txt"
exit "$RUN_RC"
