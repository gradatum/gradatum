#!/usr/bin/env bash
# install-gradatum-services.sh — Installation idempotente des services gradatum sur Linux x86_64.
# (Renamed 2026-05-10 for service-oriented scope clarity — works on any Linux host.)
#
# Usage :
#   sudo bash scripts/install-gradatum-services.sh [OPTIONS]
#
# Options :
#   --build          Forcer la compilation Rust avant installation (cargo build --release)
#   --clean          Wiper /var/lib/gradatum avant init (detruit tokens/clés existants)
#   --yes            Mode non-interactif : skip toutes les confirmations
#   --root DIR       Répertoire racine Gradatum (défaut : /var/lib/gradatum)
#   --preset NAME    Preset ACL à utiliser (défaut : hierarchical)
#   --bind ADDR      Adresse d'écoute serveur (défaut : 127.0.0.1:19090)
#   --user NAME      Compte de service (défaut : gradatum). Créé avec un UID/GID
#                    DYNAMIQUE s'il n'existe pas ; réutilisé tel quel s'il existe.
#                    Si != gradatum, un drop-in systemd applique User=/Group= aux units.
#   -h, --help       Afficher cette aide et quitter
#   --with-engine    Installer gradatum-engine (superviseur llama-server) + template systemd
#   --with-gateway   Installer gradatum-gateway (routeur LLM) + service systemd
#   --cleanup-after  Lancer cargo clean après installation réussie (libère plusieurs GB)
#
# Idempotent : peut être relancé sans casser une installation existante.
# --clean est la seule opération destructrice (requiert confirmation).
#
# Pré-requis :
#   - Lancé avec sudo (uid 0)
#   - Binaires Rust compilés dans target/release/ (ou passer --build)
#   - systemd présent et fonctionnel
#   - Script lancé depuis la racine du workspace Gradatum

set -euo pipefail

# ─── Variables configurables ─────────────────────────────────────────────────

ROOT="${ROOT:-/var/lib/gradatum}"
PRESET="${PRESET:-hierarchical}"
BIND="${BIND:-127.0.0.1:19090}"
# Service account. Designated by NAME, never by number (systemd resolves the name
# at runtime). Created with a dynamic UID/GID if absent, reused as-is if present.
SVC_USER="${SVC_USER:-gradatum}"
DO_BUILD=false
DO_CLEAN=false
DO_YES=false
DO_CLEANUP_AFTER=false
DO_WITH_ENGINE=false
DO_WITH_GATEWAY=false
STEP=0
# Step count calculé après parsing des flags

# Utilisateur qui a invoqué sudo (pour le build Rust qui doit se faire avec son cargo).
# Si le script est lancé directement en tant que root (pas via sudo), SUDO_USER est vide.
INVOKER="${SUDO_USER:-root}"
INVOKER_HOME="$(getent passwd "$INVOKER" | cut -d: -f6 2>/dev/null || echo "/root")"

# Résolution du binaire cargo à utiliser pour le build.
# Stratégie : INVOKER_HOME/.cargo/bin/cargo → root/.cargo/bin/cargo → cargo dans PATH
# On préfère la toolchain rustup d'un user réel sur la toolchain système (souvent périmée).
_find_cargo() {
    local candidates=(
        "${INVOKER_HOME}/.cargo/bin/cargo"
        "/root/.cargo/bin/cargo"
    )
    for c in "${candidates[@]}"; do
        if [[ -x "$c" ]]; then echo "$c"; return 0; fi
    done
    command -v cargo 2>/dev/null || true
}
INVOKER_CARGO="$(_find_cargo)"
# Enrichir le PATH avec l'emplacement cargo retenu pour que rustc soit trouvable.
CARGO_BIN_DIR="$(dirname "$INVOKER_CARGO")"
export PATH="${CARGO_BIN_DIR}:${PATH}"

# ─── Parsing des arguments ────────────────────────────────────────────────────

