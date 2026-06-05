#!/usr/bin/env bash
# install-gradatum-services.sh — Installation idempotente des services gradatum (server + worker) sur Linux x86_64.
# (Renamed 2026-05-10 for service-oriented scope clarity — works on any Linux host.)
#
# Usage :
#   sudo bash scripts/install-gradatum-services.sh [OPTIONS]
#
# Options :
#   --build       Forcer la compilation Rust avant installation (cargo build --release)
#   --clean       Wiper /var/lib/gradatum avant init (detruit tokens/clés existants)
#   --yes         Mode non-interactif : skip toutes les confirmations
#   --root DIR    Répertoire racine Gradatum (défaut : /var/lib/gradatum)
#   --preset NAME Preset ACL à utiliser (défaut : hierarchical)
#   --bind ADDR   Adresse d'écoute serveur (défaut : 127.0.0.1:19090)
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
DO_BUILD=false
DO_CLEAN=false
DO_YES=false
DO_CLEANUP_AFTER=false
STEP=0
TOTAL_STEPS=10

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

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build)  DO_BUILD=true ;;
        --clean)  DO_CLEAN=true ;;
        --yes)    DO_YES=true ;;
        --cleanup-after) DO_CLEANUP_AFTER=true ;;
        --root)   ROOT="$2";   shift ;;
        --preset) PRESET="$2"; shift ;;
        --bind)   BIND="$2";   shift ;;
        *)
            echo "Option inconnue : $1" >&2
            echo "Usage : sudo bash scripts/install-gradatum-services.sh [--build] [--clean] [--cleanup-after] [--yes] [--root DIR] [--preset NAME] [--bind ADDR]" >&2
            exit 1
            ;;
    esac
    shift
done

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

# ─── En-tête ─────────────────────────────────────────────────────────────────

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum — Installation"
echo "  ROOT    : $ROOT"
echo "  PRESET  : $PRESET"
echo "  BIND    : $BIND"
echo "  BUILD   : $DO_BUILD"
echo "  CLEAN   : $DO_CLEAN"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ─── [1/10] Pré-vérifications ─────────────────────────────────────────────────

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

# ─── [2/10] Build (optionnel) ─────────────────────────────────────────────────

step "Build des binaires"

BINARIES=(
    "target/release/gradatum-server"
    "target/release/gradatum-worker"
    "target/release/gradatum-admin"
)

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

    if [[ "$CARGO_OWNER" != "root" ]] && id "$CARGO_OWNER" &>/dev/null; then
        if ! sudo -u "$CARGO_OWNER" env "${BUILD_ENV[@]}" "$CARGO_CMD" build --release \
            --bin gradatum-server --bin gradatum-worker --bin gradatum-admin; then
            fail "cargo build --release a échoué (lancé comme $CARGO_OWNER)"
        fi
    else
        if ! env "${BUILD_ENV[@]}" "$CARGO_CMD" build --release \
            --bin gradatum-server --bin gradatum-worker --bin gradatum-admin; then
            fail "cargo build --release a échoué"
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
    if [[ "$MISSING" == "true" ]]; then
        fail "binaires manquants. Passer --build pour compiler, ou lancer 'cargo build --release' manuellement"
    fi
fi

ok

# ─── [3/10] Stop services ─────────────────────────────────────────────────────

step "Arrêt des services existants"

for svc in gradatum-server gradatum-worker; do
    if systemctl is-active --quiet "$svc" 2>/dev/null; then
        echo "  Arrêt de $svc…"
        systemctl stop "$svc" || true
    else
        echo "  $svc : inactif (skip)"
    fi
done

ok

# ─── [4/10] Install binaires ─────────────────────────────────────────────────

step "Installation des binaires dans /usr/bin/"

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

ok

# ─── [5/10] Sysusers (user système gradatum) ─────────────────────────────────

step "Création de l'utilisateur système gradatum (UID 985)"

SYSUSERS_SRC="packaging/sysusers.d/gradatum.conf"
if [[ ! -f "$SYSUSERS_SRC" ]]; then
    fail "fichier sysusers manquant : $SYSUSERS_SRC"
fi

install -m 644 "$SYSUSERS_SRC" /usr/lib/sysusers.d/gradatum.conf
systemd-sysusers

# Vérification post-sysusers
if ! id gradatum &>/dev/null; then
    fail "l'utilisateur gradatum n'existe pas après systemd-sysusers"
fi

ACTUAL_UID="$(id -u gradatum)"
if [[ "$ACTUAL_UID" != "985" ]]; then
    fail "UID gradatum = $ACTUAL_UID (attendu : 985) — conflit d'UID possible"
fi

echo "  id gradatum : $(id gradatum)"
ok

# ─── [6/10] Systemd units ─────────────────────────────────────────────────────

step "Installation des unit files systemd"

for unit in gradatum-server.service gradatum-worker.service; do
    src="packaging/systemd/$unit"
    if [[ ! -f "$src" ]]; then
        fail "unit file manquant : $src"
    fi
    install -m 644 "$src" "/etc/systemd/system/$unit"
    echo "  $unit → /etc/systemd/system/$unit"
done

systemctl daemon-reload
echo "  daemon-reload OK"

ok

# ─── [7/10] Init root ────────────────────────────────────────────────────────

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
chown gradatum:gradatum "$ROOT"
chmod 0750 "$ROOT"

# Init si pas encore fait (ou si --force via --clean)
BEARER_MARKER="$ROOT/config/admin.bearer.txt"
INIT_CMD=(
    sudo -u gradatum gradatum-admin init
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

# Vérification du résultat
for expected in "config/admin.bearer.txt" "config/server.toml" "config/bearer.toml" \
                "config/jwt.private.pem" "config/jwt.public.pem" \
                "db/queue.sqlite" "db/revocation.sqlite" "db/api_keys.sqlite"; do
    if [[ ! -f "$ROOT/$expected" ]]; then
        fail "fichier attendu manquant après init : $ROOT/$expected"
    fi
done

echo "  Structure $ROOT vérifiée"
ok

# ─── [8/10] Enable et démarrage services ─────────────────────────────────────

step "Activation et démarrage des services"

# Server d'abord (sd_notify ready post-bind), worker ensuite
for svc in gradatum-server gradatum-worker; do
    echo "  enable --now $svc…"
    systemctl enable --now "$svc"
done

ok

# ─── [9/10] Healthcheck ──────────────────────────────────────────────────────

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

# ─── [10/10] Verdict ─────────────────────────────────────────────────────────

step "Vérification finale"

SERVER_ACTIVE="$(systemctl is-active gradatum-server 2>/dev/null || echo inactive)"
WORKER_ACTIVE="$(systemctl is-active gradatum-worker 2>/dev/null || echo inactive)"

echo ""
echo "  gradatum-server : $SERVER_ACTIVE"
echo "  gradatum-worker : $WORKER_ACTIVE"

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
        echo "  Cleanup OK (binaires LIVE dans /usr/bin/ inchangés)"
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
