#!/usr/bin/env bash
# install-gradatum-stub-mcp.sh — Installation idempotente du proxy MCP gradatum-mcp-stub.
#
# Usage :
#   sudo bash scripts/install-gradatum-stub-mcp.sh [OPTIONS]
#
# Options :
#   --build       Forcer la compilation Rust avant installation (cargo build --release -p gradatum-mcp-stub)
#   --yes         Mode non-interactif : skip toutes les confirmations
#   --cleanup-after  Lancer cargo clean après install (libère target/ — plusieurs GB)
#   --api-key-file PATH  Path vers fichier api-key (défaut : /etc/gradatum/claude-code.api-key)
#   --server-url URL     URL serveur gradatum (défaut : http://127.0.0.1:19090)
#
# Idempotent : peut être relancé sans casser une installation existante.
# Pré-requis : gradatum-server + gradatum-admin déjà installés (via install-gradatum-services.sh).
#
# Le stub MCP fonctionne en mode auto-refresh JWT (recommandé) :
#   1. Lit l'api-key permanente depuis $GRADATUM_API_KEY_FILE (chmod 600)
#   2. Fait POST /auth/exchange → JWT
#   3. Renouvelle automatiquement le JWT quand TTL < 30%
#
# Sortie : binaire installé /usr/bin/gradatum-mcp-stub + mcp.json.sample généré
# dans scripts/ avec config prête à coller dans ~/.claude.json (Claude Code) ou
# config Claude Desktop.

set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────
DO_BUILD=false
DO_YES=false
DO_CLEANUP_AFTER=false
API_KEY_FILE="${API_KEY_FILE:-/etc/gradatum/claude-code.api-key}"
SERVER_URL="${SERVER_URL:-http://127.0.0.1:19090}"
STEP=0
TOTAL_STEPS=6

INVOKER="${SUDO_USER:-$(whoami)}"
INVOKER_HOME="$(getent passwd "$INVOKER" | cut -d: -f6 2>/dev/null || echo "$HOME")"

# ── Detect cargo (idem install-gradatum-services.sh) ────────────────────────
# Stratégie : INVOKER_HOME/.cargo/bin/cargo → /root/.cargo/bin/cargo → cargo dans PATH
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
if [[ -n "$INVOKER_CARGO" ]]; then
    CARGO_BIN_DIR="$(dirname "$INVOKER_CARGO")"
    export PATH="${CARGO_BIN_DIR}:${PATH}"
fi

# ── Args parsing ─────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --build) DO_BUILD=true; shift ;;
        --yes|-y) DO_YES=true; shift ;;
        --cleanup-after) DO_CLEANUP_AFTER=true; shift ;;
        --api-key-file) API_KEY_FILE="$2"; shift 2 ;;
        --server-url) SERVER_URL="$2"; shift 2 ;;
        --help|-h)
            sed -n '/^# Usage/,/^set -e/p' "$0" | sed 's/^# \?//' | head -n -1
            exit 0
            ;;
        *)
            echo "Option inconnue : $1" >&2
            echo "Usage : sudo bash scripts/install-gradatum-stub-mcp.sh [--build] [--yes] [--cleanup-after] [--api-key-file PATH] [--server-url URL]" >&2
            exit 2
            ;;
    esac
done

step() {
    STEP=$((STEP + 1))
    echo ""
    echo "[${STEP}/${TOTAL_STEPS}] $*"
}

