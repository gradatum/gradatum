#!/usr/bin/env bash
# =============================================================================
# deploy-gradatum-local.sh — deploy pérenne de gradatum sur the deployment host (system)
#
# Usage :
#   bash scripts/deploy-gradatum-local.sh [OPTIONS]
#
# Options :
#   --build                  Lance cargo build --release avant le deploy
#   --rebaseline-migrations  Re-base les checksums sqlx divergents (no-DDL only)
#   --gateway                Inclure gradatum-gateway (hôte local, /opt/gradatum/bin).
#                            Unité LIVE : gradatum-gateway.service (démarre
#                            indépendamment — PAS de After=gradatum-server).
#   --engine                 Inclure gradatum-engine (binaire unique multi-rôles).
#                            Déployé sur l'hôte local ET sur les hôtes de moteurs
#                            distants, selon un manifeste d'unités ATTENDUES
#                            (autorité, pas de découverte — voir ENGINE_UNITS_FILE).
#                            Les deux formes d'unité coexistent dans un même parc :
#                            instances templatées (`gradatum-engine@ROLE.service`) sur
#                            un hôte, unités plates (`gradatum-engine-ROLE.service`)
#                            sur un autre — c'est le packaging de chaque hôte qui
#                            tranche, jamais une convention globale.
#                            L'absence d'une unité attendue → ERREUR (jamais un
#                            no-op silencieux). Plancher glibc vérifié par hôte
#                            AVANT toute copie, puis VALIDATION EN ZONE DE TRANSIT
#                            (étape 0f) : le binaire est déposé HORS du chemin LIVE
#                            sur chaque hôte, y prouve son exécution réelle
#                            (--version) et y valide CHAQUE configuration servie
#                            (--check). Un seul refus ⇒ aucune substitution nulle
#                            part, binaires LIVE byte-identiques. Substitution
#                            ensuite, puis redémarrage séquentiel des moteurs
#                            distants, un à la fois, avec porte de santé ; anomalie
#                            à ce stade ⇒ restauration tout-ou-rien du parc.
#   --dry-run                Affiche le plan sans rien muter
#
# Chemins d'installation :
#   gradatum-server, gradatum-worker → /usr/bin/
#   gradatum-gateway, gradatum-engine → /opt/gradatum/bin/  (préfixe /opt)
#
# Hôtes (F-173) — le manifeste ENGINE_UNITS désigne chaque hôte par un jeton :
#   "local"  = cet hôte, piloté par systemctl direct
#   <autre>  = un alias ssh résolu par ~/.ssh/config (hôte, compte et clé y vivent),
#              qui doit accorder `sudo -n` sans mot de passe au compte de connexion.
#              Aucun nom d'hôte, compte ni adresse n'est écrit dans ce script :
#              c'est une propriété de l'installation, pas du produit.
#
# Leçon apprise (v0.3.7) :
#   Un commit anti-leak a modifié un COMMENTAIRE SQL dans une migration déjà
#   appliquée → checksum sqlx divergent → binaire refuse de démarrer.
#   Ce script détecte cette situation AVANT de swapper les binaires.
#
# Périmètre du backup (étape 2) — pourquoi `index.db` n'y figure PAS :
#   L'étape 2 sauvegarde les binaires LIVE et les DBs sqlx (api_keys, queue).
#   `index.db` (vault, cible des migrations rusqlite de gradatum-index) est
#   sauvegardé ailleurs, et c'est délibéré : les deux units systemd portent
#   `ExecStartPre=/usr/local/bin/gradatum-pre-migration-backup`
#   (source : packaging/systemd/gradatum-pre-migration-backup), exécuté à CHAQUE
#   démarrage — donc juste avant que le runner de migrations touche le schéma.
#   Ce point de garde est strictement plus couvrant que l'étape 2 :
#     - il couvre TOUS les chemins qui migrent (ce script, un `systemctl restart`
#       manuel, un restart après crash, un reboot), pas seulement un deploy ;
#     - il copie à froid, service arrêté, après `wal_checkpoint(TRUNCATE)` ;
#     - il est fail-closed : backup KO ⇒ systemd refuse de démarrer le service,
#       donc rien n'est migré.
#   Recopier `index.db` ici n'ajouterait aucune couverture : +92 Mo par deploy et
#   une seconde politique de rétention concurrente, pour le même instantané.
#   Restaurer (opération MANUELLE, services arrêtés) :
#     /var/lib/gradatum/backups/pre-migration/<TS>/dbs.tar.gz  (+ SHA256SUMS)
#     sha256sum -c SHA256SUMS && sudo tar -xzf dbs.tar.gz -C /
#
# Prérequis :
#   - sqlite3, sha384sum, jq, curl, systemctl en PATH
#   - sudo (pour systemctl et sqlite3 sur /var/lib/gradatum)
#   - Dépôt gradatum cloné, ce script exécuté depuis ~/projects/gradatum/
# =============================================================================

# ---------------------------------------------------------------------------
# Constantes
# ---------------------------------------------------------------------------

# Résolution symlink-safe : readlink -f suit le symlink vers le vrai fichier.
# Sans ça, invoqué via ~/scripts/deploy-gradatum-local.sh (symlink), dirname
# retourne ~/scripts et PROJECT_DIR pointe sur $HOME au lieu du dépôt.
# Résolution PERMISSIVE au chargement (set -euo pipefail vit désormais dans main,
# donc pas d'errexit ici) : un échec ne doit ni tuer un shell qui SOURCE le
# fichier, ni laisser une valeur silencieusement fausse. On force donc la valeur
# vide sur échec, et main() VALIDE strictement PROJECT_DIR avant toute action.
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")" || SCRIPT_PATH=""
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." 2>/dev/null && pwd)" || PROJECT_DIR=""

# Crates avec migrations sqlx (source : grep sqlx::migrate! dans les crates)
# Format : "chemin_relatif_migrations:chemin_db_absolue"
SQLX_MIGRATION_SPECS=(
    "crates/gradatum-acl-auth/migrations:/var/lib/gradatum/db/api_keys.sqlite"
    "crates/gradatum-db-sqlite/migrations:/var/lib/gradatum/db/queue.sqlite"
)

# Services systemd (system, pas user) — arrêt worker d'abord, démarrage server d'abord
UNITS_STOP_ORDER=("gradatum-worker" "gradatum-server")
UNITS_START_ORDER=("gradatum-server" "gradatum-worker")

# Binaires à déployer (liste de base ; gateway/engine ajoutés dynamiquement selon flags)
# gradatum-admin est une CLI opérateur (pas un service) : elle n'a ni unit systemd
# ni sonde de santé. Elle est présente ici pour être BUILDÉE (0c), contrôlée par
# build_sha (0d), sauvegardée (2) et installée (4) au même titre que les services —
# les listes UNITS_STOP_ORDER/UNITS_START_ORDER et le health check restent, eux,
# strictement server+worker et ne la concernent pas.
BINARIES=("gradatum-server" "gradatum-worker" "gradatum-admin")
INSTALL_DIR="/usr/bin"
OPT_INSTALL_DIR="/opt/gradatum/bin"   # préfixe partagé gateway + engine

# Unité LIVE de la passerelle (F-173). PAS gradatum-gateway-spike.service (celle-là
# est marquée « non-LIVE » dans le packaging et porte After=gradatum-server — que la
# prod évite délibérément).
GATEWAY_UNIT="gradatum-gateway.service"

# ---------------------------------------------------------------------------
# Manifeste des unités engine ATTENDUES — AUTORITÉ, et CONFIGURATION D'INSTALLATION
# ---------------------------------------------------------------------------
# Ce manifeste remplace la DÉCOUVERTE par motif `gradatum-engine@*` : sur un hôte dont
# les unités engine sont PLATES (`gradatum-engine-ROLE.service`, sans instance `@`), ce
# motif rendait ZÉRO unité — rien à redémarrer, aucun signalement, moteurs laissés sur
# l'ancien binaire (piège #1). Une liste ATTENDUE convertit ce no-op silencieux en
# erreur explicite : une unité déclarée mais introuvable fait échouer le deploy.
#
# Mais cette liste décrit la TOPOLOGIE D'UNE INSTALLATION — quels hôtes, quels rôles —
# et pas le produit. Elle est donc LUE depuis un fichier de configuration ; le défaut
# intégré plus bas ne connaît que l'hôte local. Un parc distant ne se devine pas, et
# l'inventer rouvrirait exactement le piège #1 à l'envers (une unité attendue qui
# n'existe nulle part fait échouer un deploy parfaitement sain).
#
# Fichier lu, dans l'ordre :
#   1. $GRADATUM_DEPLOY_ENGINE_UNITS — chemin explicite. Défini mais illisible ⇒ ERREUR,
#      jamais un repli : demander un manifeste précis puis en servir un autre en silence
#      est la classe de défaut que tout ce bloc existe pour empêcher.
#   2. scripts/internal/deploy-engine-units.conf sous la racine du dépôt (emplacement
#      OPÉRATEUR, hors distribution : la topologie d'un parc n'a pas à être publiée).
#   3. À défaut : le défaut intégré ci-dessous, avec un WARN — jamais en silence.
#
# Format : une entrée « hôte|unité » par ligne ; « # » commente jusqu'à la fin de ligne,
# lignes vides ignorées, espaces de bord retirés.
#   hôte  = "local" (cet hôte) ou un alias ssh (voir l'en-tête « Hôtes »)
#   unité = nom d'unité systemd COMPLET, instance templatée ou unité plate — les deux
#           formes coexistent dans un même parc, c'est tout l'objet du manifeste.
# Exemple :
#   local|gradatum-engine@curator.service
#   engine-host|gradatum-engine-embed.service
#
# L'engine est UN binaire unique multi-rôles : le même artefact est poussé sur tous les
# hôtes du manifeste, et son plancher glibc est vérifié hôte par hôte (étape 0e).
ENGINE_UNITS_FILE="${GRADATUM_DEPLOY_ENGINE_UNITS:-}"
ENGINE_UNITS_FILE_EXPLICIT=false
if [[ -n "$ENGINE_UNITS_FILE" ]]; then
    ENGINE_UNITS_FILE_EXPLICIT=true
else
    ENGINE_UNITS_FILE="${PROJECT_DIR}/scripts/internal/deploy-engine-units.conf"
fi

# Défaut intégré — hôte local seul, aucun nom propre d'installation.
ENGINE_UNITS_DEFAULT=(
    "local|gradatum-engine@curator.service"
    "local|gradatum-engine@embed.service"
)

# Peuplé par engine_units_load (appelée en 0b-bis, sous --engine seulement).
# Init inconditionnelle : lu sous set -u.
ENGINE_UNITS=()

# Options ssh communes aux opérations distantes. BatchMode=yes : jamais de
# prompt interactif (échoue franchement si la clé ne passe pas, plutôt que de bloquer).
SSH_OPTS=(-o ConnectTimeout=8 -o BatchMode=yes)

# Health endpoint (serveur local)
HEALTH_URL="http://127.0.0.1:19090/health"
HEALTH_TIMEOUT_SECS=30
HEALTH_POLL_INTERVAL=2

# Porte de santé des moteurs engine (redémarrage séquentiel des moteurs distants).
# Type=simple ⇒
# is-active passe à 'active' dès l'exec ; on confirme la STABILITÉ (pas de boucle de
# restart sur binaire/config KO) en re-lisant l'état après ENGINE_HEALTH_STABLE_SECS.
ENGINE_HEALTH_TIMEOUT_SECS=60
ENGINE_HEALTH_POLL=3
ENGINE_HEALTH_STABLE_SECS=5

# Zone de transit (étape 0f) — préfixe du chemin où le NOUVEAU binaire engine est déposé
# sur un hôte distant pour y être validé, AVANT toute substitution du binaire LIVE.
# Choisi hors de OPT_INSTALL_DIR à dessein : rien de ce qui n'a pas encore validé ne doit
# se trouver sur le chemin qu'une unité systemd pourrait exécuter. Suffixé par TS (un run,
# un chemin) et retiré par engine_stage_cleanup, succès comme échec.
ENGINE_STAGE_PREFIX="/tmp/gradatum-engine.stage"

# Backup
BACKUP_BASE="${HOME}/backups"

# ---------------------------------------------------------------------------
# Utilitaires
# ---------------------------------------------------------------------------

