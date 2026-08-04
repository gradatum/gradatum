#!/usr/bin/env bash
# Gate `public-api` — surface publique de TOUTE crate publiable du workspace.
#
# Un seul mécanisme, deux modes, pour que le contrôle local et celui de la CI ne
# puissent pas diverger : la CI appelle ce script, elle ne réimplémente pas la
# commande. (Un commentaire « SYNC » n'est pas un mécanisme de synchronisation.)
#
#   ./public-api/regen.sh --write    régénère les baselines commitées
#   ./public-api/regen.sh --check    échoue si la surface a bougé sans re-baseline
#
# Codes de sortie :
#   0  surface identique à la baseline commitée
#   1  écart de surface (le diff intégral est imprimé, jamais tronqué)
#   2  gate NON EXÉCUTÉ (nightly absent, cargo metadata KO, périmètre vide,
#      baseline manquante ou orpheline) — ce n'est jamais un PASS
#
# Périmètre : les crates du workspace qui sont (a) publiables et (b) porteuses
# d'une cible `lib`. La liste est calculée par `cargo metadata`, jamais par grep :
# un membre sans clé `publish` explicite est publiable par défaut.
#
# BORNE CONNUE, ASSUMÉE : `cargo public-api` lit le rustdoc JSON, qui n'émet pas
# les items `#[doc(hidden)]`. Ces items restent appelables par un consommateur et
# un changement dessus reste une rupture SemVer — ce gate ne les voit pas.
# `gradatum-gateway` (630 items masqués depuis ALIGN-SURFACE), `gradatum-studio`,
# `gradatum-worker` et `gradatum-admin` sont donc mesurés à 1 item : leur surface
# réelle n'est pas couverte ici.
#
# SECONDE BORNE, mesurée : `--omit auto-derived-impls` masque aussi les `impl From`
# générés par `#[derive(Error)]` + `#[from]`. Le retrait de
# `impl From<serde_yml::Error> for MarkdownError` (lot I-013) est invisible dans
# cette baseline. Détail et contre-exemple : public-api/README.md.

set -uo pipefail

MODE="${1:---check}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="$ROOT/public-api/baseline"
INDEX="$BASE/_INDEX.tsv"

# `--all-features` et non les features par défaut : la surface atteignable par un
# consommateur inclut les items derrière des features opt-in. Mesuré le
# 2026-07-30 : la mesure par défaut manque 198 items, dont 138 pour
# `gradatum-engine` (feature `serve`), soit 98,6 % de cette crate.
OMIT=(--omit blanket-impls --omit auto-trait-impls --omit auto-derived-impls)
FEATURES=(--all-features)

cd "$ROOT" || exit 2

case "$MODE" in
  --write|--check) ;;
  *) echo "usage: $0 [--write|--check]" >&2; exit 2 ;;
esac

# --- Capacité de verdict : sans nightly, pas de rustdoc JSON, donc pas de gate ---
# Ceci n'est pas un pin concurrent de rust-toolchain.toml mais un test de
# disponibilité : sans nightly le gate refuse de tourner (exit 2) plutôt que de
# rendre un faux vert. Le marqueur doit rester sur la ligne juste au-dessus.
# lint-toolchain-pin: allow nightly — rustdoc JSON indisponible hors nightly
if ! cargo +nightly --version >/dev/null 2>&1; then
  echo "ERREUR : toolchain nightly absente — \`cargo public-api\` ne peut pas produire" >&2
  echo "         de rustdoc JSON. Gate NON EXÉCUTÉ, ce n'est pas un PASS." >&2
  exit 2
fi
if ! command -v cargo-public-api >/dev/null 2>&1; then
  echo "ERREUR : cargo-public-api introuvable. Gate NON EXÉCUTÉ, ce n'est pas un PASS." >&2
  exit 2
fi