ok() { echo "  OK"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

# ── Header ───────────────────────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum MCP Stub — Installation"
echo "  API_KEY  : ${API_KEY_FILE}"
echo "  SERVER   : ${SERVER_URL}"
echo "  BUILD    : ${DO_BUILD}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ── Step 1 : Pré-vérifications ──────────────────────────────────────────────
step "Pré-vérifications"

if [ "$EUID" -ne 0 ]; then
    echo "  ERREUR : ce script doit être lancé en root (sudo)." >&2
    exit 1
fi

if ! command -v gradatum-server >/dev/null 2>&1; then
    echo "  AVERTISSEMENT : gradatum-server non installé." >&2
    echo "  Lancer d'abord : sudo bash scripts/install-gradatum-services.sh" >&2
    if ! $DO_YES; then
        read -rp "  Continuer quand même ? [y/N] " ans
        [[ "${ans,,}" == "y" ]] || exit 1
    fi
fi

if [ "$DO_BUILD" = true ] && [ -z "$INVOKER_CARGO" ]; then
    echo "  ERREUR : --build demandé mais cargo introuvable." >&2
    exit 1
fi
ok

# ── Step 2 : Build du binaire ────────────────────────────────────────────────
step "Build du binaire gradatum-mcp-stub"

if [ "$DO_BUILD" = true ]; then
    echo "  Compilation en cours (cargo build --release -p gradatum-mcp-stub)…"
    if [[ -z "$INVOKER_CARGO" ]]; then
        echo "  ERREUR : cargo introuvable — installer Rust ou passer le chemin cargo dans PATH" >&2
        exit 1
    fi
    echo "  cargo     : $INVOKER_CARGO"
    echo "  toolchain : $($INVOKER_CARGO --version 2>/dev/null || echo 'inconnue')"
    # Si cargo est dans /root/.cargo (toolchain root), on est déjà root → exécuter direct.
    # Sinon (cargo dans home invoker), on délègue à SUDO_USER pour ne pas polluer ~/.cargo root.
    if [[ "$INVOKER_CARGO" == /root/* ]] || [[ -z "${SUDO_USER:-}" ]]; then
        "$INVOKER_CARGO" build --release -p gradatum-mcp-stub --manifest-path "$REPO_DIR/Cargo.toml"
    else
        sudo -u "$SUDO_USER" "$INVOKER_CARGO" build --release -p gradatum-mcp-stub --manifest-path "$REPO_DIR/Cargo.toml"
    fi
    BIN_SRC="$REPO_DIR/target/release/gradatum-mcp-stub"
else
    if [ -x "$REPO_DIR/target/release/gradatum-mcp-stub" ]; then
        BIN_SRC="$REPO_DIR/target/release/gradatum-mcp-stub"
        echo "  Binaire existant utilisé : $BIN_SRC"
    else
        echo "  ERREUR : binaire introuvable dans $REPO_DIR/target/release/." >&2
        echo "  Relancer avec --build pour compiler." >&2
        exit 1
    fi
fi
ok

# ── Step 3 : Installation /usr/bin/gradatum-mcp-stub ────────────────────────
step "Installation du binaire dans /usr/bin/"
install -m 0755 -o root -g root "$BIN_SRC" /usr/bin/gradatum-mcp-stub
echo "  gradatum-mcp-stub → /usr/bin/gradatum-mcp-stub"
echo "  Version : $(/usr/bin/gradatum-mcp-stub --version 2>/dev/null || echo 'unknown')"
ok

# ── Step 4 : Vérification / création de l'api-key ───────────────────────────
step "Vérification du fichier api-key"

if [ -r "$API_KEY_FILE" ]; then
    APIKEY_LEN=$(wc -c < "$API_KEY_FILE" | tr -d ' ')
    APIKEY_PERMS=$(stat -c %a "$API_KEY_FILE")
    APIKEY_OWNER=$(stat -c %U:%G "$API_KEY_FILE")
    echo "  api-key existante : $API_KEY_FILE"
    echo "  longueur : ${APIKEY_LEN} chars | perms : ${APIKEY_PERMS} | owner : ${APIKEY_OWNER}"

    if [ "$APIKEY_PERMS" != "600" ] && [ "$APIKEY_PERMS" != "400" ]; then
        echo "  AVERTISSEMENT : permissions ${APIKEY_PERMS} non sécurisées — corriger en 600" >&2
        chmod 600 "$API_KEY_FILE"
        echo "  → permissions corrigées : 600"
    fi
else
    echo "  api-key absente → création via gradatum-admin"
    if ! command -v gradatum-admin >/dev/null 2>&1; then
        echo "  ERREUR : gradatum-admin introuvable. Lancer install-gradatum-services.sh d'abord." >&2
        exit 1
    fi
    mkdir -p "$(dirname "$API_KEY_FILE")"
    NEW_KEY=$(gradatum-admin api-key create --owner claude-code --scope vault.* 2>/dev/null \
        | grep -oE 'ak_[a-zA-Z0-9_-]+' | head -1)
    if [ -z "$NEW_KEY" ]; then
        echo "  ERREUR : gradatum-admin n'a pas retourné d'api-key. Vérifier manuellement." >&2
        exit 1
    fi
    echo -n "$NEW_KEY" > "$API_KEY_FILE"
    chmod 600 "$API_KEY_FILE"
    chown gradatum:gradatum "$API_KEY_FILE" 2>/dev/null || true
    echo "  api-key créée : $API_KEY_FILE (chmod 600)"
fi
ok

# ── Step 5 : Génération mcp.json.sample ─────────────────────────────────────
step "Génération scripts/mcp.json.sample"

GENDATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cat > "$SCRIPT_DIR/mcp.json.sample" <<EOF
{
  "_comment": "Gradatum MCP server config — fragment à fusionner dans 'mcpServers' de ~/.claude.json (Claude Code) ou claude_desktop_config.json. Flow auto-refresh JWT (api-key fichier → /auth/exchange → JWT). Généré ${GENDATE} par install-gradatum-stub-mcp.sh.",
  "mcpServers": {
    "gradatum": {
      "command": "/usr/bin/gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "${SERVER_URL}",
        "GRADATUM_API_KEY_FILE": "${API_KEY_FILE}"
      }
    }
  }
}
EOF
chmod 0644 "$SCRIPT_DIR/mcp.json.sample"
echo "  Fichier généré : $SCRIPT_DIR/mcp.json.sample"
ok

# ── Step 6 : Test stdio handshake (smoke) ───────────────────────────────────
step "Smoke handshake stdio (rmcp initialize)"

# Test minimal : vérifier que le binaire répond à initialize sur stdin/stdout (timeout 3s).
# On envoie une JSON-RPC initialize et on vérifie qu'on reçoit du JSON valide en retour.
HANDSHAKE='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"1.0"}}}'
HANDSHAKE_RESPONSE=$(timeout 3 bash -c "
    GRADATUM_SERVER_URL='$SERVER_URL' \
    GRADATUM_API_KEY_FILE='$API_KEY_FILE' \
    /usr/bin/gradatum-mcp-stub <<< '$HANDSHAKE' 2>/dev/null | head -1" || echo "")

if [ -n "$HANDSHAKE_RESPONSE" ] && echo "$HANDSHAKE_RESPONSE" | grep -q '"jsonrpc"'; then
    echo "  Handshake OK : $(echo "$HANDSHAKE_RESPONSE" | head -c 80)…"
else
    echo "  AVERTISSEMENT : handshake KO (réponse vide ou non-JSON)." >&2
    echo "  Diagnostic possible : api-key invalide, server :19090 down, ou binaire incompatible." >&2
fi
ok

# ── Cleanup target/ post-install (opt-in via --cleanup-after) ────────────────
if [ "$DO_CLEANUP_AFTER" = true ] && [ "$DO_BUILD" = true ]; then
    echo ""
    echo "[cleanup] cargo clean (libération target/release et target/debug)…"
    TARGET_SIZE_BEFORE="$(du -sh "$REPO_DIR/target" 2>/dev/null | awk '{print $1}')"
    if [[ "$INVOKER_CARGO" == /root/* ]] || [[ -z "${SUDO_USER:-}" ]]; then
        "$INVOKER_CARGO" clean --manifest-path "$REPO_DIR/Cargo.toml" 2>&1 | tail -3 || true
    else
        sudo -u "$SUDO_USER" "$INVOKER_CARGO" clean --manifest-path "$REPO_DIR/Cargo.toml" 2>&1 | tail -3 || true
    fi
    echo "  target/ avant cleanup : ${TARGET_SIZE_BEFORE:-N/A}"
    echo "  Cleanup OK (binaire LIVE /usr/bin/gradatum-mcp-stub inchangé)"
fi

# ── Final ────────────────────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PASS — gradatum-mcp-stub installé"
echo ""
echo "  Étapes suivantes :"
echo "    1. Fusionner $SCRIPT_DIR/mcp.json.sample dans :"
echo "       - Claude Code  : ~/.claude.json (section \"mcpServers\")"
echo "       - Claude Desktop : ~/.config/Claude/claude_desktop_config.json"
echo "    2. Recharger le client MCP (kill+relaunch ou /reload)"
echo "    3. Vérifier : depuis Claude → /mcp list → 'gradatum' doit apparaître"
echo "    4. Logs stub : /usr/bin/gradatum-mcp-stub écrit sur stderr (capté par client MCP)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