log()  { echo "[$(date '+%H:%M:%S')] $*"; }
info() { log "INFO  $*"; }
warn() { log "WARN  $*"; }
fail() {
    log "FAIL  $*" >&2
    # Hors déploiement (fichier sourcé, fonction appelée à la main), `exit` fermerait le
    # SHELL DE L'APPELANT. On rend 1 à la place — même diagnostic, sans dégât collatéral.
    deploy_in_progress || {
        log "      (hors déploiement : 'fail' rend 1 au lieu de fermer le shell appelant)" >&2
        return 1
    }
    exit 1
}

# ---------------------------------------------------------------------------
# Garde d'appel légitime (F-186) — « suis-je appelé à ma place ? »
#
# F-185 a rendu ce fichier sûr à SOURCER (main ne s'exécute plus qu'au lancement). Le
# sourcing est donc devenu la façon normale d'éprouver ces fonctions — et du même coup,
# les fonctions les plus dangereuses du fichier sont devenues les plus faciles à
# atteindre : sourcée puis appelée avec un répertoire, rollback() écrase les binaires
# LIVE et démarre leurs unités, hors de tout déploiement et de tous ses contrôles.
# Rendre un artefact sûr à charger déplace le risque vers ce que le chargement expose.
#
# Le marqueur retenu est la PILE D'APPELS, pas une variable posée par main() :
#   - la pile EST le contexte ; une variable n'en est qu'une assertion, que l'appelant
#     peut poser lui-même — « MARQUEUR=1 rollback /tmp/x » — c'est-à-dire exactement le
#     geste à empêcher ;
#   - une variable NON positionnée s'évalue VRAIE dans `if $VAR` (bash exécute alors une
#     commande nulle, statut 0) : un marqueur porté par variable échoue OUVERT dans son
#     propre cas nominal — fichier sourcé, main jamais entré, marqueur jamais posé.
#     C'est déjà ce qui se produit ici avec $FLAG_GATEWAY / $FLAG_ENGINE, non positionnés
#     après un simple `source` : rollback() prenait leurs branches ;
#   - une variable posée par main() lui survit dans le shell qui a sourcé le fichier
#     (rémanence après un run), et s'hérite d'un processus enfant si elle est exportée.
# La pile n'a aucun de ces trois défauts, ne demande ni état, ni nettoyage, ni polarité.
#
# Limite ASSUMÉE : un appelant qui définit lui-même une fonction nommée `main` et appelle
# rollback depuis elle passe la garde. Ce n'est plus la maladresse visée (un `source`
# suivi d'un appel), c'est une contrefaçon délibérée — hors modèle de menace.
deploy_in_progress() {
    local IFS=' '
    [[ " ${FUNCNAME[*]} " == *" main "* ]]
}

# refuse_out_of_deploy NOM_FONCTION — refus commun aux chemins de restauration appelés
# hors déploiement : aucun effet, et un message qui dit POURQUOI et par où passer.
# Rend 2 (distinct de 1, qui reste « la restauration a été tentée et a échoué »).
refuse_out_of_deploy() {
    warn "${1}() IGNORÉ — aucun déploiement en cours dans ce processus (fonction appelée hors de main, typiquement après un \`source\` de ce fichier)." >&2
    warn "  Aucun effet : ni binaire écrasé, ni unité systemd arrêtée ou démarrée." >&2
    warn "  Ces fonctions ne sont pas des outils autonomes — ce sont les chemins d'échec du déploiement. Hors deploy, elles écraseraient des binaires LIVE et redémarreraient des unités sans AUCUN des contrôles du script (build_sha, intégrité sqlx, sauvegarde horodatée, porte de santé)." >&2
    warn "  Rollback légitime : relancer 'bash scripts/deploy-gradatum-local.sh' — c'est le déploiement qui déclenche la restauration lorsqu'il échoue." >&2
    warn "  Restauration manuelle assumée (opérateur, services arrêtés) : sudo systemctl stop <unité> ; sudo install -m 0755 -o root -g root <BACKUP>/<binaire> <chemin LIVE> ; sudo systemctl start <unité>." >&2
    return 2
}

# Exécute la commande, ou l'affiche seulement en dry-run
dry() {
    if $FLAG_DRY_RUN; then
        echo "[DRY-RUN] $*"
    else
        "$@"
    fi
}

# Répertoire d'installation pour un binaire donné.
# Server/worker/admin → /usr/bin, gateway/engine → /opt/gradatum/bin.
install_dir_for() {
    local bin="$1"
    case "$bin" in
        gradatum-engine|gradatum-gateway) echo "$OPT_INSTALL_DIR" ;;
        *)                                echo "$INSTALL_DIR" ;;
    esac
}

# ---------------------------------------------------------------------------
# Périmètre étendu (F-173) — helpers gateway/engine multi-hôtes.
# TOUS read-only (existence, glibc, état systemd) : s'exécutent aussi en dry-run,
# car les portes d'absence-d'unité (piège #1) et de plancher glibc (piège #2)
# doivent pouvoir FAIRE ÉCHOUER le plan avant toute mutation. `|| true` en fin de
# substitution évite qu'un échec réseau/objdump n'abaisse set -e dans l'appelant.
# ---------------------------------------------------------------------------

