#!/usr/bin/env bash
# Regenerate legacy vault v1.6.2 snapshot DB for parity tests.
#
# Run on a host that has access to the legacy vault DB
# (typiquement : runner Forgejo Actions self-hosted, ou machine de développement).
# Ce script ne commite jamais le fichier produit — il est .gitignored.
#
# Usage :
#   bash crates/v1-parity-tests/scripts/regenerate-snapshot.sh
#
# Variables d'environnement :
#   VAULT_DB  Chemin vers la base source legacy vault. Par défaut :
#             ~/.memory-vault/.vault-index/vault.db

set -euo pipefail

VAULT_DB="${VAULT_DB:-$HOME/.memory-vault/.vault-index/vault.db}"
OUT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/tests/fixtures"
OUT_FILE="$OUT_DIR/legacy-vault-snapshot.db"

if [ ! -f "$VAULT_DB" ]; then
  echo "ERROR: legacy vault source DB not found at $VAULT_DB" >&2
  echo "Set VAULT_DB env to override." >&2
  exit 1
fi

if ! command -v sqlite3 &>/dev/null; then
  echo "ERROR: sqlite3 not found in PATH" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

# sqlite3 .backup utilise l'API Online Backup — safe même si le vault source tourne.
sqlite3 "$VAULT_DB" ".backup '$OUT_FILE'"
chmod 0444 "$OUT_FILE"

SIZE=$(du -h "$OUT_FILE" | cut -f1)
TABLES=$(sqlite3 "$OUT_FILE" ".tables" | tr -s ' ' '\n' | grep -c '.')

echo "Snapshot régénéré : $OUT_FILE"
echo "  taille  : $SIZE"
echo "  tables  : $TABLES"
echo ""
echo "Le fichier est .gitignored — ne PAS le commiter."
