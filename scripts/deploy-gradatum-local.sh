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
#   --engine                 Inclure gradatum-engine dans le build/copie/démarrage.
#                            Cherche les instances actives (gradatum-engine@*) et les
#                            redémarre après le deploy server+worker.
#   --dry-run                Affiche le plan sans rien muter
#
# Chemins d'installation :
#   gradatum-server, gradatum-worker → /usr/bin/
#   gradatum-engine                  → /opt/gradatum/bin/  (template systemd)
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
#   (source : scripts/gradatum-pre-migration-backup), exécuté à CHAQUE
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
set -euo pipefail

# ---------------------------------------------------------------------------
# Constantes
# ---------------------------------------------------------------------------

# Résolution symlink-safe : readlink -f suit le symlink vers le vrai fichier.
# Sans ça, invoqué via ~/scripts/deploy-gradatum-local.sh (symlink), dirname
# retourne ~/scripts et PROJECT_DIR pointe sur $HOME au lieu du dépôt.
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Crates avec migrations sqlx (source : grep sqlx::migrate! dans les crates)
# Format : "chemin_relatif_migrations:chemin_db_absolue"
SQLX_MIGRATION_SPECS=(
    "crates/gradatum-acl-auth/migrations:/var/lib/gradatum/db/api_keys.sqlite"
    "crates/gradatum-db-sqlite/migrations:/var/lib/gradatum/db/queue.sqlite"
)

# Services systemd (system, pas user) — arrêt worker d'abord, démarrage server d'abord
UNITS_STOP_ORDER=("gradatum-worker" "gradatum-server")
UNITS_START_ORDER=("gradatum-server" "gradatum-worker")

# Binaires à déployer (liste de base ; engine ajouté dynamiquement si --engine)
BINARIES=("gradatum-server" "gradatum-worker")
INSTALL_DIR="/usr/bin"
ENGINE_INSTALL_DIR="/opt/gradatum/bin"

# Health endpoint
HEALTH_URL="http://127.0.0.1:19090/health"
HEALTH_TIMEOUT_SECS=30
HEALTH_POLL_INTERVAL=2

# Backup
BACKUP_BASE="${HOME}/backups"

# ---------------------------------------------------------------------------
# Flags
# ---------------------------------------------------------------------------

FLAG_BUILD=false
FLAG_REBASELINE=false
FLAG_DRY_RUN=false
FLAG_ENGINE=false

for arg in "$@"; do
    case "$arg" in
        --build)                 FLAG_BUILD=true ;;
        --rebaseline-migrations) FLAG_REBASELINE=true ;;
        --engine)                FLAG_ENGINE=true ;;
        --dry-run)               FLAG_DRY_RUN=true ;;
        *)
            echo "ERREUR: flag inconnu '$arg'" >&2
            echo "Usage: $0 [--build] [--rebaseline-migrations] [--engine] [--dry-run]" >&2
            exit 1
            ;;
    esac
done

# Ajouter engine aux binaires si demandé — utilisé pour build, build_sha check,
# backup et install. L'install dir d'engine diffère (ENGINE_INSTALL_DIR vs INSTALL_DIR).
if $FLAG_ENGINE; then
    BINARIES+=("gradatum-engine")
fi

# ---------------------------------------------------------------------------
# Utilitaires
# ---------------------------------------------------------------------------

log()  { echo "[$(date '+%H:%M:%S')] $*"; }
info() { log "INFO  $*"; }
warn() { log "WARN  $*"; }
fail() { log "FAIL  $*" >&2; exit 1; }

# Exécute la commande, ou l'affiche seulement en dry-run
dry() {
    if $FLAG_DRY_RUN; then
        echo "[DRY-RUN] $*"
    else
        "$@"
    fi
}

# Répertoire d'installation pour un binaire donné.
# Server/worker → /usr/bin, engine → /opt/gradatum/bin.
install_dir_for() {
    local bin="$1"
    case "$bin" in
        gradatum-engine) echo "$ENGINE_INSTALL_DIR" ;;
        *)               echo "$INSTALL_DIR" ;;
    esac
}

