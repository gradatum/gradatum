#!/usr/bin/env bash
#
# publish-stubs-v0.0.1.sh — Publication séquentielle des 26 stubs v0.0.1 sur crates.io.
#
# Chaque stub contient un README enrichi avec les signatures publiques principales.
# Le code source réel reste PRIVÉ (D5 criterion — repo public au tag v1.0 uniquement).
#
# Prérequis :
#   - bash scripts/gen-stubs-v0.0.1.sh  (générer les stubs d'abord)
#   - Un token crates.io avec scope "publish-update" (les noms sont déjà réservés)
#     Obtenir sur https://crates.io/me → API Tokens → New Token → scope: publish-update
#
# Usage :
#   export CRATES_IO_TOKEN=<token>
#   bash scripts/publish-stubs-v0.0.1.sh
#
# ou sans env var (prompt interactif) :
#   bash scripts/publish-stubs-v0.0.1.sh
#
# Rate limit crates.io : 90s entre chaque publish (en cas de 429 → 120s).
# Temps total estimé : 26 × 90s ≈ 39 minutes.
#
# Idempotent : si un crate est déjà à v0.0.1, il sera skippé (cargo publish retourne
# une erreur "version already exists" — le script la détecte et continue).

set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STUBS_DIR="${WORKSPACE_ROOT}/crates-publish-stubs"

# Ordre topologique : les crates sans dépendances en premier.
# Ici tous les stubs sont indépendants (lib.rs minimal, aucune dépendance déclarée).
# Ordre choisi : core d'abord, binaires à la fin.
CRATES=(
    "gradatum-core"
    "gradatum-markdown"
    "gradatum-storage"
    "gradatum-index"
    "gradatum-cache"
    "gradatum-embed"
    "gradatum-chat"
    "gradatum-curator"
    "gradatum-auth"
    "gradatum-acl-policy"
    "gradatum-acl-auth"
    "gradatum-queue"
    "gradatum-search"
    "gradatum-engine"
    "gradatum-vault"
    "gradatum-sdk-rs"
    "gradatum-distill"
    "gradatum-mcp"
    "gradatum-mcp-stub"
    "gradatum-protocol"
    "gradatum-studio"
    "gradatum-server"
    "gradatum-worker"
    "gradatum-admin"
    "gradatum-cli"
    "gradatum"
)

readonly RATE_LIMIT_SLEEP_S=90
readonly RATE_LIMIT_SLEEP_429_S=180

# ─── Vérifications préliminaires ──────────────────────────────────────────────

if [[ ! -d "$STUBS_DIR" ]]; then
    echo "ERROR: ${STUBS_DIR} absent. Exécuter d'abord : bash scripts/gen-stubs-v0.0.1.sh" >&2
    exit 1
fi

stubs_count=$(ls "$STUBS_DIR" | wc -l)
if [[ "$stubs_count" -lt 26 ]]; then
    echo "ERROR: seulement ${stubs_count} stubs dans ${STUBS_DIR} (26 attendus)." >&2
    echo "       Exécuter : bash scripts/gen-stubs-v0.0.1.sh" >&2
    exit 1
fi

command -v cargo >/dev/null 2>&1 || { echo "ERROR: cargo not found" >&2; exit 1; }

# ─── Token crates.io ──────────────────────────────────────────────────────────

if [[ -n "${CRATES_IO_TOKEN:-}" ]]; then
    TOKEN="$CRATES_IO_TOKEN"
    echo "==> Token depuis \$CRATES_IO_TOKEN (prompt skippé)."
else
    read -rsp "Token crates.io (scope: publish-update) — input masqué, puis Entrée : " TOKEN
    echo
fi

if [[ -z "${TOKEN:-}" ]]; then
    echo "ERROR: token vide." >&2
    exit 1
fi

if [[ ! "$TOKEN" =~ ^cio ]]; then
    echo "WARNING: token ne commence pas par 'cio'. Est-ce bien un token crates.io ?"
    read -rp "Continuer quand même ? (yes/no) " confirm
    [[ "$confirm" == "yes" ]] || { echo "Abandon."; exit 1; }
fi

# ─── CARGO_HOME isolé (bypass proxy private registry) ───────────────────────────────────

ORIG_CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
ISOLATED_CARGO_HOME="$(mktemp -d -t stubs-cargo-XXXXXX)"
export CARGO_HOME="$ISOLATED_CARGO_HOME"