print_usage() {
    cat <<'USAGE'
Usage : sudo bash scripts/install-gradatum-services.sh [OPTIONS]

Options :
  --build          Forcer la compilation Rust avant installation (cargo build --release)
  --clean          Wiper /var/lib/gradatum avant init (détruit tokens/clés existants)
  --yes            Mode non-interactif : skip toutes les confirmations
  --cleanup-after  Lancer cargo clean après installation réussie (libère plusieurs GB)
  --with-engine    Installer gradatum-engine (superviseur llama-server) + template systemd
  --with-gateway   Installer gradatum-gateway (routeur LLM) + service systemd
  --root DIR       Répertoire racine Gradatum (défaut : /var/lib/gradatum)
  --preset NAME    Preset ACL à utiliser (défaut : hierarchical)
  --bind ADDR      Adresse d'écoute serveur (défaut : 127.0.0.1:19090)
  --user NAME      Compte de service (défaut : gradatum). Créé avec un UID/GID
                   DYNAMIQUE s'il n'existe pas ; réutilisé tel quel s'il existe.
                   Si != gradatum, un drop-in systemd applique User=/Group= aux units.
  -h, --help       Afficher cette aide et quitter

Idempotent : peut être relancé sans casser une installation existante.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build)         DO_BUILD=true ;;
        --clean)         DO_CLEAN=true ;;
        --yes)           DO_YES=true ;;
        --cleanup-after) DO_CLEANUP_AFTER=true ;;
        --with-engine)   DO_WITH_ENGINE=true ;;
        --with-gateway)  DO_WITH_GATEWAY=true ;;
        --root)          ROOT="$2";   shift ;;
        --preset)        PRESET="$2"; shift ;;
        --bind)          BIND="$2";   shift ;;
        --user)          SVC_USER="$2"; shift ;;
        -h|--help)       print_usage; exit 0 ;;
        *)
            echo "Option inconnue : $1" >&2
            print_usage >&2
            exit 1
            ;;
    esac
    shift
done

# Recompute TOTAL_STEPS after parsing flags
TOTAL_STEPS=10
if [[ "$DO_WITH_ENGINE" == "true" ]]; then TOTAL_STEPS=$(( TOTAL_STEPS + 1 )); fi
if [[ "$DO_WITH_GATEWAY" == "true" ]]; then TOTAL_STEPS=$(( TOTAL_STEPS + 1 )); fi

# ─── Fonctions utilitaires ────────────────────────────────────────────────────

step() {
    STEP=$(( STEP + 1 ))
    echo ""
    echo "[$STEP/$TOTAL_STEPS] $*"
}

ok() {
    echo "  OK"
}

fail() {
    echo "  FAIL: $*" >&2
    exit 1
}

confirm() {
    # Demande confirmation interactive sauf si --yes
    local msg="$1"
    if [[ "$DO_YES" == "true" ]]; then
        echo "  (--yes) confirmation automatique : $msg"
        return 0
    fi
    read -r -p "  $msg [o/N] " reply
    if [[ ! "$reply" =~ ^[oO]$ ]]; then
        echo "  Annulé."
        exit 0
    fi
}

# Apply (or remove) the systemd drop-in that overrides User=/Group= for a
# non-default service account. Packaged units carry User=gradatum inline; a
# drop-in under /etc/systemd/system/<unit>.d/ overrides them WITHOUT editing the
# unit file, and survives a package/unit reinstall.
# When SVC_USER == gradatum, any stale drop-in from a previous --user run is
# removed so repeated installs converge (idempotence).
apply_service_user_dropin() {
    local unit="$1"
    local dropin_dir="/etc/systemd/system/${unit}.d"
    local dropin="${dropin_dir}/10-service-user.conf"
    if [[ "$SVC_USER" == "gradatum" ]]; then
        if [[ -f "$dropin" ]]; then
            rm -f "$dropin"
            echo "  Removed stale service-user drop-in: $dropin"
        fi
        return 0
    fi
    mkdir -p "$dropin_dir"
    printf '[Service]\nUser=%s\nGroup=%s\n' "$SVC_USER" "$SVC_USER" > "$dropin"
    chmod 0644 "$dropin"
    echo "  Service-user drop-in: $dropin (User=Group=$SVC_USER)"
}

# ─── En-tête ─────────────────────────────────────────────────────────────────

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum — Installation"
echo "  ROOT     : $ROOT"
echo "  PRESET   : $PRESET"
echo "  BIND     : $BIND"
echo "  USER     : $SVC_USER"
echo "  BUILD    : $DO_BUILD"
echo "  CLEAN    : $DO_CLEAN"
echo "  ENGINE   : $DO_WITH_ENGINE"
echo "  GATEWAY  : $DO_WITH_GATEWAY"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ─── [1/N] Pré-vérifications ─────────────────────────────────────────────────

step "Pré-vérifications"

# Root requis
if [[ "$(id -u)" -ne 0 ]]; then
    fail "ce script doit être exécuté avec sudo (uid 0 requis)"
fi

# OS Linux x86_64
ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
    fail "architecture non supportée : $ARCH (attendu : x86_64)"
fi

# Service account name must match the sysusers.d grammar (man sysusers.d):
# [a-zA-Z_] first char, then [a-zA-Z0-9_-], at most 31 chars total.
# Reject early with an actionable message rather than failing mid-install.
if [[ ! "$SVC_USER" =~ ^[a-zA-Z_][a-zA-Z0-9_-]{0,30}$ ]]; then
    fail "invalid --user name '$SVC_USER' (allowed: [a-zA-Z_][a-zA-Z0-9_-]{0,30}, per sysusers.d)"
fi