# Restaure les binaires depuis le backup et redémarre les services
# Appelé uniquement si le health check échoue après le deploy.
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
    local bdir="$1"
    warn "=== ROLLBACK ==="
    warn "Restauration depuis : ${bdir}"
    for bin in "${BINARIES[@]}"; do
        local bak="${bdir}/${bin}"
        local idir
        idir="$(install_dir_for "$bin")"
        local dst="${idir}/${bin}"
        if [[ -f "$bak" ]]; then
            sudo install -m 0755 -o root -g root "$bak" "$dst"
            warn "  Restauré : ${dst}"
        fi
    done
    for unit in "${UNITS_START_ORDER[@]}"; do
        sudo systemctl start "$unit" 2>/dev/null || true
    done
    # Redémarrer les instances engine si --engine
    if $FLAG_ENGINE; then
        for unit in "${ENGINE_INSTANCES[@]}"; do
            sudo systemctl start "$unit" 2>/dev/null || true
            warn "  Redémarré : ${unit}"
        done
    fi
    warn "Services redémarrés avec binaires rollback"
}

# ---------------------------------------------------------------------------
# Chrono
# ---------------------------------------------------------------------------
START_TIME=$(date +%s)

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
if $FLAG_ENGINE; then
    info "Mode           : server + worker + engine"
fi

# 0b. Vérifier les prérequis
for cmd in sqlite3 sha384sum jq curl systemctl sudo git; do
    command -v "$cmd" >/dev/null 2>&1 || fail "Prérequis manquant : $cmd"
done
info "Prérequis : OK (sqlite3, sha384sum, jq, curl, systemctl, sudo, git)"

# 0b-bis. Découvrir les instances engine actives (avant tout arrêt)
ENGINE_INSTANCES=()
if $FLAG_ENGINE; then
    while IFS= read -r unit; do
        [[ -z "$unit" ]] && continue
        ENGINE_INSTANCES+=("$unit")
    done < <(systemctl list-units --type=service --state=active 'gradatum-engine@*' --no-legend 2>/dev/null | awk '{print $1}' || true)
    if [[ ${#ENGINE_INSTANCES[@]} -gt 0 ]]; then
        info "Instances engine actives : ${ENGINE_INSTANCES[*]}"
    else
        info "Aucune instance engine active (template installé, instances à créer manuellement)"
    fi
fi

# 0c. Build si demandé
if $FLAG_BUILD; then
    info "Build release demandé..."

    # Build principal (server + worker)
    dry cargo build --release \
        -p gradatum-server \
        -p gradatum-worker \
        --manifest-path "${PROJECT_DIR}/Cargo.toml"

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
TS=$(date +%Y%m%d_%H%M%S)
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

# Arrêter les instances engine d'abord (elles ne dépendent pas du serveur,
# mais les arrêter avant évite des appels vers un serveur qui va s'arrêter)
if $FLAG_ENGINE && [[ ${#ENGINE_INSTANCES[@]} -gt 0 ]]; then
    for unit in "${ENGINE_INSTANCES[@]}"; do
        info "  Stop : ${unit}"
        dry sudo systemctl stop "$unit"
    done
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

# Créer le répertoire engine si premier deploy --engine
if $FLAG_ENGINE && ! $FLAG_DRY_RUN; then
    sudo mkdir -p "$ENGINE_INSTALL_DIR"
    sudo chown gradatum:gradatum "$ENGINE_INSTALL_DIR" 2>/dev/null || true
    sudo chmod 0755 "$ENGINE_INSTALL_DIR"
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

# Redémarrer les instances engine actives
if $FLAG_ENGINE && [[ ${#ENGINE_INSTANCES[@]} -gt 0 ]]; then
    for unit in "${ENGINE_INSTANCES[@]}"; do
        info "  Start : ${unit}"
        dry sudo systemctl start "$unit"
    done
elif $FLAG_ENGINE; then
    info "  Engine : aucune instance active à redémarrer"
    info "  (créer une instance : sudo systemctl enable --now gradatum-engine@<nom>)"
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
    if [[ $DIV_COUNT -gt 0 ]]; then
        info "  → Re-baseline requise : relancer avec --rebaseline-migrations"
    fi
else
    info "  Version LIVE      : ${VERSION_CIBLE}"
    info "  Commit LIVE       : ${COMMIT_REF}"
    info "  Services          : gradatum-server=$(sudo systemctl is-active gradatum-server 2>/dev/null) / gradatum-worker=$(sudo systemctl is-active gradatum-worker 2>/dev/null)"
    if $FLAG_ENGINE; then
        for unit in "${ENGINE_INSTANCES[@]}"; do
            info "  Engine            : ${unit}=$(sudo systemctl is-active "$unit" 2>/dev/null)"
        done
    fi
    info "  Backup            : ${BACKUP_DIR}"
    info "  Durée             : ${DURATION}s"
fi