cleanup() {
    rm -rf "$ISOLATED_CARGO_HOME"
    export CARGO_HOME="$ORIG_CARGO_HOME"
    unset TOKEN 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# config.toml vide = pas de replace-with, crates.io natif
: > "$CARGO_HOME/config.toml"

# credentials.toml sous [registry] = token par défaut crates-io
{
    echo "[registry]"
    echo "token = \"${TOKEN}\""
} > "$CARGO_HOME/credentials.toml"
chmod 600 "$CARGO_HOME/credentials.toml"
unset TOKEN

echo "==> CARGO_HOME isolé : $CARGO_HOME (proxy private registry bypassé)"
echo "==> ${#CRATES[@]} crates à publier en v0.0.1"
echo "==> Rate limit : ${RATE_LIMIT_SLEEP_S}s entre chaque (total estimé : $((${#CRATES[@]} * RATE_LIMIT_SLEEP_S / 60)) min)"
echo

# ─── Phase 1 : Dry-run de validation ──────────────────────────────────────────

echo "==> Phase 1 : Dry-run validation..."
ok_count=0
fail_count=0
fail_names=()

for name in "${CRATES[@]}"; do
    dir="${STUBS_DIR}/${name}"
    if [[ ! -d "$dir" ]]; then
        echo "  MISSING: ${name} (${dir} absent)"
        fail_count=$((fail_count + 1))
        fail_names+=("$name")
        continue
    fi
    printf "  %-30s " "${name}..."
    if (cd "$dir" && cargo publish --registry crates-io --dry-run --allow-dirty --quiet 2>/dev/null); then
        printf "OK\n"
        ok_count=$((ok_count + 1))
    else
        # Vérifier si c'est juste "already uploaded" (version existante)
        err=$(cd "$dir" && cargo publish --registry crates-io --dry-run --allow-dirty 2>&1 || true)
        if echo "$err" | grep -q "already uploaded"; then
            printf "SKIP (déjà publié)\n"
            ok_count=$((ok_count + 1))
        else
            printf "FAIL\n"
            echo "$err" | head -5 | sed 's/^/    /'
            fail_count=$((fail_count + 1))
            fail_names+=("$name")
        fi
    fi
done

echo
echo "==> Dry-run : ${ok_count} OK / ${fail_count} FAIL"

if [[ ${fail_count} -gt 0 ]]; then
    echo "Crates en échec :"
    for n in "${fail_names[@]}"; do
        echo "  - ${n}"
    done
    echo
    read -rp "Des échecs détectés. Continuer quand même les crates OK ? (yes/no) " confirm
    [[ "$confirm" == "yes" ]] || { echo "Abandon."; exit 1; }
fi

# ─── Gate de confirmation ──────────────────────────────────────────────────────

cat <<'EOF'

==> Prêt à publier 26 stubs v0.0.1 sur crates.io.

IMPORTANT :
  - cargo publish est IRRÉVERSIBLE (les versions ne se suppriment pas).
  - Ces stubs remplacent les placeholders v0.0.0 avec README + signatures publiques.
  - Le code source réel reste PRIVÉ (D5 criterion).

EOF

read -rp "Taper exactement 'PUBLISH' pour confirmer (autre = abandon) : " confirm
if [[ "$confirm" != "PUBLISH" ]]; then
    echo "Abandon par l'utilisateur."
    exit 0
fi

# ─── Phase 2 : Publication séquentielle ───────────────────────────────────────

echo
echo "==> Phase 2 : Publication séquentielle (${RATE_LIMIT_SLEEP_S}s entre chaque)..."
echo

publish_ok=0
publish_skip=0
publish_fail=0
publish_fail_names=()

for name in "${CRATES[@]}"; do
    dir="${STUBS_DIR}/${name}"
    printf "  [%-28s] " "${name}..."

    if [[ ! -d "$dir" ]]; then
        printf "SKIP (répertoire absent)\n"
        publish_skip=$((publish_skip + 1))
        continue
    fi

    # Publication
    publish_output=$(cd "$dir" && cargo publish --registry crates-io --allow-dirty 2>&1 || true)
    exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        printf "OK v0.0.1\n"
        publish_ok=$((publish_ok + 1))
    elif echo "$publish_output" | grep -q "already uploaded"; then
        printf "SKIP (v0.0.1 déjà publiée)\n"
        publish_skip=$((publish_skip + 1))
    elif echo "$publish_output" | grep -q "429\|rate limit\|too many"; then
        printf "RATE LIMIT — attente ${RATE_LIMIT_SLEEP_429_S}s...\n"
        sleep "$RATE_LIMIT_SLEEP_429_S"
        # Réessai
        if (cd "$dir" && cargo publish --registry crates-io --allow-dirty --quiet 2>/dev/null); then
            printf "  %-30s OK v0.0.1 (retry)\n" "${name}"
            publish_ok=$((publish_ok + 1))
        else
            printf "  %-30s FAIL (même après retry)\n" "${name}"
            echo "$publish_output" | head -5 | sed 's/^/    /'
            publish_fail=$((publish_fail + 1))
            publish_fail_names+=("$name")
        fi
    else
        printf "FAIL\n"
        echo "$publish_output" | head -8 | sed 's/^/    /'
        publish_fail=$((publish_fail + 1))
        publish_fail_names+=("$name")
    fi

    # Rate limit delay (sauf après le dernier)
    if [[ "$name" != "${CRATES[-1]}" ]]; then
        echo "    sleeping ${RATE_LIMIT_SLEEP_S}s..."
        sleep "$RATE_LIMIT_SLEEP_S"
    fi
done

# ─── Rapport final ────────────────────────────────────────────────────────────

echo
echo "==================================================================="
echo "==> RAPPORT PUBLICATION v0.0.1"
echo "==================================================================="
echo "  Publiés avec succès : ${publish_ok}"
echo "  Skippés (déjà v0.0.1) : ${publish_skip}"
echo "  Échecs : ${publish_fail}"
echo

if [[ ${publish_fail} -gt 0 ]]; then
    echo "Crates en échec :"
    for n in "${publish_fail_names[@]}"; do
        echo "  - https://crates.io/crates/${n}"
    done
    echo
    exit 1
fi

echo "==> Toutes les publications réussies."
echo
echo "Vérification :"
echo "  https://crates.io/search?q=gradatum"
echo
echo "Vérification unitaire (exemples) :"
echo "  curl -s https://crates.io/api/v1/crates/gradatum-core | jq '.crate.max_version,.crate.description'"
echo "  curl -s https://crates.io/api/v1/crates/gradatum | jq '.crate.max_version,.crate.description'"