# Outils requis
for tool in systemctl install cp chmod id; do
    if ! command -v "$tool" &>/dev/null; then
        fail "outil requis manquant : $tool"
    fi
done

# curl présent pour le healthcheck (facultatif mais recommandé)
HAVE_CURL=true
if ! command -v curl &>/dev/null; then
    echo "  AVERTISSEMENT : curl absent — healthcheck ignoré à l'étape 9"
    HAVE_CURL=false
fi

# Script lancé depuis la racine workspace (contient Cargo.toml workspace)
if [[ ! -f "Cargo.toml" ]] || ! grep -q '^\[workspace\]' Cargo.toml 2>/dev/null; then
    fail "ce script doit être lancé depuis la racine du workspace Gradatum (Cargo.toml workspace introuvable)"
fi

ok

# ─── [2/N] Build (optionnel) ─────────────────────────────────────────────────

step "Build des binaires"

# Binaires de base (toujours installés)
BINARIES=(
    "target/release/gradatum-server"
    "target/release/gradatum-worker"
    "target/release/gradatum-admin"
)

# Binaires engine (optionnels, installés dans /opt/gradatum/bin/)
ENGINE_BINARIES=()
if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    ENGINE_BINARIES+=("target/release/gradatum-engine")
fi

# Binaires gateway (optionnels, installés dans /opt/gradatum/bin/)
GATEWAY_BINARIES=()
if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    GATEWAY_BINARIES+=("target/release/gradatum-gateway")
fi

if [[ "$DO_BUILD" == "true" ]]; then
    echo "  Compilation en cours (cargo build --release)…"
    echo "  toolchain : $(cargo --version 2>/dev/null || echo 'inconnue')"
    # Le build se fait avec les droits de l'utilisateur invocant si possible,
    # pour éviter de polluer ~/.cargo/registry avec des fichiers root.
    # On passe CARGO et RUSTUP_HOME explicitement pour forcer la toolchain de l'invocant.
    CARGO_CMD="${INVOKER_CARGO:-cargo}"
    if [[ -z "$CARGO_CMD" ]]; then
        fail "cargo introuvable — installer Rust ou passer le chemin cargo dans PATH"
    fi
    echo "  cargo    : $CARGO_CMD"
    echo "  rustc    : $("$CARGO_CMD" --version 2>/dev/null || echo 'inconnue')"

    # Déterminer le home du propriétaire du cargo (peut être root même si INVOKER != root).
    CARGO_OWNER="$(stat -c '%U' "$CARGO_CMD" 2>/dev/null || echo "root")"
    CARGO_OWNER_HOME="$(getent passwd "$CARGO_OWNER" | cut -d: -f6 2>/dev/null || echo "/root")"

    BUILD_ENV=(
        "CARGO_HOME=${CARGO_OWNER_HOME}/.cargo"
        "RUSTUP_HOME=${CARGO_OWNER_HOME}/.rustup"
        "PATH=${CARGO_BIN_DIR}:${PATH}"
    )

    # Construire la liste des arguments --bin pour le build principal
    MAIN_BUILD_BINS=(--bin gradatum-server --bin gradatum-worker --bin gradatum-admin)
    if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
        MAIN_BUILD_BINS+=(--bin gradatum-gateway)
    fi

    _run_build() {
        if [[ "$CARGO_OWNER" != "root" ]] && id "$CARGO_OWNER" &>/dev/null; then
            sudo -u "$CARGO_OWNER" env "${BUILD_ENV[@]}" "$CARGO_CMD" "$@"
        else
            env "${BUILD_ENV[@]}" "$CARGO_CMD" "$@"
        fi
    }

    # Build principal : server, worker, admin (et gateway si demandé)
    echo "  Build principal : ${MAIN_BUILD_BINS[*]}"
    if ! _run_build build --release "${MAIN_BUILD_BINS[@]}"; then
        fail "cargo build --release a échoué (build principal)"
    fi

    # Build engine (séparé — nécessite le feature 'serve')
    if [[ "$DO_WITH_ENGINE" == "true" ]]; then
        echo "  Build engine : --bin gradatum-engine --features gradatum-engine/serve"
        if ! _run_build build --release --bin gradatum-engine --features gradatum-engine/serve; then
            fail "cargo build --release --bin gradatum-engine a échoué"
        fi
    fi
else
    # Vérifier que les binaires existent
    MISSING=false
    for bin in "${BINARIES[@]}"; do
        if [[ ! -f "$bin" ]]; then
            echo "  Binaire manquant : $bin"
            MISSING=true
        fi
    done
    if [[ "$DO_WITH_ENGINE" == "true" ]]; then
        for bin in "${ENGINE_BINARIES[@]}"; do
            if [[ ! -f "$bin" ]]; then
                echo "  Binaire manquant : $bin"
                MISSING=true
            fi
        done
    fi
    if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
        for bin in "${GATEWAY_BINARIES[@]}"; do
            if [[ ! -f "$bin" ]]; then
                echo "  Binaire manquant : $bin"
                MISSING=true
            fi
        done
    fi
    if [[ "$MISSING" == "true" ]]; then
        fail "binaires manquants. Passer --build pour compiler, ou lancer 'cargo build --release' manuellement"
    fi