# engine_units_load — peuple ENGINE_UNITS depuis ENGINE_UNITS_FILE, ou depuis le défaut
# intégré. Appelée en 0b-bis, avant toute lecture du manifeste.
#
# Trois refus, parce que les trois dégradations possibles sont SILENCIEUSES si on les
# laisse passer — et un manifeste amputé ne se voit pas : le deploy réussit, sur moins
# d'unités qu'attendu. C'est le piège #1 rhabillé en fichier de configuration.
#   - chemin explicite illisible  ⇒ fail (on a demandé CE manifeste, pas un autre) ;
#   - fichier présent mais sans aucune entrée ⇒ fail (tronqué / entièrement commenté) ;
#   - entrée mal formée ⇒ fail en la citant (jamais ignorée en passant).
# Le repli sur le défaut intégré, lui, est légitime — mais annoncé par un WARN.
engine_units_load() {
    local line raw n=0
    if [[ -r "$ENGINE_UNITS_FILE" ]]; then
        while IFS= read -r raw || [[ -n "$raw" ]]; do
            line="${raw%%#*}"
            line="${line#"${line%%[![:space:]]*}"}"
            line="${line%"${line##*[![:space:]]}"}"
            [[ -z "$line" ]] && continue
            n=$((n + 1))
            if [[ "$line" != *"|"* || "${line%%|*}" == "" || "${line#*|}" == "" || "${line#*|}" == *"|"* ]]; then
                fail "Manifeste engine invalide (${ENGINE_UNITS_FILE}, entrée ${n}) : « ${line} » — forme attendue « hôte|unité », un seul séparateur. Deploy annulé."
                return 1
            fi
            ENGINE_UNITS+=("$line")
        done < "$ENGINE_UNITS_FILE"
        if [[ ${#ENGINE_UNITS[@]} -eq 0 ]]; then
            fail "Manifeste engine vide : ${ENGINE_UNITS_FILE} ne déclare aucune unité. Un manifeste vide déploierait ZÉRO moteur en rendant un succès — deploy annulé."
            return 1
        fi
        info "  Manifeste engine : ${ENGINE_UNITS_FILE} (${#ENGINE_UNITS[@]} unités)"
        return 0
    fi
    if $ENGINE_UNITS_FILE_EXPLICIT; then
        fail "Manifeste engine illisible : ${ENGINE_UNITS_FILE} (GRADATUM_DEPLOY_ENGINE_UNITS) — chemin demandé explicitement, aucun repli. Deploy annulé."
        return 1
    fi
    ENGINE_UNITS=("${ENGINE_UNITS_DEFAULT[@]}")
    warn "  Manifeste engine : ${ENGINE_UNITS_FILE} absent — défaut intégré (hôte local seul, ${#ENGINE_UNITS[@]} unités). Les moteurs d'un hôte distant NE SERONT PAS déployés. Déclarer le parc dans ce fichier, ou pointer GRADATUM_DEPLOY_ENGINE_UNITS ailleurs."
    return 0
}

# host_reachable HOST — vrai si l'hôte répond (local toujours vrai). Distingue
# « hôte injoignable » d'« unité absente » (messages d'erreur non ambigus).
host_reachable() {
    local host="$1"
    [[ "$host" == "local" ]] && return 0
    ssh "${SSH_OPTS[@]}" "$host" true >/dev/null 2>&1
}

# unit_exists HOST UNIT — vrai si l'unité est réellement INSTALLÉE et configurée pour
# tourner, via `systemctl is-enabled` (état d'installation).
#
# Pourquoi is-enabled et pas LoadState/cat :
#   - `LoadState` rend "loaded" pour TOUTE instance d'un template existant, même bidon
#     (gradatum-engine@N'IMPORTE_QUOI.service) — aveugle aux instances templatées.
#   - `systemctl cat` rend rc!=0 SANS sudo sur une unité à drop-in root-only (faux
#     négatif observé sur gradatum-gateway.service).
#   - `is-enabled` distingue proprement (vérifié sur les 8 unités) : "enabled" pour les
#     unités réelles (gateway, instances templatées locales, unités plates d'un hôte
#     distant) ; "disabled" pour une instance templatée non configurée ;
#     "not-found" pour une unité distincte absente. Fiable sans sudo, local et via ssh.
# Ensemble de REJET explicite = absente ou non-voulue : le reste (enabled/static/…) est
# accepté. C'est ce test qui convertit le no-op silencieux de la découverte par motif en
# ERREUR explicite (piège #1).
unit_exists() {
    local host="$1" unit="$2" state
    if [[ "$host" == "local" ]]; then
        state=$(systemctl is-enabled "$unit" 2>/dev/null || true)
    else
        # Expansion client-side VOULUE : $unit vient du manifeste (valeur de confiance,
        # noms d'unités fixes), envoyée résolue au distant. ssh joint les args en une
        # seule chaîne — le passage positionnel `_ "$unit"` ne fonctionne pas ici.
        # shellcheck disable=SC2029
        state=$(ssh "${SSH_OPTS[@]}" "$host" "systemctl is-enabled '$unit'" 2>/dev/null || true)
    fi
    case "$state" in
        ""|not-found|disabled|masked) return 1 ;;
        *)                            return 0 ;;
    esac
}

# sc_is_active HOST UNIT — état systemd (active/failed/activating/…) ; jamais fatal.
sc_is_active() {
    local host="$1" unit="$2"
    if [[ "$host" == "local" ]]; then
        systemctl is-active "$unit" 2>/dev/null || true
    else
        # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
        ssh "${SSH_OPTS[@]}" "$host" "systemctl is-active '$unit'" 2>/dev/null || true
    fi
}

# host_glibc HOST — version glibc de l'hôte (dernier X.Y de `ldd --version`).
host_glibc() {
    local host="$1" line
    if [[ "$host" == "local" ]]; then
        line=$(ldd --version 2>/dev/null | head -1 || true)
    else
        line=$(ssh "${SSH_OPTS[@]}" "$host" 'ldd --version 2>/dev/null | head -1' 2>/dev/null || true)
    fi
    printf '%s\n' "$line" | grep -oE '[0-9]+\.[0-9]+' | tail -1 || true
}

# binary_required_glibc PATH — plus haute version GLIBC_x.y référencée par le binaire
# (objdump LOCAL : on mesure l'artefact target/release qu'on s'apprête à pousser).
binary_required_glibc() {
    objdump -T "$1" 2>/dev/null | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -V | tail -1 | sed 's/GLIBC_//' || true
}

# glibc_ge HAVE NEED — vrai si HAVE >= NEED (comparaison de version X.Y via sort -V).
glibc_ge() {
    [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" == "$2" ]]
}

# check_glibc_target BIN_PATH HOST — REFUSE (fail) si l'hôte est SOUS le plancher
# glibc du binaire. Exécuté AVANT toute copie (piège #2) : la marge peut être NULLE sur
# un hôte de moteurs (mesuré : binaire exigeant 2.39, hôte fournissant exactement 2.39)
# — un futur binaire bâti contre 2.40 doit échouer ici, pas au démarrage là-bas.
check_glibc_target() {
    local bin_path="$1" host="$2" need have
    need=$(binary_required_glibc "$bin_path")
    [[ -z "$need" ]] && fail "glibc requise illisible pour ${bin_path} (objdump) — copie refusée"
    have=$(host_glibc "$host")
    [[ -z "$have" ]] && fail "glibc de l'hôte ${host} illisible (ldd) — copie refusée"
    if glibc_ge "$have" "$need"; then
        info "  glibc OK : $(basename "$bin_path") exige ${need}, ${host} fournit ${have}"
    else
        fail "PLANCHER GLIBC : $(basename "$bin_path") exige GLIBC_${need} mais l'hôte ${host} ne fournit que ${have} — copie refusée (le binaire ne démarrerait pas). Rebuild contre une glibc ≤ ${have}, ou déployer depuis un hôte compatible."
    fi
}

# ---------------------------------------------------------------------------
# Validation en ZONE DE TRANSIT (étape 0f) — déplace le verdict AVANT le point de
# non-retour.
#
# Le binaire engine est PARTAGÉ par tous les moteurs d'un hôte : le remplacer engage
# d'un coup toutes ses unités. Jusqu'ici la seule preuve avant substitution était
# l'inspection statique d'objdump (0e), qui compare des chaînes de version GLIBC_x.y —
# elle ne prouve ni que le binaire s'exécute là-bas, ni qu'une configuration est encore
# servable par lui. Le verdict tombait donc au redémarrage, après la substitution : le
# filet de restauration (engine_abort_restore) répare ce cas, il ne l'évite pas.
#
# Ici on l'évite, pour toutes les causes détectables : le binaire est déposé hors du
# chemin LIVE, il y prouve son exécution d'un geste (--version : plancher glibc,
# bibliothèques dynamiques, architecture — bien plus fort qu'objdump), puis il valide
# CHAQUE configuration réellement servie sur cet hôte (--check).
#
# Un refus ici n'appelle JAMAIS engine_abort_restore : rien n'a été muté, il n'y a rien
# à restaurer. Restaurer un parc qui n'a pas bougé serait un contresens — et un risque
# ajouté par la mesure elle-même. On sort par `fail`, chemin d'erreur simple.
#
# Angles morts assumés (le verdict n'est pas une garantie de démarrage) :
#   - --check émet sur stderr une NOTE informative pour CHACUNE des configurations de ce
#     parc (l'api-key du journal d'événements vient d'un EnvironmentFile systemd,
#     invisible depuis un shell). Le checker l'exclut lui-même de son verdict ; ici seul
#     le CODE DE RETOUR décide. Cette note n'est pas relayée à chaque configuration : la
#     répéter 5× par deploy entraînerait à ignorer la sortie.
#   - identité : --check tourne sous le compte de connexion, qui n'est pas partout le
#     `User=` des unités. Sur un hôte distant les deux coïncident (le compte ssh EST le
#     `User=`) ; en local ils divergent — le compte de déploiement lance le check, un
#     compte de service dédié sert les unités. Vérifié le 2026-08-18 : verdicts
#     identiques sous les deux identités locales sur les deux configurations locales,
#     parce que les fichiers lus sont en 0644. Un durcissement futur des permissions
#     rouvrirait cet écart.
#   - ce qui n'est pas testé reste non testé : bind effectif des ports, acceptation par
#     le serveur, chargement du modèle par llama-server.
# ---------------------------------------------------------------------------

# Chemins de transit RÉELLEMENT déposés : entrées "host|path". Alimenté au dépôt (0f),
# consommé par engine_stage_cleanup. Init inconditionnelle (lu sous set -u).
ENGINE_STAGED=()

# engine_stage_cleanup — retire les binaires de transit sur TOUS les hôtes où un dépôt a
# effectivement eu lieu, SUCCÈS COMME ÉCHEC (armé en trap EXIT, pas appelé en ligne : un
# `fail` sauterait un appel en ligne, jamais le trap). Un binaire non validé abandonné
# dans /tmp sur un hôte de production est un piège pour le prochain : il ressemble à un
# artefact légitime et ne porte aucune trace de son rejet.
# Best-effort ASSUMÉ : le trap s'exécute après que le code de sortie est arrêté, donc un
# échec de nettoyage ne peut pas changer le verdict du deploy — il est signalé, avec sa
# commande de reprise.
engine_stage_cleanup() {
    local entry shost spath
    for entry in "${ENGINE_STAGED[@]:-}"; do
        [[ -z "$entry" ]] && continue
        shost="${entry%%|*}"
        spath="${entry#*|}"
        # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
        if ssh "${SSH_OPTS[@]}" "$shost" "rm -f '${spath}'" >/dev/null 2>&1; then
            info "  [${shost}] transit nettoyé : ${spath}"
        else
            warn "  [${shost}] transit NON nettoyé : ${spath} — reprise : ssh ${shost} \"rm -f ${spath}\""
        fi
    done
    ENGINE_STAGED=()
}

# engine_unit_config_path HOST UNIT — chemin de configuration SERVI par une unité, LU
# sur l'hôte dans l'argument de son ExecStart. Jamais fabriqué par motif à partir du nom
# d'unité : le manifeste nomme des unités, pas des fichiers, et systemd est seul à savoir
# ce que %i résout pour une instance templatée.
# Protocole de retour (stdout, toujours rc=0 — l'appelant décide, aucun errexit subi) :
#   "OK|<chemin>"   chemin résolu sans ambiguïté
#   "ERR|<raison>"  rien n'est deviné : l'appelant échoue en nommant la raison
engine_unit_config_path() {
    local host="$1" unit="$2" raw argv_seg
    local -a segs=() toks=()
    if [[ "$host" == "local" ]]; then
        raw=$(systemctl show -p ExecStart --value "$unit" 2>/dev/null || true)
    else
        # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
        raw=$(ssh "${SSH_OPTS[@]}" "$host" "systemctl show -p ExecStart --value '$unit'" 2>/dev/null || true)
    fi
    if [[ -z "$raw" ]]; then
        echo "ERR|ExecStart illisible (systemctl show sans réponse sur ${host})"
        return 0
    fi
    mapfile -t segs < <(printf '%s\n' "$raw" | sed -nE 's/.*argv\[\]=([^;]*);.*/\1/p')
    if [[ ${#segs[@]} -ne 1 ]]; then
        echo "ERR|ExecStart porte ${#segs[@]} commande(s) au lieu d'une seule — forme d'unité inattendue, aucune supposition n'est faite (brut : ${raw})"
        return 0
    fi
    argv_seg="${segs[0]}"
    read -r -a toks <<< "$argv_seg"
    if [[ ${#toks[@]} -ne 2 ]]; then
        echo "ERR|ExecStart porte ${#toks[@]} mot(s) au lieu de 2 (binaire + configuration) — impossible de désigner la configuration sans ambiguïté (argv : ${argv_seg})"
        return 0
    fi
    echo "OK|${toks[1]}"
}

# engine_validate_transit HOST BIN_PATH UNIT [UNIT…]
# Valide BIN_PATH sur HOST, AVANT toute substitution :
#   1. BIN_PATH --version sur HOST — preuve d'exécution réelle là-bas (un seul geste :
#      plancher glibc, bibliothèques dynamiques, architecture) ;
#   2. BIN_PATH --check CONFIG pour CHAQUE unité servie sur cet hôte, la configuration
#      étant lue dans son ExecStart (engine_unit_config_path).
# Codes de --check : 0 servable · 1 non servable (raisons sur stderr) · 2 erreur d'usage.
# Tout autre code vient de la couche ssh et n'est PAS un verdict sur la configuration —
# les trois cas sont distingués, jamais confondus.
# Ne rend la main QUE si tout passe ; sinon `fail` (parc intact, rien à restaurer).
engine_validate_transit() {
    local host="$1" bin="$2"
    shift 2
    local unit cfg res out rc

    if $FLAG_DRY_RUN && [[ "$host" != "local" ]]; then
        echo "[DRY-RUN] [${host}] ${bin} --version   → doit rendre 0 (binaire exécutable SUR ${host})"
    else
        rc=0
        if [[ "$host" == "local" ]]; then
            out=$("$bin" --version 2>&1) || rc=$?
        else
            # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
            out=$(ssh "${SSH_OPTS[@]}" "$host" "'$bin' --version" 2>&1) || rc=$?
        fi
        if [[ $rc -ne 0 ]]; then
            fail "$(printf '%s\n' \
"TRANSIT REFUSÉ — le binaire de transit ne s'exécute pas sur ${host} (rc=${rc})." \
"  Binaire : ${bin}  (zone de transit — le binaire LIVE n'a pas été touché)" \
"  Sortie  : ${out}" \
"Causes typiques : plancher glibc, bibliothèque dynamique absente, architecture, ou accès ssh." \
"Aucune substitution n'a eu lieu : le parc est INTACT, il n'y a rien à restaurer.")"
        fi
        info "  [${host}] transit exécutable : ${out}"
    fi

    for unit in "$@"; do
        res=$(engine_unit_config_path "$host" "$unit")
        if [[ "${res%%|*}" != "OK" ]]; then
            fail "TRANSIT REFUSÉ — configuration de ${unit} sur ${host} non résolue : ${res#*|}. Aucune substitution n'a eu lieu, le parc est INTACT."
        fi
        cfg="${res#*|}"

        if $FLAG_DRY_RUN && [[ "$host" != "local" ]]; then
            echo "[DRY-RUN] [${host}] ${bin} --check ${cfg}   (unité ${unit}) → doit rendre 0"
            continue
        fi

        rc=0
        if [[ "$host" == "local" ]]; then
            out=$("$bin" --check "$cfg" 2>&1) || rc=$?
        else
            # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
            out=$(ssh "${SSH_OPTS[@]}" "$host" "'$bin' --check '$cfg'" 2>&1) || rc=$?
        fi

        case "$rc" in
            0)
                info "  [${host}] ${unit} → ${cfg} : servable (check OK)"
                ;;
            1)
                fail "$(printf '%s\n' \
"VALIDATION EN ZONE DE TRANSIT REFUSÉE — aucune substitution n'a eu lieu, le parc est INTACT." \
"  Hôte          : ${host}" \
"  Unité         : ${unit}" \
"  Configuration : ${cfg}   (lue dans l'ExecStart de l'unité, sur l'hôte)" \
"  Binaire testé : ${bin}   (zone de transit — le binaire LIVE reste byte-identique)" \
"  Raison rendue par --check :" \
"${out}" \
"Corriger la configuration (ou le binaire), puis relancer le deploy." \
"Rien à restaurer : le filet de restauration n'a PAS été déclenché, car rien n'a bougé.")"
                ;;
            2)
                fail "TRANSIT — erreur d'USAGE de --check sur ${host} pour ${unit} (rc=2) : le chemin '${cfg}' a été rejeté comme ARGUMENT ; ce n'est PAS un verdict sur la configuration. Sortie : ${out}. Deploy annulé, parc INTACT."
                ;;
            *)
                fail "TRANSIT — aucun verdict rendu pour ${unit} sur ${host} (rc=${rc}, hors du contrat 0/1/2 de --check) : l'exécution elle-même a échoué (ssh, permissions, binaire absent), la configuration '${cfg}' n'est ni validée ni invalidée. Sortie : ${out}. Deploy annulé, parc INTACT."
                ;;
        esac
    done
}

# engine_health_gate HOST UNIT — attend un état systemd 'active' STABLE (confirmé
# après ENGINE_HEALTH_STABLE_SECS pour écarter une boucle de restart sur binaire/config
# KO). Retourne 0 si sain, 1 sinon. N'est appelé qu'en mode réel (le dry-run décrit la
# porte sans l'exercer). Type=simple ⇒ 'active' ne prouve que l'exec, pas la
# disponibilité applicative ; c'est néanmoins suffisant pour capter le mode d'échec
# visé ici (le binaire ne démarre pas / crash immédiat), le plancher glibc (0e) ayant
# déjà écarté l'incompatibilité en amont.
engine_health_gate() {
    local host="$1" unit="$2" elapsed=0 state
    while [[ $elapsed -lt $ENGINE_HEALTH_TIMEOUT_SECS ]]; do
        state=$(sc_is_active "$host" "$unit")
        if [[ "$state" == "active" ]]; then
            sleep "$ENGINE_HEALTH_STABLE_SECS"
            state=$(sc_is_active "$host" "$unit")
            [[ "$state" == "active" ]] && return 0
        fi
        [[ "$state" == "failed" ]] && return 1
        sleep "$ENGINE_HEALTH_POLL"
        elapsed=$((elapsed + ENGINE_HEALTH_POLL))
    done
    return 1
}

# Restauration LOCALE complète : ARRÊT → binaires d'origine → DÉMARRAGE → état OBSERVÉ.
# DEUX appelants, tous deux sur échec d'une porte de santé après le deploy :
#   - étape 6  : health LOCAL non concluant ;
#   - étape 6b : moteur DISTANT non sain → via engine_abort_restore, APRÈS restauration
#                du distant (ordre imposé : distant d'abord, local ensuite).
#
# L'ARRÊT PRÉALABLE EST LOAD-BEARING (F-186), et il vit ici — pas chez un appelant.
# À l'appel, les unités locales tournent DÉJÀ le nouveau binaire (installé étape 4,
# démarré étape 5) : sur les deux chemins, pas seulement le 6b. Or :
#   - `systemctl start` est un NO-OP sur une unité active ;
#   - `sudo install` par-dessus un binaire EN COURS D'EXÉCUTION RÉUSSIT (coreutils délie
#     le fichier et réessaie — vérifié le 2026-08-18 sur le parc LIVE, aucun ETXTBSY).
# Restaurer sans arrêter rend donc un parc restauré SUR DISQUE dont les PROCESSUS
# tournent toujours l'artefact qu'on vient de refuser — pendant que le script annonce
# « rollback effectué ». C'est ce faux vert qu'a subi le chemin 6 jusqu'au 2026-08-18 :
# la parade n'existait que chez l'appelant du 6b, donc elle ne couvrait qu'un chemin.
# L'ordre ARRÊT → RESTAURATION → DÉMARRAGE est le contrat de cette fonction ; il n'est
# ni dupliqué ni délégué, pour qu'aucun futur appelant ne puisse l'oublier.
#
# Aucun verdict n'est déduit d'un code retour : les starts neutralisent leurs échecs
# (`|| true`), donc l'état de CHAQUE unité est relu (sc_is_active) et ce qui n'est pas
# revenu est consigné dans ENGINE_RESTORE_ERRORS avec sa commande de reprise — journal
# lu par engine_abort_restore pour son verdict final (chemin 6b), et rendu ici même en
# clair (les deux chemins).
#
# Périmètre : BINAIRES SEULEMENT. `index.db` n'est délibérément pas restauré ici.
#   Ce chemin s'exécute sans opérateur, sur un simple timeout de health check —
#   dont les causes les plus fréquentes n'ont rien à voir avec le schéma (port
#   occupé, config, dépendance absente). Réécrire 92 Mo de vault LIVE dans ce
#   cas détruirait sans retour toutes les écritures acceptées depuis le deploy,
#   pour réparer un problème qui n'est pas là. Un boot en échec se rejoue ; une
#   restauration de données, non.
#   Restaurer le schéma sous un binaire ancien reste par ailleurs un acte à
#   arbitrer (le couple binaire/schéma doit être choisi, pas subi) : cela reste
#   une opération manuelle, depuis l'archive pre-migration (voir en-tête).
rollback() {
    deploy_in_progress || { refuse_out_of_deploy rollback; return $?; }
    local bdir="$1" unit bin bak idir dst state
    local locals=()
    local errors_before=${#ENGINE_RESTORE_ERRORS[@]}

    warn "=== ROLLBACK ==="
    warn "Restauration depuis : ${bdir}"

    # --- 1/3 ARRÊT (avant toute réécriture — voir en-tête : sans lui, disque restauré,
    #     processus non). Un échec d'arrêt n'interrompt pas la restauration : il est dit,
    #     et l'état réel est relu en 3/3.
    warn "  Arrêt des unités locales avant restauration"
    if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
        for unit in "${ENGINE_LOCAL_UNITS[@]}"; do
            sudo systemctl stop "$unit" || warn "  [local] stop ${unit} : échec — état réel vérifié après restauration"
        done
    fi
    if $FLAG_GATEWAY; then
        sudo systemctl stop "$GATEWAY_UNIT" || warn "  [local] stop ${GATEWAY_UNIT} : échec — état réel vérifié après restauration"
    fi
    for unit in "${UNITS_STOP_ORDER[@]}"; do
        sudo systemctl stop "$unit" || warn "  [local] stop ${unit} : échec — état réel vérifié après restauration"
    done

    # --- 2/3 RESTAURATION DISQUE. Une sauvegarde absente (cas réel : premier deploy
    #     engine/gateway, rien à sauvegarder étape 2) n'est PAS sautée en silence :
    #     l'installation de l'étape 4 subsiste sur ce binaire-là, on le dit.
    for bin in "${BINARIES[@]}"; do
        bak="${bdir}/${bin}"
        idir="$(install_dir_for "$bin")"
        dst="${idir}/${bin}"
        if [[ -f "$bak" ]]; then
            sudo install -m 0755 -o root -g root "$bak" "$dst"
            warn "  Restauré : ${dst}"
        else
            warn "  [local] ${bin} : aucune sauvegarde ${bak} — binaire NON restauré"
            ENGINE_RESTORE_ERRORS+=("[local] ${bin} NON restauré : aucune sauvegarde ${bak} (binaire absent du parc avant ce deploy) — l'installation de l'étape 4 subsiste, retirer ou remplacer manuellement ${dst}")
        fi
    done

    # --- 3/3 DÉMARRAGE sur les binaires restaurés, puis ÉTAT OBSERVÉ.
    for unit in "${UNITS_START_ORDER[@]}"; do
        sudo systemctl start "$unit" 2>/dev/null || true
    done
    # Passerelle locale si --gateway
    if $FLAG_GATEWAY; then
        sudo systemctl start "$GATEWAY_UNIT" 2>/dev/null || true
        warn "  Redémarré : ${GATEWAY_UNIT}"
    fi
    # Instances engine LOCALES si --engine.
    # Périmètre LOCAL seulement, par construction : cette fonction ne connaît que le parc
    # local, et aucun de ses deux appelants ne lui demande de toucher au distant — sur
    # échec local (étape 6) le distant n'est pas encore modifié ; sur échec distant (6b) le
    # distant a DÉJÀ été restauré avant cet appel (engine_remote_restore).
    if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
        for unit in "${ENGINE_LOCAL_UNITS[@]}"; do
            sudo systemctl start "$unit" 2>/dev/null || true
            warn "  Redémarré : ${unit}"
        done
    fi

    locals+=("${UNITS_START_ORDER[@]}")
    if $FLAG_GATEWAY; then
        locals+=("$GATEWAY_UNIT")
    fi
    if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
        locals+=("${ENGINE_LOCAL_UNITS[@]}")
    fi
    for unit in "${locals[@]}"; do
        state=$(sc_is_active "local" "$unit")
        if [[ "$state" == "active" ]]; then
            warn "  [local] ${unit} : actif sur le binaire d'origine (is-active=active)"
        else
            warn "  [local] ${unit} : NON actif après restauration (is-active=${state:-inconnu})"
            ENGINE_RESTORE_ERRORS+=("[local] ${unit} n'est PAS 'active' après restauration (is-active=${state:-inconnu}) — reprise : sudo systemctl start ${unit}; systemctl status ${unit}")
        fi
    done

    # Verdict sur ce que CET appel a constaté (et non sur le journal global, qui porte
    # aussi les points distants du chemin 6b).
    if [[ ${#ENGINE_RESTORE_ERRORS[@]} -eq $errors_before ]]; then
        warn "Parc local RESTAURÉ : binaires d'origine réinstallés, unités actives dessus"
    else
        warn "Restauration locale INCOMPLÈTE — $(( ${#ENGINE_RESTORE_ERRORS[@]} - errors_before )) point(s) à reprendre (détail ci-dessus)"
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Restauration TOUT-OU-RIEN sur échec d'un moteur DISTANT (étape 6b) — F-173 critère 2 :
# « un déploiement qui ne peut pas mettre à niveau l'ensemble échoue et laisse le parc
# dans son état d'origine ».
#
# Sans ce chemin, un échec au 2e moteur sur 3 laissait un parc MIXTE : local déjà basculé
# ET validé (étapes 4-6), binaire engine distant déjà remplacé sur DISQUE pour tous les
# moteurs de chaque hôte copié, une partie des moteurs redémarrés sur le nouveau binaire,
# le reste encore sur l'ancien processus. L'échec était lisible, pas atomique.
#
# Ordre imposé : DISTANT d'abord (le plus exposé — binaire partagé par tous les moteurs
# d'un hôte, moteurs de supervision/curation du homelab), LOCAL ensuite, puis sortie
# non-zéro. Le local vient en second parce que le distant dépend du local qui tourne :
# on rend d'abord aux moteurs leur binaire, on ramène ensuite le serveur en arrière.
#
# Aucune restauration ne peut échouer en SILENCE : chaque commande dont l'échec est
# plausible est neutralisée explicitement (jamais subie par set -e), l'anomalie est
# consignée dans ENGINE_RESTORE_ERRORS avec sa commande de reprise manuelle, et la
# restauration CONTINUE. Une restauration partielle silencieuse serait pire que l'état
# d'origine : elle ferait croire au parc homogène.
# ---------------------------------------------------------------------------

# Journal des éléments NON restaurés : une entrée = un point resté en version mixte,
# formulé avec sa commande de reprise. Vidé au début de chaque engine_abort_restore.
ENGINE_RESTORE_ERRORS=()

# engine_remote_restore FAIL_IDX REMOTE_DST REMOTE_BAK
# Rend l'ANCIEN binaire à CHAQUE hôte distant ayant reçu une copie — pas seulement
# l'hôte fautif : la copie précède la boucle de redémarrage, donc plusieurs hôtes
# peuvent déjà être basculés sur disque. Redémarre ensuite les moteurs EFFECTIVEMENT
# redémarrés (index ≤ FAIL_IDX dans l'ordre d'itération de 6b — l'itération étant à plat
# sur ENGINE_REMOTE, ce seul critère couvre l'hôte fautif ET les hôtes déjà traités),
# pour qu'ils reprennent ce binaire. Vérifie is-active après chaque redémarrage.
engine_remote_restore() {
    deploy_in_progress || { refuse_out_of_deploy engine_remote_restore; return $?; }
    local fail_idx="$1" remote_dst="$2" remote_bak="$3"
    local ehost oidx ounit state
    for ehost in "${ENGINE_HOSTS_REMOTE[@]}"; do
        # Échec PLAUSIBLE ici (.bak absent, sudo refusé, hôte devenu injoignable) :
        # neutralisé par le `if` — set -e ne doit pas emporter la restauration locale.
        # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
        if ssh "${SSH_OPTS[@]}" "$ehost" "sudo install -m 0755 -o root -g root '${remote_bak}' '${remote_dst}'"; then
            warn "  [${ehost}] binaire restauré : ${remote_bak} → ${remote_dst}"
        else
            warn "  [${ehost}] ÉCHEC restauration binaire : ${remote_bak} → ${remote_dst}"
            warn "  [${ehost}] moteurs NON redémarrés — un redémarrage les relancerait sur le binaire NON validé (aggravation)"
            ENGINE_RESTORE_ERRORS+=("[${ehost}] ${remote_dst} porte TOUJOURS le binaire non validé (réinstallation de ${remote_bak} échouée) ; les moteurs déjà redémarrés de cet hôte tournent dessus — reprise : ssh ${ehost} \"sudo install -m 0755 -o root -g root ${remote_bak} ${remote_dst}\" puis redémarrer ses moteurs un à un")
            continue
        fi
        for oidx in "${!ENGINE_REMOTE[@]}"; do
            [[ "${ENGINE_REMOTE[$oidx]%%|*}" == "$ehost" ]] || continue
            [[ "$oidx" -le "$fail_idx" ]] || continue
            ounit="${ENGINE_REMOTE[$oidx]##*|}"
            # Échec plausible (unité en cours de basculement) : capté, jamais subi. Le
            # verdict ne s'appuie pas sur ce code retour mais sur l'état OBSERVÉ ensuite.
            # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
            ssh "${SSH_OPTS[@]}" "$ehost" "sudo systemctl restart '${ounit}'" \
                || warn "  [${ehost}] ${ounit} : commande restart en échec — état réel vérifié ci-dessous"
            state=$(sc_is_active "$ehost" "$ounit")
            if [[ "$state" == "active" ]]; then
                warn "  [${ehost}] ${ounit} : actif sur l'ANCIEN binaire (is-active=active)"
            else
                warn "  [${ehost}] ${ounit} : NON actif après restauration (is-active=${state:-inconnu})"
                ENGINE_RESTORE_ERRORS+=("[${ehost}] ${ounit} n'est PAS revenu 'active' après restauration du binaire d'origine (is-active=${state:-inconnu}) — reprise : ssh ${ehost} \"sudo systemctl restart ${ounit}; systemctl status ${ounit}\"")
            fi
        done
    done
    return 0
}

# engine_abort_restore FAIL_IDX FAIL_HOST FAIL_UNIT REMOTE_DST REMOTE_BAK BACKUP_DIR
# Point d'entrée du chemin d'échec 6b. NE REND JAMAIS LA MAIN : se termine par `fail`
# (sortie non-zéro), après restauration distante puis locale, en énonçant l'état final
# réellement observé — restauré, ou incomplet avec la liste de ce qui reste à reprendre.
engine_abort_restore() {
    deploy_in_progress || { refuse_out_of_deploy engine_abort_restore; return $?; }
    local fail_idx="$1" fail_host="$2" fail_unit="$3" remote_dst="$4" remote_bak="$5" bdir="$6"
    local oidx ounit host_count
    local new_running=() old_running=()

    # Populations des moteurs de l'hôte fautif, partitionnées par l'index d'itération
    # (information livrée en 8102d141, ici réarticulée autour de la restauration).
    for oidx in "${!ENGINE_REMOTE[@]}"; do
        [[ "${ENGINE_REMOTE[$oidx]%%|*}" == "$fail_host" ]] || continue
        ounit="${ENGINE_REMOTE[$oidx]##*|}"
        if   [[ "$oidx" -lt "$fail_idx" ]]; then new_running+=("$ounit")
        elif [[ "$oidx" -gt "$fail_idx" ]]; then old_running+=("$ounit")
        fi
    done
    host_count=$(( ${#new_running[@]} + 1 + ${#old_running[@]} ))

    warn "=== RESTAURATION TOUT-OU-RIEN — DÉPLOIEMENT ANNULÉ ==="
    warn "Déclencheur : ${fail_unit} sur ${fail_host} n'est pas devenu 'active' stable."
    warn "Le binaire ${remote_dst} est PARTAGÉ par les ${host_count} moteurs de ${fail_host} et a été"
    warn "remplacé sur disque pour TOUS avant la boucle de redémarrage (sauvegarde unique : ${remote_bak})."
    warn "  Basculés sur le NOUVEAU binaire (redémarrés avant l'échec) : ${new_running[*]:-aucun} — plus ${fail_unit} (redémarré, NON sain)."
    warn "  Encore sur l'ANCIEN processus, mais amorcés sur le NOUVEAU binaire au prochain restart/reboot/crash : ${old_running[*]:-aucun}."
    warn "Restaurer le .bak rend l'ANCIEN binaire aux ${host_count} moteurs D'UN COUP : c'est ce qui suit,"
    warn "distant d'abord, local ensuite. Le parc doit revenir à son état d'origine, pas rester mixte."

    ENGINE_RESTORE_ERRORS=()

    warn "--- 1/2 DISTANT : ${#ENGINE_HOSTS_REMOTE[@]} hôte(s) ayant reçu une copie (${ENGINE_HOSTS_REMOTE[*]}) ---"
    engine_remote_restore "$fail_idx" "$remote_dst" "$remote_bak"

    warn "--- 2/2 LOCAL : binaires depuis ${bdir} + unités locales ---"
    rollback "$bdir"

    if [[ ${#ENGINE_RESTORE_ERRORS[@]} -eq 0 ]]; then
        fail "$(printf '%s\n' \
"Deploy ANNULÉ : ${fail_unit} sur ${fail_host} n'est pas devenu 'active' stable après redémarrage." \
"ÉTAT FINAL : parc RESTAURÉ dans son état d'origine — restauration COMPLÈTE, aucun élément en version mixte." \
"  Distant : ${remote_dst} rendu au binaire d'origine sur ${ENGINE_HOSTS_REMOTE[*]} ; moteurs redémarrés vérifiés 'active'." \
"  Local   : binaires réinstallés depuis ${bdir}, unités locales vérifiées 'active'." \
"Sauvegardes CONSERVÉES : ${remote_bak} (distant) et ${bdir} (local)." \
"Avant de relancer le deploy, diagnostiquer la cause : ssh ${fail_host} \"systemctl status ${fail_unit}; journalctl -u ${fail_unit} -n 80 --no-pager\".")"
    else
        fail "$(printf '%s\n' \
"Deploy ANNULÉ : ${fail_unit} sur ${fail_host} n'est pas devenu 'active' stable après redémarrage." \
"ÉTAT FINAL : restauration INCOMPLÈTE — ${#ENGINE_RESTORE_ERRORS[@]} élément(s) NON restauré(s). Le parc reste MIXTE sur ces points, à reprendre à la main :" \
"$(printf '  - %s\n' "${ENGINE_RESTORE_ERRORS[@]}")" \
"Tout le reste a été restauré (distant tenté sur ${ENGINE_HOSTS_REMOTE[*]} ; local depuis ${bdir})." \
"Sauvegardes CONSERVÉES : ${remote_bak} (distant) et ${bdir} (local) — ne pas les supprimer avant reprise.")"
    fi
}

# ---------------------------------------------------------------------------
# main — orchestration complète du deploy (pré-vol, build_sha, intégrité sqlx,
# backup, arrêt, install, démarrage, health + rollback). La garde de source en
# fin de fichier n'appelle main QUE si le script est EXÉCUTÉ. Le SOURCER se
# borne à définir constantes et fonctions ci-dessus : aucun service arrêté,
# aucune construction, aucune écriture.
# ---------------------------------------------------------------------------
main() {
set -euo pipefail

# ---------------------------------------------------------------------------
# Validation du contexte de résolution (récupère la garantie que set -e donnait
# avant son déplacement dans main). Refuse AVANT toute action une résolution
# vide OU qui réussit mais pointe hors du dépôt. Marqueur non ambigu : le
# Cargo.toml du workspace (racine gradatum, section [workspace]) — pas un simple
# test -d qui accepterait n'importe quel répertoire.
# ---------------------------------------------------------------------------
if [[ -z "$PROJECT_DIR" ]]; then
    fail "Résolution du répertoire projet échouée (PROJECT_DIR vide) — lancer 'bash scripts/deploy-gradatum-local.sh' depuis le dépôt gradatum cloné (ou via le symlink ~/scripts/)"
fi
if [[ ! -f "${PROJECT_DIR}/Cargo.toml" ]] || ! grep -q '^\[workspace\]' "${PROJECT_DIR}/Cargo.toml"; then
    fail "PROJECT_DIR='${PROJECT_DIR}' ne contient pas le Cargo.toml du workspace gradatum (marqueur [workspace] absent) — répertoire projet mal résolu, deploy annulé. Lancer le script depuis le dépôt gradatum."
fi

# ---------------------------------------------------------------------------
# Flags
# ---------------------------------------------------------------------------

FLAG_BUILD=false
FLAG_REBASELINE=false
FLAG_DRY_RUN=false
FLAG_ENGINE=false
FLAG_GATEWAY=false

for arg in "$@"; do
    case "$arg" in
        --build)                 FLAG_BUILD=true ;;
        --rebaseline-migrations) FLAG_REBASELINE=true ;;
        --gateway)               FLAG_GATEWAY=true ;;
        --engine)                FLAG_ENGINE=true ;;
        --dry-run)               FLAG_DRY_RUN=true ;;
        *)
            echo "ERREUR: flag inconnu '$arg'" >&2
            echo "Usage: $0 [--build] [--rebaseline-migrations] [--gateway] [--engine] [--dry-run]" >&2
            exit 1
            ;;
    esac
done

# Périmètre engine résolu en 0b-bis depuis le manifeste (init inconditionnelle : le
# rollback et le résumé les lisent sous set -u même quand --engine est absent).
ENGINE_LOCAL_UNITS=()   # unités engine sur l'hôte local (à stopper/démarrer localement)
ENGINE_REMOTE=()        # entrées "host|unit" des engines distants (manifeste)
ENGINE_HOSTS_REMOTE=()  # hôtes distants uniques recevant le binaire engine

# Ajouter gateway/engine aux binaires si demandé — utilisés pour build, build_sha
# check, backup et install LOCAUX. Leur install dir diffère (OPT_INSTALL_DIR).
if $FLAG_GATEWAY; then
    BINARIES+=("gradatum-gateway")
fi
if $FLAG_ENGINE; then
    BINARIES+=("gradatum-engine")
fi

# ---------------------------------------------------------------------------
# Chrono
# ---------------------------------------------------------------------------
START_TIME=$(date +%s)

# Horodatage UNIQUE du run : nomme le répertoire de backup (étape 2), le .bak du binaire
# engine distant et le chemin de transit (0f). Remonté ici parce que la zone de transit
# est écrite AVANT l'étape 2 — un seul run, un seul identifiant, corrélables en journal.
TS=$(date +%Y%m%d_%H%M%S)

# ---------------------------------------------------------------------------
# ÉTAPE 0 — Pré-vol
# ---------------------------------------------------------------------------
info "=== ÉTAPE 0 : PRÉ-VOL ==="

# 0a. Lire la version cible depuis Cargo.toml workspace
VERSION_CIBLE=$(grep -m1 '^version' "${PROJECT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')
if [[ -z "$VERSION_CIBLE" ]]; then
    fail "Impossible de lire la version dans ${PROJECT_DIR}/Cargo.toml"
fi
info "Version cible : ${VERSION_CIBLE}"
if $FLAG_GATEWAY; then
    info "Mode           : + gateway (hôte local, /opt/gradatum/bin)"
fi
if $FLAG_ENGINE; then
    info "Mode           : + engine (manifeste multi-hôtes)"
fi

# 0b. Vérifier les prérequis
for cmd in sqlite3 sha384sum jq curl systemctl sudo git; do
    command -v "$cmd" >/dev/null 2>&1 || fail "Prérequis manquant : $cmd"
done
info "Prérequis : OK (sqlite3, sha384sum, jq, curl, systemctl, sudo, git)"

# Prérequis du périmètre étendu (guardés : n'altèrent pas la ligne de base ci-dessus).
# objdump : mesure du plancher glibc. ssh/scp : opérations sur les hôtes distants.
if $FLAG_GATEWAY || $FLAG_ENGINE; then
    command -v objdump >/dev/null 2>&1 || fail "Prérequis (--gateway/--engine) manquant : objdump"
fi
if $FLAG_ENGINE; then
    for cmd in ssh scp; do
        command -v "$cmd" >/dev/null 2>&1 || fail "Prérequis (--engine) manquant : $cmd"
    done
    info "Prérequis étendus : OK (objdump, ssh, scp)"
elif $FLAG_GATEWAY; then
    info "Prérequis étendus : OK (objdump)"
fi

# 0b-bis. Vérifier l'EXISTENCE des unités du périmètre étendu (manifeste = autorité).
# Remplace l'ancienne DÉCOUVERTE par motif `gradatum-engine@*` : sur un hôte aux unités
# PLATES ce motif rendait zéro unité → rien à redémarrer, aucun signalement (piège #1).
# Ici, l'absence
# d'une unité ATTENDUE est FATALE et nomme l'unité ET l'hôte. Aussi : peuple
# ENGINE_LOCAL_UNITS / ENGINE_REMOTE / ENGINE_HOSTS_REMOTE pour les étapes suivantes.
if $FLAG_GATEWAY; then
    host_reachable "local" || fail "Hôte local injoignable — impossible"
    if unit_exists "local" "$GATEWAY_UNIT"; then
        info "  Unité présente : ${GATEWAY_UNIT} (local)"
    else
        fail "Unité ATTENDUE absente : ${GATEWAY_UNIT} sur l'hôte local — installer l'unité (packaging/systemd/) avant deploy. Deploy annulé plutôt qu'un no-op silencieux."
    fi
fi
if $FLAG_ENGINE; then
    # Le manifeste est chargé ICI, juste avant sa première lecture : sa provenance est
    # nommée dans le journal du deploy, au même endroit que les unités qu'il déclare.
    engine_units_load
    # Sonde de joignabilité par hôte distant AVANT les tests d'existence : distingue
    # « hôte injoignable » (réseau/ssh) d'« unité absente » (message non ambigu).
    for entry in "${ENGINE_UNITS[@]}"; do
        ehost="${entry%%|*}"
        [[ "$ehost" == "local" ]] && continue
        case " ${ENGINE_HOSTS_REMOTE[*]:-} " in
            *" $ehost "*) : ;;
            *)
                host_reachable "$ehost" || fail "Hôte engine distant injoignable : ${ehost} (ssh ${SSH_OPTS[*]}) — vérifier réseau/clé. Deploy annulé."
                ENGINE_HOSTS_REMOTE+=("$ehost")
                ;;
        esac
    done
    for entry in "${ENGINE_UNITS[@]}"; do
        ehost="${entry%%|*}"
        eunit="${entry##*|}"
        if unit_exists "$ehost" "$eunit"; then
            info "  Unité présente : ${eunit} (${ehost})"
        else
            fail "Unité ATTENDUE absente : ${eunit} sur l'hôte ${ehost} — déclarée au manifeste F-173 mais introuvable (renommée/non installée). Deploy annulé plutôt qu'un no-op silencieux."
        fi
        if [[ "$ehost" == "local" ]]; then
            ENGINE_LOCAL_UNITS+=("$eunit")
        else
            ENGINE_REMOTE+=("$entry")
        fi
    done
fi

# 0c. Build si demandé
if $FLAG_BUILD; then
    info "Build release demandé..."

    # Build principal (server + worker + admin CLI)
    dry cargo build --release \
        -p gradatum-server \
        -p gradatum-worker \
        -p gradatum-admin \
        --manifest-path "${PROJECT_DIR}/Cargo.toml"

    # Build gateway (features par défaut — le crate n'en requiert aucun)
    if $FLAG_GATEWAY; then
        dry cargo build --release \
            -p gradatum-gateway \
            --manifest-path "${PROJECT_DIR}/Cargo.toml"
    fi

    # Build engine (séparé — nécessite le feature 'serve')
    if $FLAG_ENGINE; then
        dry cargo build --release \
            -p gradatum-engine --features serve \
            --manifest-path "${PROJECT_DIR}/Cargo.toml"
    fi
fi

# 0d. Vérifier que les binaires target/release sont bien ceux du commit courant
#
# CE SCRIPT NE BUILD PAS (hors --build) : il copie target/release/* tels quels.
# Si la session a tourné en profil dev, target/release reste sur le build
# précédent → deploy silencieux d'un binaire ancien.
# La version sémantique seule NE détecte PAS ce cas : elle est identique sur
# des dizaines de commits consécutifs. Le build_sha, lui, identifie le commit.
info "Vérification des binaires target/release..."

# Commit de référence = HEAD du dépôt au moment du deploy.
# Même commande que celle des build.rs des deux crates (`rev-parse --short HEAD`)
# → même longueur d'abréviation, comparaison textuelle exacte et sans normalisation.
COMMIT_REF=$(git -C "${PROJECT_DIR}" rev-parse --short HEAD 2>/dev/null || echo "")
if [[ -z "$COMMIT_REF" ]]; then
    fail "Impossible de résoudre HEAD dans ${PROJECT_DIR} — le contrôle build_sha ne peut pas s'exécuter, deploy annulé"
fi
info "Commit de référence (HEAD) : ${COMMIT_REF}"

# Le build_sha atteste du COMMIT, pas de l'arbre de travail : un binaire construit
# sur un arbre modifié non commité porte quand même le SHA de HEAD.
if [[ -n "$(git -C "${PROJECT_DIR}" status --porcelain 2>/dev/null)" ]]; then
    warn "Arbre de travail non propre — build_sha atteste du commit HEAD, PAS des modifications non commitées"
fi

# --dry-run + --build : le build a été affiché, pas exécuté. Les binaires présents
# sont ceux d'avant, les rejeter masquerait le reste du plan. Écarts en WARN.
# --dry-run seul : ce sont exactement les binaires qui partiraient → FAIL.
CHECK_FATAL=true
if $FLAG_DRY_RUN && $FLAG_BUILD; then
    CHECK_FATAL=false
    info "  (dry-run + --build : build non exécuté → écarts signalés, non bloquants)"
fi

# Échec de contrôle : fatal en temps normal, dégradé en avertissement dans le
# seul cas où le build attendu n'a délibérément pas eu lieu (dry-run + --build).
check_bin_fail() {
    if $CHECK_FATAL; then
        fail "$*"
    else
        warn "[DRY-RUN] écart ignoré : $*"
    fi
}

for bin in "${BINARIES[@]}"; do
    local_path="${PROJECT_DIR}/target/release/${bin}"
    if [[ ! -x "$local_path" ]]; then
        fail "Binaire absent ou non exécutable : ${local_path} — lancer avec --build"
    fi

    # Contrat de sortie (documenté dans les main.rs respectifs, format stable) :
    #   "<nom_binaire> <semver> (build_sha <sha>)"
    ver_line=$("${local_path}" --version 2>/dev/null || echo "")

    # Version : premier motif sémantique X.Y.Z[-pre] de la ligne.
    # On cherche « le numéro de version », pas « le champ n° 2 » → insensible à
    # tout suffixe ajouté ultérieurement au format.
    bin_ver=$(printf '%s\n' "$ver_line" \
        | grep -oE '[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?' | head -n1 || echo "")

    # SHA de build : contenu du groupe « (build_sha …) ».
    # Vide si le binaire est antérieur à l'exposition du SHA → écart, pas succès.
    bin_sha=$(printf '%s\n' "$ver_line" | sed -nE 's/.*\(build_sha ([^)]+)\).*/\1/p')

    if [[ "$bin_ver" != "$VERSION_CIBLE" ]]; then
        check_bin_fail "Version binaire ${bin} = '${bin_ver}' ≠ version cible '${VERSION_CIBLE}' — relancer avec --build (sortie brute : '${ver_line}')"
        continue
    fi

    if [[ -z "$bin_sha" ]]; then
        check_bin_fail "Binaire ${bin} n'expose aucun build_sha (sortie brute : '${ver_line}') — build antérieur à l'exposition du SHA, le commit déployé n'est pas prouvable. Relancer avec --build"
        continue
    fi

    # Repli du build.rs quand le SHA n'a pas pu être résolu (pas de .git, tarball).
    # Ce n'est PAS une correspondance : c'est l'absence de preuve.
    if [[ "$bin_sha" == "unknown" ]]; then
        check_bin_fail "Binaire ${bin} : build_sha = 'unknown' (construit hors dépôt git) — commit non prouvable. Relancer avec --build depuis ${PROJECT_DIR}"
        continue
    fi

    if [[ "$bin_sha" != "$COMMIT_REF" ]]; then
        check_bin_fail "Binaire ${bin} construit au commit '${bin_sha}' ≠ HEAD '${COMMIT_REF}' — binaire périmé (target/release non reconstruit). Relancer avec --build"
        continue
    fi

    info "  ${bin} v${bin_ver} @ ${bin_sha} : OK"
done

# ---------------------------------------------------------------------------
# ÉTAPE 0e — Plancher glibc du périmètre étendu (piège #2, F-173)
# ---------------------------------------------------------------------------
# S'exécute AVANT toute copie (backup étape 2, install étape 4, scp étape 6b). La marge
# peut être NULLE sur un hôte de moteurs — mesuré : le binaire engine exige GLIBC_2.39 et
# l'hôte fournit exactement 2.39. Un futur binaire bâti contre une glibc supérieure serait poussé et ne
# démarrerait plus → ici, la copie est REFUSÉE. Read-only ⇒ actif aussi en dry-run
# (le plan doit pouvoir échouer sur ce contrôle). Le périmètre de base (server/worker/
# admin, local, glibc hôte toujours ≥) n'est pas concerné : contrôle guardé.
if $FLAG_GATEWAY || $FLAG_ENGINE; then
    info "=== ÉTAPE 0e : PLANCHER GLIBC (hôtes cibles) ==="
    if $FLAG_GATEWAY; then
        check_glibc_target "${PROJECT_DIR}/target/release/gradatum-gateway" "local"
    fi
    if $FLAG_ENGINE; then
        # Binaire engine unique poussé sur local + chaque hôte distant du manifeste.
        check_glibc_target "${PROJECT_DIR}/target/release/gradatum-engine" "local"
        for ehost in "${ENGINE_HOSTS_REMOTE[@]:-}"; do
            [[ -z "$ehost" ]] && continue
            check_glibc_target "${PROJECT_DIR}/target/release/gradatum-engine" "$ehost"
        done
    fi
fi

# ---------------------------------------------------------------------------
# ÉTAPE 0f — VALIDATION EN ZONE DE TRANSIT (F-173) — AVANT TOUTE SUBSTITUTION
# ---------------------------------------------------------------------------
# Placée ici, en fin de pré-vol, parce que c'est le dernier point où RIEN n'a encore été
# muté : ni binaire local (étape 4), ni service arrêté (étape 3), ni binaire distant (6b).
# Un refus à cette étape laisse donc le parc ENTIER dans son état d'origine — engine,
# server, worker et gateway compris, aucun n'ayant été installé. C'est ce qui autorise à
# sortir par `fail` plutôt que par le filet de restauration : il n'y a rien à restaurer.
#
# Deux périmètres, même logique :
#   - LOCAL   : l'artefact ${PROJECT_DIR}/target/release/gradatum-engine EST déjà hors du
#               chemin LIVE (/opt/gradatum/bin) — il est sa propre zone de transit, aucune
#               copie n'est nécessaire.
#   - DISTANT : le binaire est déposé dans ENGINE_STAGE_PREFIX.<TS>, validé SUR PLACE,
#               puis c'est CE MÊME exemplaire que 6b substitue — jamais une copie fraîche,
#               qui rouvrirait l'écart « on valide un exemplaire, on installe l'autre ».
#
# En --dry-run : la résolution des chemins de configuration (lecture systemd) et la
# validation LOCALE sont EXERCÉES pour de bon — sans effet de bord, et c'est là que le
# plan gagne son pouvoir discriminant. Le dépôt distant, lui, est décrit et non exécuté :
# --dry-run ne mute rien, pas même le /tmp d'un hôte de production.
if $FLAG_ENGINE; then
    info "=== ÉTAPE 0f : VALIDATION EN ZONE DE TRANSIT (engine) ==="
    ENGINE_SRC="${PROJECT_DIR}/target/release/gradatum-engine"
    ENGINE_STAGE_PATH="${ENGINE_STAGE_PREFIX}.${TS}"

    if [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
        info "  --- local : ${ENGINE_SRC} (artefact de build = zone de transit) ---"
        engine_validate_transit "local" "$ENGINE_SRC" "${ENGINE_LOCAL_UNITS[@]}"
    fi

    if [[ ${#ENGINE_REMOTE[@]} -gt 0 ]]; then
        # Armé AVANT le premier dépôt : à partir d'ici, tout chemin de sortie — succès,
        # `fail`, interruption — repasse par le nettoyage du transit.
        trap engine_stage_cleanup EXIT
        for ehost in "${ENGINE_HOSTS_REMOTE[@]}"; do
            info "  --- ${ehost} : dépôt en transit → ${ENGINE_STAGE_PATH} ---"
            if $FLAG_DRY_RUN; then
                echo "[DRY-RUN] scp ${ENGINE_SRC} → ${ehost}:${ENGINE_STAGE_PATH}  (zone de transit, PAS le chemin LIVE)"
                echo "[DRY-RUN]   non exécuté : --dry-run ne mute rien, pas même le /tmp d'un hôte distant"
            else
                scp "${SSH_OPTS[@]}" "$ENGINE_SRC" "${ehost}:${ENGINE_STAGE_PATH}" >/dev/null \
                    || fail "TRANSIT [${ehost}] : dépôt de ${ENGINE_SRC} vers ${ENGINE_STAGE_PATH} échoué — aucune substitution tentée, parc INTACT."
                # Enregistré IMMÉDIATEMENT après le dépôt réussi : le nettoyage doit
                # connaître ce chemin même si le chmod qui suit échoue.
                ENGINE_STAGED+=("${ehost}|${ENGINE_STAGE_PATH}")
                # shellcheck disable=SC2029  # expansion client-side voulue (voir unit_exists)
                ssh "${SSH_OPTS[@]}" "$ehost" "chmod 0755 '${ENGINE_STAGE_PATH}'" \
                    || fail "TRANSIT [${ehost}] : le binaire de transit ${ENGINE_STAGE_PATH} n'a pas pu être rendu exécutable — aucune substitution tentée, parc INTACT."
            fi

            # Unités servies par CET hôte : c'est leur configuration à elles qui doit
            # valider, pas celle d'un autre hôte (les fichiers diffèrent d'un hôte à
            # l'autre sous le même nom).
            stage_units=()
            for entry in "${ENGINE_REMOTE[@]}"; do
                [[ "${entry%%|*}" == "$ehost" ]] || continue
                stage_units+=("${entry##*|}")
            done
            engine_validate_transit "$ehost" "$ENGINE_STAGE_PATH" "${stage_units[@]}"
        done
    fi
    info "  Validation en zone de transit : OK — substitution autorisée"
fi

# ---------------------------------------------------------------------------
# ÉTAPE 1 — Pré-check intégrité migrations sqlx (LA leçon v0.3.7)
# ---------------------------------------------------------------------------
info "=== ÉTAPE 1 : CONTRÔLE INTÉGRITÉ MIGRATIONS SQLX ==="
info "  Algorithme : SHA-384 (48 bytes) — même méthode que sqlx::migrate!"
info "  RÈGLE : ne jamais modifier une migration déjà appliquée en DB."
info "  Si seuls des commentaires/whitespace ont changé → --rebaseline-migrations (sûr)."
info "  Si le DDL a changé → créer une NOUVELLE migration (ne pas re-baseline)."

MIGRATIONS_OK=true
# Tableau des divergences : format "db_path|version|source_hex|db_hex"
DIVERGENCES=()

for spec in "${SQLX_MIGRATION_SPECS[@]}"; do
    mig_rel="${spec%%:*}"
    db_path="${spec##*:}"
    mig_dir="${PROJECT_DIR}/${mig_rel}"

    info "--- ${mig_rel} → ${db_path}"

    # Vérifier si la DB est accessible via sudo
    if ! sudo test -f "$db_path" 2>/dev/null; then
        info "  DB absente (${db_path}) — skip"
        continue
    fi

    # Vérifier que la table _sqlx_migrations existe
    has_table=$(sudo sqlite3 "$db_path" \
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations';" 2>/dev/null || echo "0")
    if [[ "$has_table" -eq 0 ]]; then
        info "  Pas de table _sqlx_migrations — skip"
        continue
    fi

    # Lire les migrations appliquées (version + checksum hex uppercase)
    while IFS='|' read -r db_version db_checksum_hex; do
        [[ -z "$db_version" ]] && continue

        # Trouver le fichier source correspondant à cette version.
        # sqlx dérive la version depuis le préfixe numérique du nom de fichier.
        # Ex: "20260506000001_create_api_keys.sql" → version 20260506000001
        # Ex: "006_apalis_bootstrap.sql"           → version 6
        source_file=""
        while IFS= read -r f; do
            fname=$(basename "$f")
            file_ver="${fname%%_*}"
            # Supprimer les zéros de tête pour les petits entiers (006 → 6)
            file_ver_stripped=$(echo "$file_ver" | sed 's/^0*//' || echo "$file_ver")
            if [[ "$file_ver" == "$db_version" ]] || [[ "$file_ver_stripped" == "$db_version" ]]; then
                source_file="$f"
                break
            fi
        done < <(find "$mig_dir" -name "*.sql" | sort)

        if [[ -z "$source_file" ]]; then
            warn "  v${db_version} : fichier source absent dans ${mig_dir} — skip"
            continue
        fi

        # Calculer SHA-384 du fichier source (identique à la méthode sqlx)
        source_hex=$(sha384sum "$source_file" | awk '{print toupper($1)}')
        db_hex_upper=$(echo "$db_checksum_hex" | tr '[:lower:]' '[:upper:]')

        if [[ "$source_hex" == "$db_hex_upper" ]]; then
            info "  v${db_version} : OK  ($(basename "$source_file"))"
        else
            MIGRATIONS_OK=false
            DIVERGENCES+=("${db_path}|${db_version}|${source_hex}|${db_hex_upper}")
            warn "  v${db_version} : DIVERGENCE  ($(basename "$source_file"))"
            warn "    Source SHA-384 : ${source_hex}"
            warn "    DB checksum    : ${db_hex_upper}"
        fi

    done < <(sudo sqlite3 "$db_path" \
        "SELECT version, hex(checksum) FROM _sqlx_migrations ORDER BY version;" 2>/dev/null)
done

# Traitement des divergences détectées
if ! $MIGRATIONS_OK; then
    warn ""
    warn "=== ${#DIVERGENCES[@]} DIVERGENCE(S) DÉTECTÉE(S) ==="
    for div in "${DIVERGENCES[@]}"; do
        IFS='|' read -r d_path d_ver d_src d_db <<< "$div"
        warn "  DB=${d_path}  version=${d_ver}"
    done

    if $FLAG_REBASELINE; then
        warn ""
        warn "Re-baseline demandée (--rebaseline-migrations)."
        warn "AVERTISSEMENT : sûr UNIQUEMENT si le DDL est identique"
        warn "(seuls les commentaires / whitespace ont changé)."
        warn "Si le DDL a changé → CTRL+C et créer une nouvelle migration."
        warn ""

        if $FLAG_DRY_RUN; then
            for div in "${DIVERGENCES[@]}"; do
                IFS='|' read -r d_path d_ver d_src d_db <<< "$div"
                echo "[DRY-RUN] UPDATE _sqlx_migrations SET checksum=X'${d_src,,}' WHERE version=${d_ver}; -- ${d_path}"
            done
        else
            # Backup des DBs concernées avant la mutation
            TS_REB=$(date +%Y%m%d_%H%M%S)
            REBASELINE_BACKUP="${BACKUP_BASE}/gradatum-rebaseline-${TS_REB}"
            mkdir -p "$REBASELINE_BACKUP"
            for div in "${DIVERGENCES[@]}"; do
                IFS='|' read -r d_path d_ver d_src d_db <<< "$div"
                db_name=$(basename "$d_path")
                sudo cp "$d_path" "${REBASELINE_BACKUP}/${db_name}"
                info "Backup re-baseline : ${REBASELINE_BACKUP}/${db_name}"
            done

            # Appliquer le re-baseline
            for div in "${DIVERGENCES[@]}"; do
                IFS='|' read -r d_path d_ver d_src d_db <<< "$div"
                blob_hex="${d_src,,}"  # lowercase pour sqlite3 X'...'
                sudo sqlite3 "$d_path" \
                    "UPDATE _sqlx_migrations SET checksum = X'${blob_hex}' WHERE version = ${d_ver};"
                info "Re-baseline appliquée : DB=${d_path} version=${d_ver}"
            done
        fi
    else
        fail "Deploy annulé — divergences de checksums sqlx détectées.
Options :
  1. Relancer avec --rebaseline-migrations (si seuls commentaires/whitespace ont changé)
  2. Restaurer le fichier source via git checkout (si le source a divergé par erreur)
  3. Créer une nouvelle migration (si le DDL a légitimement changé)"
    fi
fi

info "Migrations sqlx : contrôle terminé"

# ---------------------------------------------------------------------------
# ÉTAPE 2 — Backup horodaté (binaires LIVE + DBs sqlx)
# ---------------------------------------------------------------------------
info "=== ÉTAPE 2 : BACKUP ==="
BACKUP_DIR="${BACKUP_BASE}/gradatum-deploy-${TS}"
info "Répertoire backup : ${BACKUP_DIR}"

if $FLAG_DRY_RUN; then
    echo "[DRY-RUN] mkdir -p ${BACKUP_DIR}"
    for bin in "${BINARIES[@]}"; do
        idir="$(install_dir_for "$bin")"
        echo "[DRY-RUN] cp -a ${idir}/${bin} → ${BACKUP_DIR}/${bin}"
    done
    for spec in "${SQLX_MIGRATION_SPECS[@]}"; do
        db_path="${spec##*:}"
        echo "[DRY-RUN] cp -a ${db_path} → ${BACKUP_DIR}/$(basename "${db_path}")"
    done
else
    mkdir -p "$BACKUP_DIR"

    # Binaires actuellement déployés
    for bin in "${BINARIES[@]}"; do
        idir="$(install_dir_for "$bin")"
        bin_live="${idir}/${bin}"
        if [[ -f "$bin_live" ]]; then
            cp -a "$bin_live" "${BACKUP_DIR}/${bin}"
            info "  Binaire sauvegardé : ${bin_live}"
        else
            # Engine peut ne pas encore exister sur l'hôte (premier deploy --engine)
            if [[ "$bin" == "gradatum-engine" ]]; then
                info "  Binaire engine absent (${bin_live}) — premier deploy engine, skip backup"
            fi
        fi
    done

    # DBs sqlx (avec WAL si présents).
    # index.db (vault) est absent de cette liste À DESSEIN — il est sauvegardé
    # par ExecStartPre au redémarrage de l'étape 5, au plus près des migrations
    # rusqlite qui le réécrivent. Justification complète en en-tête du script.
    for spec in "${SQLX_MIGRATION_SPECS[@]}"; do
        db_path="${spec##*:}"
        if sudo test -f "$db_path" 2>/dev/null; then
            db_name=$(basename "$db_path")
            sudo cp "$db_path" "${BACKUP_DIR}/${db_name}"
            sudo test -f "${db_path}-wal" 2>/dev/null && sudo cp "${db_path}-wal" "${BACKUP_DIR}/${db_name}-wal" || true
            sudo test -f "${db_path}-shm" 2>/dev/null && sudo cp "${db_path}-shm" "${BACKUP_DIR}/${db_name}-shm" || true
            info "  DB sauvegardée : ${db_path}"
        fi
    done
fi

# ---------------------------------------------------------------------------
# ÉTAPE 3 — Arrêt des services
# ---------------------------------------------------------------------------
info "=== ÉTAPE 3 : ARRÊT SERVICES (engine instances → worker → server) ==="

# Arrêter les instances engine LOCALES d'abord (elles ne dépendent pas du serveur,
# mais les arrêter avant évite des appels vers un serveur qui va s'arrêter).
# Les engines DISTANTS ne sont PAS arrêtés ici : ils sont redémarrés un à
# un avec porte de santé à l'étape 6b, après confirmation du health local.
if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
    for unit in "${ENGINE_LOCAL_UNITS[@]}"; do
        info "  Stop : ${unit}"
        dry sudo systemctl stop "$unit"
    done
fi

# Arrêter la passerelle locale (démarrage indépendant : pas de dépendance d'ordre
# avec le serveur — voir piège #4, l'unité évite délibérément After=gradatum-server).
if $FLAG_GATEWAY; then
    info "  Stop : ${GATEWAY_UNIT}"
    dry sudo systemctl stop "$GATEWAY_UNIT"
fi

for unit in "${UNITS_STOP_ORDER[@]}"; do
    info "  Stop : ${unit}"
    dry sudo systemctl stop "$unit"
done

# Attendre l'arrêt effectif
if ! $FLAG_DRY_RUN; then
    sleep 1
    for unit in "${UNITS_STOP_ORDER[@]}"; do
        if sudo systemctl is-active --quiet "$unit" 2>/dev/null; then
            warn "  ${unit} encore actif — attente 3s..."
            sleep 3
        else
            info "  ${unit} : arrêté"
        fi
    done
fi

# ---------------------------------------------------------------------------
# ÉTAPE 4 — Copie des nouveaux binaires
# ---------------------------------------------------------------------------
info "=== ÉTAPE 4 : INSTALLATION BINAIRES ==="

# Créer le répertoire /opt/gradatum/bin si premier deploy gateway/engine
if ( $FLAG_ENGINE || $FLAG_GATEWAY ) && ! $FLAG_DRY_RUN; then
    sudo mkdir -p "$OPT_INSTALL_DIR"
    sudo chown gradatum:gradatum "$OPT_INSTALL_DIR" 2>/dev/null || true
    sudo chmod 0755 "$OPT_INSTALL_DIR"
fi

for bin in "${BINARIES[@]}"; do
    src="${PROJECT_DIR}/target/release/${bin}"
    idir="$(install_dir_for "$bin")"
    dst="${idir}/${bin}"
    info "  ${src} → ${dst}"
    dry sudo install -m 0755 -o root -g root "$src" "$dst"
done

# ---------------------------------------------------------------------------
# ÉTAPE 5 — Démarrage des services
# ---------------------------------------------------------------------------
info "=== ÉTAPE 5 : DÉMARRAGE SERVICES (server → worker → engine instances) ==="
for unit in "${UNITS_START_ORDER[@]}"; do
    info "  Start : ${unit}"
    dry sudo systemctl start "$unit"
done

# Démarrer la passerelle locale (indépendante du serveur — piège #4).
if $FLAG_GATEWAY; then
    info "  Start : ${GATEWAY_UNIT}"
    dry sudo systemctl start "$GATEWAY_UNIT"
fi

# Démarrer les instances engine LOCALES (existence garantie en 0b-bis).
if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
    for unit in "${ENGINE_LOCAL_UNITS[@]}"; do
        info "  Start : ${unit}"
        dry sudo systemctl start "$unit"
    done
fi

# ---------------------------------------------------------------------------
# ÉTAPE 6 — Vérification health (rollback automatique sur échec)
# ---------------------------------------------------------------------------
info "=== ÉTAPE 6 : VÉRIFICATION HEALTH ==="

if $FLAG_DRY_RUN; then
    echo "[DRY-RUN] Attente health ${HEALTH_URL} → .version == ${VERSION_CIBLE} ET .build_sha == ${COMMIT_REF}"
    echo "[DRY-RUN] Échec → rollback automatique depuis ${BACKUP_DIR}"
else
    info "Attente démarrage (max ${HEALTH_TIMEOUT_SECS}s)..."
    elapsed=0
    health_ok=false

    while [[ $elapsed -lt $HEALTH_TIMEOUT_SECS ]]; do
        http_code=$(curl -s -o /dev/null -w "%{http_code}" "$HEALTH_URL" 2>/dev/null || echo "000")
        if [[ "$http_code" == "200" ]]; then
            health_ok=true
            break
        fi
        sleep $HEALTH_POLL_INTERVAL
        elapsed=$((elapsed + HEALTH_POLL_INTERVAL))
    done

    if ! $health_ok; then
        warn "Health endpoint non répondu après ${HEALTH_TIMEOUT_SECS}s — ROLLBACK"
        rollback "$BACKUP_DIR"
        fail "Deploy échoué : health timeout. Rollback effectué depuis ${BACKUP_DIR}"
    fi

    # Une seule lecture du payload : version et build_sha doivent décrire le MÊME
    # processus (deux curl successifs pourraient tomber de part et d'autre d'un restart).
    health_json=$(curl -s "$HEALTH_URL" 2>/dev/null || echo "")

    live_ver=$(printf '%s' "$health_json" | jq -r '.version // empty' 2>/dev/null || echo "")
    if [[ "$live_ver" != "$VERSION_CIBLE" ]]; then
        warn "Version LIVE '${live_ver}' ≠ cible '${VERSION_CIBLE}' — ROLLBACK"
        rollback "$BACKUP_DIR"
        fail "Deploy échoué : version inattendue. Rollback depuis ${BACKUP_DIR}"
    fi

    # Preuve que le processus RÉELLEMENT en vie est celui du commit contrôlé en 0d
    # (et non un binaire resté en place parce que l'install a échoué en silence).
    live_sha=$(printf '%s' "$health_json" | jq -r '.build_sha // empty' 2>/dev/null || echo "")
    if [[ "$live_sha" != "$COMMIT_REF" ]]; then
        warn "build_sha LIVE '${live_sha}' ≠ commit de référence '${COMMIT_REF}' — ROLLBACK"
        rollback "$BACKUP_DIR"
        fail "Deploy échoué : le serveur LIVE ne tourne pas le commit attendu. Rollback depuis ${BACKUP_DIR}"
    fi

    info "Health : OK — version ${live_ver} @ ${live_sha} LIVE"
fi

# ---------------------------------------------------------------------------
# ÉTAPE 6b — Engines DISTANTS : copie + redémarrage séquentiel
# ---------------------------------------------------------------------------
# Exécutée APRÈS confirmation du health local (étape 6) : si le déploiement local
# échoue, on rollback local et on ne touche JAMAIS un hôte distant. Le binaire engine unique
# (build_sha contrôlé en 0d, plancher glibc en 0e, exécution réelle sur l'hôte et
# validation de CHAQUE configuration servie en 0f) est SUBSTITUÉ depuis sa zone de
# transit — les causes détectables de non-démarrage ont déjà été écartées AVANT ce point
# de non-retour. Les unités sont ensuite redémarrées UNE À LA FOIS, porte de santé entre
# chaque et
# ARRÊT à la première anomalie (piège #3 — ces moteurs servent la supervision et la
# curation de tout le homelab, jamais les trois en bloc). Cet arrêt n'abandonne PAS le
# parc en l'état : il enchaîne sur engine_abort_restore (restauration distante puis
# locale, sortie non-zéro) — F-173 critère 2, tout-ou-rien.
if $FLAG_ENGINE && [[ ${#ENGINE_REMOTE[@]} -gt 0 ]]; then
    info "=== ÉTAPE 6b : ENGINES DISTANTS ==="
    remote_dst="${OPT_INSTALL_DIR}/gradatum-engine"

    # Substitution depuis la ZONE DE TRANSIT (0f) — et non une nouvelle copie : le binaire
    # installé ici est EXACTEMENT l'exemplaire qui a prouvé son exécution sur cet hôte et
    # validé toutes ses configurations. Backup .bak-<TS> avant écrasement (guard-data-loss ;
    # les unités étant actives, le binaire cible existe). Le chemin de transit n'est PAS
    # retiré ici : engine_stage_cleanup (trap EXIT) s'en charge dans tous les cas — un
    # `rm` en ligne serait sauté par le premier `fail`.
    for ehost in "${ENGINE_HOSTS_REMOTE[@]}"; do
        remote_bak="${remote_dst}.bak-${TS}"
        info "  [${ehost}] backup ${remote_dst} → ${remote_bak}"
        dry ssh "${SSH_OPTS[@]}" "$ehost" "sudo cp -a '${remote_dst}' '${remote_bak}'"
        info "  [${ehost}] substitution ${ENGINE_STAGE_PATH} (validé en 0f) → ${remote_dst}"
        dry ssh "${SSH_OPTS[@]}" "$ehost" "sudo install -m 0755 -o root -g root '${ENGINE_STAGE_PATH}' '${remote_dst}'"
    done

    # Redémarrage séquentiel avec porte de santé — arrêt à la première anomalie.
    info "  --- Redémarrage séquentiel (un moteur à la fois, porte de santé) ---"
    for eidx in "${!ENGINE_REMOTE[@]}"; do
        entry="${ENGINE_REMOTE[$eidx]}"
        ehost="${entry%%|*}"
        eunit="${entry##*|}"
        info "  [${ehost}] restart ${eunit}"
        dry ssh "${SSH_OPTS[@]}" "$ehost" "sudo systemctl restart '${eunit}'"
        if $FLAG_DRY_RUN; then
            echo "[DRY-RUN] porte santé ${eunit}@${ehost} : is-active == active stable ${ENGINE_HEALTH_STABLE_SECS}s (max ${ENGINE_HEALTH_TIMEOUT_SECS}s)"
            echo "[DRY-RUN]   anomalie ⇒ moteurs suivants NON redémarrés, PUIS restauration TOUT-OU-RIEN : binaire d'origine rendu à ${ENGINE_HOSTS_REMOTE[*]} et moteurs déjà redémarrés relancés dessus, puis binaires + unités LOCALES restaurés depuis ${BACKUP_DIR}, puis sortie non-zéro. Le parc ne reste pas mixte."
            echo "[DRY-RUN]   (une configuration non servable, elle, a déjà été refusée en 0f — avant toute substitution, donc sans rien à restaurer)"
            continue
        fi
        if engine_health_gate "$ehost" "$eunit"; then
            info "  [${ehost}] ${eunit} : actif (stable) OK"
        else
            warn "  [${ehost}] ${eunit} : NON sain après redémarrage"
            # Restauration TOUT-OU-RIEN (F-173 critère 2). Toute la logique vit hors du
            # chemin nominal : engine_abort_restore restaure le distant (tous les hôtes
            # copiés) puis le local (rollback()), et NE REND JAMAIS LA MAIN — elle se
            # termine par `fail`, en énonçant l'état final réellement observé.
            engine_abort_restore "$eidx" "$ehost" "$eunit" \
                "$remote_dst" "${remote_dst}.bak-${TS}" "$BACKUP_DIR"
        fi
    done
    info "  Engines distants : tous redémarrés et sains (${#ENGINE_REMOTE[@]} unités)"
fi

# ---------------------------------------------------------------------------
# ÉTAPE 7 — Smoke (best-effort, rappel opérateur)
# ---------------------------------------------------------------------------
info "=== ÉTAPE 7 : SMOKE (rappel opérateur) ==="
info "  Le round-trip vault_write → vault_read → vault_search se vérifie via MCP :"
info "    mcp__gradatum__vault_write  { section: debug, title: smoke-deploy-v${VERSION_CIBLE} }"
info "    mcp__gradatum__vault_read   { note_id: <ulid retourné> }"
info "    mcp__gradatum__vault_search { query: smoke-deploy-v${VERSION_CIBLE} }"

# ---------------------------------------------------------------------------
# ÉTAPE 8 — Résumé
# ---------------------------------------------------------------------------
info "=== RÉSUMÉ ==="
END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))
DIV_COUNT=${#DIVERGENCES[@]}

if $FLAG_DRY_RUN; then
    info "DRY-RUN terminé — aucune mutation effectuée"
    info "  Version cible     : ${VERSION_CIBLE}"
    info "  Commit cible      : ${COMMIT_REF}"
    info "  Divergences sqlx  : ${DIV_COUNT}"
    info "  Engine            : $FLAG_ENGINE"
    if $FLAG_GATEWAY; then
        info "  Gateway           : $FLAG_GATEWAY"
    fi
    if $FLAG_ENGINE && [[ ${#ENGINE_REMOTE[@]} -gt 0 ]]; then
        info "  Engines distants  : ${#ENGINE_REMOTE[@]} unités"
    fi
    if [[ $DIV_COUNT -gt 0 ]]; then
        info "  → Re-baseline requise : relancer avec --rebaseline-migrations"
    fi
else
    info "  Version LIVE      : ${VERSION_CIBLE}"
    info "  Commit LIVE       : ${COMMIT_REF}"
    info "  Services          : gradatum-server=$(sudo systemctl is-active gradatum-server 2>/dev/null) / gradatum-worker=$(sudo systemctl is-active gradatum-worker 2>/dev/null)"
    if $FLAG_GATEWAY; then
        info "  Gateway           : ${GATEWAY_UNIT}=$(sudo systemctl is-active "$GATEWAY_UNIT" 2>/dev/null)"
    fi
    if $FLAG_ENGINE && [[ ${#ENGINE_LOCAL_UNITS[@]} -gt 0 ]]; then
        for unit in "${ENGINE_LOCAL_UNITS[@]}"; do
            info "  Engine (local)    : ${unit}=$(sudo systemctl is-active "$unit" 2>/dev/null)"
        done
    fi
    if $FLAG_ENGINE && [[ ${#ENGINE_REMOTE[@]} -gt 0 ]]; then
        for entry in "${ENGINE_REMOTE[@]}"; do
            e_h="${entry%%|*}"; e_u="${entry##*|}"
            info "  Engine (${e_h}) : ${e_u}=$(sc_is_active "$e_h" "$e_u")"
        done
    fi
    info "  Backup            : ${BACKUP_DIR}"
    info "  Durée             : ${DURATION}s"
fi
}

# ---------------------------------------------------------------------------
# Garde de source : main ne s'exécute QUE si le script est lancé, jamais sourcé.
#   exécuté  → "${0}" == "${BASH_SOURCE[0]}"  → main "$@"
#   sourcé   → "${0}" == nom du shell appelant → définitions seules, zéro effet
# ---------------------------------------------------------------------------
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