# --- Périmètre, établi par commande ---
CRATES="$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[]
           | select(.publish == null)
           | select([.targets[].kind[]] | any(. == "lib" or . == "rlib" or . == "proc-macro"))
           | .name' | sort)"
if [ -z "$CRATES" ]; then
  echo "ERREUR : périmètre vide — 0 crate publiable détectée. Gate NON EXÉCUTÉ." >&2
  exit 2
fi
N=$(printf '%s\n' "$CRATES" | wc -l)

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$BASE"

FAIL=0
TOTAL=0
: > "$TMP/_INDEX.tsv"

echo "public-api : $N crates publiables avec cible lib (--all-features, 3 --omit)"

for c in $CRATES; do
  # stdout capturé, stderr laissé passer : une mesure ne masque pas son stderr.
  cargo public-api -p "$c" "${OMIT[@]}" "${FEATURES[@]}" > "$TMP/$c.txt"
  rc=$?   # code du gate lui-même, lu immédiatement, jamais celui d'un wrapper
  if [ "$rc" -ne 0 ]; then
    echo "ERREUR : \`cargo public-api -p $c\` a rendu rc=$rc — surface NON mesurée." >&2
    exit 2
  fi
  items=$(wc -l < "$TMP/$c.txt")
  TOTAL=$((TOTAL + items))
  printf '%s\t%d\n' "$c" "$items" >> "$TMP/_INDEX.tsv"
done
printf 'TOTAL\t%d\n' "$TOTAL" >> "$TMP/_INDEX.tsv"
printf 'CRATES\t%d\n' "$N" >> "$TMP/_INDEX.tsv"

if [ "$MODE" = "--write" ]; then
  rm -f "$BASE"/*.txt "$INDEX"
  for c in $CRATES; do cp "$TMP/$c.txt" "$BASE/$c.txt"; done
  cp "$TMP/_INDEX.tsv" "$INDEX"
  echo "baselines écrites : $N crates / $TOTAL items → public-api/baseline/"
  exit 0
fi

# --- Mode --check ---

# Une crate publiable nouvellement ajoutée doit faire échouer le gate, sinon elle
# entre dans la surface publiée sans jamais être mesurée — le mode de défaillance
# que ce gate existe pour fermer.
for c in $CRATES; do
  if [ ! -f "$BASE/$c.txt" ]; then
    echo "ERREUR : aucune baseline pour la crate publiable \`$c\`." >&2
    echo "         Lancer \`./public-api/regen.sh --write\` et commiter le résultat." >&2
    FAIL=2
  fi
done
for f in "$BASE"/*.txt; do
  [ -e "$f" ] || continue
  b="$(basename "$f" .txt)"
  if ! printf '%s\n' "$CRATES" | grep -qx -- "$b"; then
    echo "ERREUR : baseline orpheline \`$b.txt\` — crate absente du périmètre publiable." >&2
    FAIL=2
  fi
done
[ "$FAIL" -eq 2 ] && exit 2

for c in $CRATES; do
  if ! diff -u "$BASE/$c.txt" "$TMP/$c.txt" \
        --label "baseline/$c.txt" --label "mesure/$c.txt"; then
    echo "--- écart de surface publique : $c ---"
    FAIL=1
  fi
done
if ! diff -u "$INDEX" "$TMP/_INDEX.tsv" --label "baseline/_INDEX.tsv" --label "mesure/_INDEX.tsv"; then
  FAIL=1
fi

if [ "$FAIL" -ne 0 ]; then
  cat >&2 <<'EOF'

La surface publique a changé sans que la baseline soit remise à jour.
Ce n'est pas un interdit : c'est une rupture à assumer explicitement.
  1. vérifier que le diff ci-dessus est voulu ;
  2. le documenter au CHANGELOG s'il est breaking ;
  3. `./public-api/regen.sh --write` puis commiter public-api/baseline/.
EOF
  exit 1
fi

echo "public-api OK : $N crates / $TOTAL items, identiques à la baseline commitée"
exit 0