fi

ok

# ─── [3/N] Stop services ─────────────────────────────────────────────────────

step "Arrêt des services existants"

for svc in gradatum-server gradatum-worker; do
    if systemctl is-active --quiet "$svc" 2>/dev/null; then
        echo "  Arrêt de $svc…"
        systemctl stop "$svc" || true
    else
        echo "  $svc : inactif (skip)"
    fi
done

# Arrêter les instances engine actives (template gradatum-engine@*.service)
if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    while IFS= read -r unit; do
        [[ -z "$unit" ]] && continue
        echo "  Arrêt de $unit…"
        systemctl stop "$unit" || true
    done < <(systemctl list-units --type=service --state=active 'gradatum-engine@*' --no-legend 2>/dev/null | awk '{print $1}' || true)
fi

# Arrêter le gateway s'il tourne
if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    if systemctl is-active --quiet gradatum-gateway-spike 2>/dev/null; then
        echo "  Arrêt de gradatum-gateway-spike…"
        systemctl stop gradatum-gateway-spike || true
    fi
fi

ok

# ─── [4/N] Install binaires ─────────────────────────────────────────────────

step "Installation des binaires"

# Binaires cœur → /usr/bin/
for src in "${BINARIES[@]}"; do
    name="$(basename "$src")"
    dest="/usr/bin/$name"
    # Comparer les checksums pour éviter un install inutile
    if [[ -f "$dest" ]] && sha256sum -c <(sha256sum "$src" | awk "{print \$1, \"$dest\"}") &>/dev/null; then
        echo "  $name : identique à l'existant (skip)"
    else
        install -m 755 "$src" "$dest"
        echo "  $name → $dest"
    fi
done

# Helper ExecStartPre gradatum-pre-migration-backup → /usr/local/bin/ (TOUJOURS : les units
# server ET worker le déclarent en ExecStartPre SANS préfixe `-` — son absence fait échouer
# le démarrage (status=216/GROUP). Backup atomique des DBs avant toute migration de schéma :
# fail-closed volontaire, jamais `-`, un backup qui échoue doit bloquer, pas passer en silence.
BACKUP_HELPER_SRC="packaging/systemd/gradatum-pre-migration-backup"
if [[ ! -f "$BACKUP_HELPER_SRC" ]]; then
    fail "helper manquant : $BACKUP_HELPER_SRC (requis par ExecStartPre des units server/worker)"
fi
install -m 755 "$BACKUP_HELPER_SRC" /usr/local/bin/gradatum-pre-migration-backup
echo "  gradatum-pre-migration-backup → /usr/local/bin/gradatum-pre-migration-backup"

# Binaires engine/gateway → /opt/gradatum/bin/ (cohérent avec leurs templates systemd)
if [[ "$DO_WITH_ENGINE" == "true" ]] || [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    mkdir -p /opt/gradatum/bin
    chown "$SVC_USER:$SVC_USER" /opt/gradatum/bin 2>/dev/null || true
    chmod 0755 /opt/gradatum/bin
fi

if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    for src in "${ENGINE_BINARIES[@]}"; do
        name="$(basename "$src")"
        dest="/opt/gradatum/bin/$name"
        if [[ -f "$dest" ]] && sha256sum -c <(sha256sum "$src" | awk "{print \$1, \"$dest\"}") &>/dev/null; then
            echo "  $name : identique à l'existant (skip)"
        else
            install -m 755 "$src" "$dest"
            echo "  $name → $dest"
        fi
    done
    # wait-for-port-free.sh → /opt/gradatum/bin/
    if [[ -f "packaging/systemd/wait-for-port-free.sh" ]]; then
        install -m 755 "packaging/systemd/wait-for-port-free.sh" /opt/gradatum/bin/wait-for-port-free.sh
        echo "  wait-for-port-free.sh → /opt/gradatum/bin/wait-for-port-free.sh"
    else
        fail "wait-for-port-free.sh manquant dans packaging/systemd/"
    fi
fi

if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    for src in "${GATEWAY_BINARIES[@]}"; do
        name="$(basename "$src")"
        dest="/opt/gradatum/bin/$name"
        if [[ -f "$dest" ]] && sha256sum -c <(sha256sum "$src" | awk "{print \$1, \"$dest\"}") &>/dev/null; then
            echo "  $name : identique à l'existant (skip)"
        else
            install -m 755 "$src" "$dest"
            echo "  $name → $dest"
        fi
    done
fi

ok

# ─── [5/N] Sysusers (user système gradatum) ─────────────────────────────────

step "Création du compte de service ($SVC_USER)"

if id "$SVC_USER" &>/dev/null; then
    # Reuse an existing account as-is — no requirement on its numeric UID/GID.
    echo "  Account '$SVC_USER' already exists — reusing as-is (UID $(id -u "$SVC_USER"))"
elif [[ "$SVC_USER" == "gradatum" ]]; then
    # Default account: install the packaged sysusers.d unit (dynamic UID/GID).
    SYSUSERS_SRC="packaging/sysusers.d/gradatum.conf"
    if [[ ! -f "$SYSUSERS_SRC" ]]; then
        fail "fichier sysusers manquant : $SYSUSERS_SRC"
    fi
    install -m 644 "$SYSUSERS_SRC" /usr/lib/sysusers.d/gradatum.conf
    systemd-sysusers /usr/lib/sysusers.d/gradatum.conf
else
    # Custom account name: create a system user + matching group with a DYNAMIC
    # UID/GID, fed to systemd-sysusers on stdin. No persistent sysusers.d file is
    # installed for a custom name.
    echo "  Creating system account '$SVC_USER' (dynamic UID/GID)…"
    printf 'u %s - "Gradatum service" /var/lib/gradatum /sbin/nologin\n' "$SVC_USER" \
        | systemd-sysusers -
fi

# Postflight — closes the exact hole that caused the historical mid-install panic.
# systemd-sysusers can create the user while attaching it to a pre-existing
# FOREIGN group that already holds the target numeric GID, leaving NO group named
# "$SVC_USER". A later `chown $SVC_USER:$SVC_USER` would then die with "invalid
# group" half-way through the install. Fail EARLY and actionably instead.
if ! id "$SVC_USER" &>/dev/null; then
    fail "system account '$SVC_USER' does not exist after creation (systemd-sysusers failed)"
fi
if ! getent group "$SVC_USER" &>/dev/null; then
    fail "group '$SVC_USER' is missing: the account exists but no matching group was created (a numeric GID collision likely attached '$SVC_USER' to a foreign group). Resolve the GID conflict, or install under a free name with --user <name>, then re-run."
fi

echo "  id $SVC_USER : $(id "$SVC_USER")"
ok

# ─── [6/N] Systemd units ─────────────────────────────────────────────────────

step "Installation des unit files systemd"

# Server + worker (toujours)
for unit in gradatum-server.service gradatum-worker.service; do
    src="packaging/systemd/$unit"
    if [[ ! -f "$src" ]]; then
        fail "unit file manquant : $src"
    fi
    install -m 644 "$src" "/etc/systemd/system/$unit"
    echo "  $unit → /etc/systemd/system/$unit"
    apply_service_user_dropin "$unit"
done

# Engine template
if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    src="packaging/systemd/gradatum-engine@.service"
    if [[ ! -f "$src" ]]; then
        fail "unit file manquant : $src"
    fi
    install -m 644 "$src" "/etc/systemd/system/gradatum-engine@.service"
    echo "  gradatum-engine@.service → /etc/systemd/system/gradatum-engine@.service"
    apply_service_user_dropin "gradatum-engine@.service"
fi

# Gateway service
if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    src="packaging/systemd/gradatum-gateway-spike.service"
    if [[ ! -f "$src" ]]; then
        fail "unit file manquant : $src"
    fi
    install -m 644 "$src" "/etc/systemd/system/gradatum-gateway-spike.service"
    echo "  gradatum-gateway-spike.service → /etc/systemd/system/gradatum-gateway-spike.service"
    apply_service_user_dropin "gradatum-gateway-spike.service"
fi

systemctl daemon-reload
echo "  daemon-reload OK"

ok

# ─── [7/N] Configuration engine (optionnel) ──────────────────────────────────

if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    step "Configuration engine"

    # Répertoire conf.d pour les configs engine
    mkdir -p /etc/gradatum/conf.d
    chown "$SVC_USER:$SVC_USER" /etc/gradatum/conf.d
    chmod 0750 /etc/gradatum/conf.d
    echo "  /etc/gradatum/conf.d/ créé"

    # Copie des exemples de config engine (si absents)
    for example_cfg in engine-chat.toml engine-embed.toml; do
        src="examples/configs/$example_cfg"
        dest="/etc/gradatum/conf.d/70-$example_cfg"
        if [[ -f "$src" ]]; then
            if [[ ! -f "$dest" ]]; then
                install -m 644 -o "$SVC_USER" -g "$SVC_USER" "$src" "$dest"
                echo "  $example_cfg → $dest"
            else
                echo "  $dest existe déjà (skip — préserver la config existante)"
            fi
        else
            echo "  AVERTISSEMENT : exemple config manquant : $src"
        fi
    done

    # Répertoire models/
    if [[ ! -d /opt/gradatum/models ]]; then
        mkdir -p /opt/gradatum/models
        chown "$SVC_USER:$SVC_USER" /opt/gradatum/models
        chmod 0755 /opt/gradatum/models
        echo "  /opt/gradatum/models/ créé (placer vos fichiers GGUF ici)"
    fi

    # EnvironmentFile pour secrets engine
    ENV_FILE="/etc/gradatum/engine-secrets.env"
    if [[ ! -f "$ENV_FILE" ]]; then
        touch "$ENV_FILE"
        chown "$SVC_USER:$SVC_USER" "$ENV_FILE"
        chmod 0600 "$ENV_FILE"
        echo "  $ENV_FILE créé (vide — ajouter les variables de secret ici)"
    fi

    ok
fi

# ─── [8/N] Configuration gateway (optionnel) ─────────────────────────────────

if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    step "Configuration gateway"

    GATEWAY_CFG="/etc/gradatum/gateway-spike.toml"
    if [[ ! -f "$GATEWAY_CFG" ]]; then
        echo "  AVERTISSEMENT : $GATEWAY_CFG absent — le gateway ne démarrera pas sans config"
        echo "  Exemple à créer : consulter docs/DEPLOYMENT.md §10 (gateway wiring)"
    else
        echo "  $GATEWAY_CFG présent"
    fi

    ok
fi

# ─── [9/N] Init root ────────────────────────────────────────────────────────

step "Initialisation du répertoire racine Gradatum ($ROOT)"

# Gestion du --clean
if [[ "$DO_CLEAN" == "true" ]]; then
    if [[ -d "$ROOT" ]] && [[ -n "$(ls -A "$ROOT" 2>/dev/null)" ]]; then
        echo "  AVERTISSEMENT : --clean va supprimer tout le contenu de $ROOT"
        echo "  Cela inclut les clés JWT, le bearer admin et toutes les bases SQLite."
        confirm "Confirmer la suppression de $ROOT ?"
        rm -rf "${ROOT:?}"/*
        echo "  $ROOT vidé"
    fi
fi

# Créer le répertoire si absent
mkdir -p "$ROOT"
chown "$SVC_USER:$SVC_USER" "$ROOT"
chmod 0750 "$ROOT"

# Init si pas encore fait (ou si --force via --clean)
BEARER_MARKER="$ROOT/config/admin.bearer.txt"
INIT_CMD=(
    sudo -u "$SVC_USER" gradatum-admin init
    --root "$ROOT"
    --preset "$PRESET"
    --bind "$BIND"
    --non-interactive
)

if [[ -f "$BEARER_MARKER" ]] && [[ "$DO_CLEAN" != "true" ]]; then
    echo "  Déjà initialisé ($BEARER_MARKER existe) — re-init avec --force"
    INIT_CMD+=(--force)
fi

echo "  Exécution : ${INIT_CMD[*]}"
if ! "${INIT_CMD[@]}"; then
    fail "gradatum-admin init a échoué"
fi

# Vérification du résultat — liste confrontée au réel d'un `gradatum-admin init` 2.0.0.
# jwt.private.pem / jwt.public.pem RETIRÉS : init ne les génère plus (le serveur crée
# lui-même config/jwt-signing-key.secret au 1er boot ; cf packaging/systemd/README.md).
# Les attendre ici abattait le script juste après un init réussi, sur toute machine.
for expected in "config/admin.bearer.txt" "config/server.toml" "config/bearer.toml" \
                "db/queue.sqlite" "db/revocation.sqlite" "db/api_keys.sqlite"; do
    if [[ ! -f "$ROOT/$expected" ]]; then
        fail "fichier attendu manquant après init : $ROOT/$expected"
    fi
done

echo "  Structure $ROOT vérifiée"
ok

# ─── [9b/N] Fichier d'environnement worker (jeton API interne) ────────────────
# Le worker lit GRADATUM_INTERNAL_TOKEN depuis /etc/gradatum/env (EnvironmentFile SANS `-`
# dans l'unit : une absence échoue TÔT au niveau systemd, jamais en crash-loop silencieux).
# init vient de déposer ce jeton dans <root>/config/internal-worker.token.txt ; sans ce
# transport, le worker boucle sur « GRADATUM_INTERNAL_TOKEN must be set » pendant que
# /health reste vert. On complète donc ici le chemin systemd.
step "Fichier d'environnement worker (/etc/gradatum/env)"

# Nom de variable distinct de l'ENV_FILE de la section engine (engine-secrets.env) pour
# écarter tout risque de collision/shadowing entre les deux blocs top-level.
WORKER_ENV_FILE="/etc/gradatum/env"
WORKER_TOKEN_FILE="$ROOT/config/internal-worker.token.txt"
mkdir -p /etc/gradatum

if [[ -f "$WORKER_ENV_FILE" ]] && grep -qE '^[[:space:]]*GRADATUM_INTERNAL_TOKEN=' "$WORKER_ENV_FILE"; then
    # Idempotence : un jeton déjà présent (run précédent OU posé par l'opérateur) n'est
    # JAMAIS écrasé ni dupliqué. init --force PRÉSERVE le [internal_api].token existant,
    # donc le jeton du fichier reste valide d'une exécution à l'autre.
    echo "  $WORKER_ENV_FILE porte déjà GRADATUM_INTERNAL_TOKEN — laissé tel quel (valeur préservée)"
else
    # Fail-early actionnable : sans le jeton, le worker bouclerait en silence. On refuse
    # d'installer un chemin systemd cassé.
    if [[ ! -f "$WORKER_TOKEN_FILE" ]]; then
        fail "jeton API interne absent : $WORKER_TOKEN_FILE — lancer 'gradatum-admin init' d'abord. Refus de laisser gradatum-worker en crash-loop silencieux (GRADATUM_INTERNAL_TOKEN must be set)."
    fi
    INTERNAL_TOKEN="$(tr -d '\n' < "$WORKER_TOKEN_FILE")"
    [[ -n "$INTERNAL_TOKEN" ]] || fail "jeton API interne vide dans $WORKER_TOKEN_FILE — re-lancer 'gradatum-admin init'."
    touch "$WORKER_ENV_FILE"
    # URL : n'ajouter la ligne que si absente (jamais de doublon ; défaut worker = 19092 si omise).
    grep -qE '^[[:space:]]*GRADATUM_INTERNAL_URL=' "$WORKER_ENV_FILE" \
        || printf 'GRADATUM_INTERNAL_URL=http://127.0.0.1:19092\n' >> "$WORKER_ENV_FILE"
    printf 'GRADATUM_INTERNAL_TOKEN=%s\n' "$INTERNAL_TOKEN" >> "$WORKER_ENV_FILE"
    # Secret lu par SYSTEMD (root) au démarrage, PAS par le compte de service (le worker
    # reçoit le jeton via l'environnement injecté). Propriétaire root, groupe = compte de
    # service ($SVC_USER, suit --user), mode 0640 — cohérent avec engine-secrets.env.
    chown "root:$SVC_USER" "$WORKER_ENV_FILE"
    chmod 0640 "$WORKER_ENV_FILE"
    echo "  $WORKER_ENV_FILE écrit (root:$SVC_USER 0640) — GRADATUM_INTERNAL_TOKEN + URL"
fi

ok

# ─── [10/N] Enable et démarrage services ─────────────────────────────────────

step "Activation et démarrage des services"

# Server d'abord (sd_notify ready post-bind), worker ensuite
for svc in gradatum-server gradatum-worker; do
    echo "  enable --now $svc…"
    systemctl enable --now "$svc"
done

# Engine : le template est installé mais aucune instance n'est activée automatiquement.
# L'opérateur crée les instances manuellement :
#   systemctl enable --now gradatum-engine@curator
#   systemctl enable --now gradatum-engine@embed
if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    echo ""
    echo "  Engine : template gradatum-engine@.service installé."
    echo "  Créer une instance (après avoir placé le modèle GGUF) :"
    echo "    sudo systemctl enable --now gradatum-engine@curator"
    echo "    sudo systemctl enable --now gradatum-engine@embed"
fi

# Gateway : démarrage manuel (nécessite une config gateway-spike.toml)
if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
    echo ""
    echo "  Gateway : service gradatum-gateway-spike installé (démarrage manuel)."
    echo "    sudo systemctl enable --now gradatum-gateway-spike"
fi

ok

# ─── [11/N] Healthcheck ──────────────────────────────────────────────────────

step "Healthcheck HTTP"

if [[ "$HAVE_CURL" == "false" ]]; then
    echo "  curl absent — healthcheck ignoré"
    ok
else
    # Extraire le host:port depuis BIND
    HEALTH_URL="http://$BIND/health"
    echo "  Attente démarrage (3s)…"
    sleep 3

    HTTP_RESPONSE="$(curl -fsS --max-time 5 "$HEALTH_URL" 2>&1)" || {
        echo "  WARN : curl a retourné une erreur — vérifier le statut manuellement :"
        echo "    curl $HEALTH_URL"
        echo "    sudo journalctl -u gradatum-server -n 30"
        # Non-fatal : le service peut être encore en train de démarrer
    }

    if echo "$HTTP_RESPONSE" | grep -q '"status"'; then
        echo "  Réponse /health : $HTTP_RESPONSE"
        ok
    else
        echo "  WARN : réponse inattendue de /health : $HTTP_RESPONSE"
        echo "  Vérifier manuellement : curl $HEALTH_URL"
    fi
fi

# ─── [12/N] Verdict ─────────────────────────────────────────────────────────

step "Vérification finale"

SERVER_ACTIVE="$(systemctl is-active gradatum-server 2>/dev/null || echo inactive)"
WORKER_ACTIVE="$(systemctl is-active gradatum-worker 2>/dev/null || echo inactive)"

echo ""
echo "  gradatum-server : $SERVER_ACTIVE"
echo "  gradatum-worker : $WORKER_ACTIVE"

# Lister les instances engine actives (informatif)
if [[ "$DO_WITH_ENGINE" == "true" ]]; then
    echo ""
    echo "  Instances engine actives :"
    systemctl list-units --type=service --state=active 'gradatum-engine@*' --no-legend 2>/dev/null | awk '{print "    " $1}' || echo "    (aucune)"
fi

if [[ "$SERVER_ACTIVE" == "active" ]] && [[ "$WORKER_ACTIVE" == "active" ]]; then
    # Cleanup target/ post-install (libère plusieurs GB) — opt-in via --cleanup-after.
    if [[ "$DO_CLEANUP_AFTER" == "true" ]] && [[ "$DO_BUILD" == "true" ]]; then
        echo ""
        echo "[cleanup] cargo clean (libération target/release et target/debug)…"
        REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
        TARGET_SIZE_BEFORE="$(du -sh "$REPO_DIR/target" 2>/dev/null | awk '{print $1}')"
        if [[ "$INVOKER_CARGO" == /root/* ]] || [[ -z "${SUDO_USER:-}" ]]; then
            "$INVOKER_CARGO" clean --manifest-path "$REPO_DIR/Cargo.toml" 2>&1 | tail -3 || true
        else
            sudo -u "$SUDO_USER" "$INVOKER_CARGO" clean --manifest-path "$REPO_DIR/Cargo.toml" 2>&1 | tail -3 || true
        fi
        echo "  target/ avant cleanup : ${TARGET_SIZE_BEFORE:-N/A}"
        echo "  Cleanup OK (binaires LIVE dans /usr/bin/ et /opt/gradatum/bin/ inchangés)"
    fi

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  PASS — Gradatum opérationnel sur $BIND"
    echo ""
    # Détection dynamique du dernier smoke alpha-N disponible
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    SMOKE_LATEST=$(ls "$SCRIPT_DIR"/smoke-alpha-*.sh 2>/dev/null \
        | sed 's/.*smoke-alpha-\([0-9]*\).*/\1 &/' \
        | sort -n | tail -1 | awk '{print $2}')
    SMOKE_LATEST="${SMOKE_LATEST##*/}"
    [ -z "$SMOKE_LATEST" ] && SMOKE_LATEST="smoke-alpha-13.sh"

    echo "  Étapes suivantes :"
    echo "    1. Récupérer le bearer admin : sudo cat $ROOT/config/admin.bearer.txt"
    echo "    2. Lancer le smoke test      :"
    echo "         export GRADATUM_BEARER=\$(sudo cat $ROOT/config/admin.bearer.txt)"
    echo "         bash scripts/$SMOKE_LATEST"
    echo "       (ou : sudo -E bash scripts/$SMOKE_LATEST si lancement sudo nécessaire)"
    echo "    3. Logs serveur              : sudo journalctl -u gradatum-server -f"
    echo "    4. Logs worker               : sudo journalctl -u gradatum-worker -f"

    if [[ "$DO_WITH_ENGINE" == "true" ]]; then
        echo ""
        echo "  ── Engine ──"
        echo "    Config example  : /etc/gradatum/conf.d/70-engine-chat.toml"
        echo "    Config example  : /etc/gradatum/conf.d/70-engine-embed.toml"
        echo "    Modèles         : /opt/gradatum/models/"
        echo "    Démarrer instance: sudo systemctl enable --now gradatum-engine@curator"
        echo "    Logs engine      : sudo journalctl -u gradatum-engine@curator -f"
    fi

    if [[ "$DO_WITH_GATEWAY" == "true" ]]; then
        echo ""
        echo "  ── Gateway ──"
        echo "    Config requise  : /etc/gradatum/gateway-spike.toml"
        echo "    Démarrer         : sudo systemctl enable --now gradatum-gateway-spike"
        echo "    Logs gateway     : sudo journalctl -u gradatum-gateway-spike -f"
    fi

    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
else
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  FAIL — Un ou plusieurs services sont inactifs"
    echo ""
    echo "  Diagnostic :"
    echo "    sudo systemctl status gradatum-server gradatum-worker"
    echo "    sudo journalctl -u gradatum-server -n 50"
    echo "    sudo journalctl -u gradatum-worker -n 50"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    exit 1
fi
