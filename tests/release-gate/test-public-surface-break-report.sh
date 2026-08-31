#!/usr/bin/env bash
# Carte F-145 (jalon 2.1.0) — critère : « un test vérifie qu'un consommateur épinglé
# sur la version précédente reçoit un rapport de rupture NON VIDE ».
#
# Le dispositif qui VOIT la rupture est le diff de la baseline de surface publique
# (`cargo public-api`) — PAS `cargo-semver-checks`. Mesuré trois fois le 2026-08-25/26
# sur les changements F-145 réels (sous-lots 1/2/3 : champs de variantes
# `#[non_exhaustive]` qui changent de type) : cargo-semver-checks rend
# « no semver update required » (rapport VIDE) alors que la baseline `public-api`
# rend un diff NON VIDE — ruptures de compilation certaines.
#
# Pourquoi la garde semver ne convient pas : `#[non_exhaustive]` sur une énumération
# autorise l'ajout de variantes SANS rupture SemVer, et l'outil traite le changement
# de type d'un champ de variante de la même manière (une variante est « opaque » pour
# lui) — alors que tout consommateur qui match la variante cesse de compiler.
#
# Ce test reproduit la classe de rupture sur un FIXTURE autonome (avant/après, dans
# `fixtures/public-surface-break-{before,after}`) et vérifie que le diff de surface
# publique est NON VIDE — le rapport que reçoit un consommateur épinglé sur l'avant.
#
# Codes de sortie :
#   0  rapport NON VIDE prouvé (le diff de surface est non vide)
#   1  rapport VIDE (le dispositif n'a rien vu — la garde est aveugle)
#   2  gate NON EXÉCUTÉ (nightly absent, fixture introuvable) — jamais un PASS

set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BEFORE="$HERE/fixtures/public-surface-break-before"
AFTER="$HERE/fixtures/public-surface-break-after"

# Répertoire temporaire PRIVÉ (`mktemp -d`), jamais de chemins fixes sous /tmp :
# des noms prévisibles dans un répertoire monde-accessible sont une surface
# d'attaque par symlink. Chaque exécution écrit dans un répertoire neuf,
# détruit à la sortie (trap EXIT, y compris sur exit 1/2).
TMPDIR_RUN="$(mktemp -d)" || { echo "ERREUR : mktemp -d a échoué" >&2; exit 2; }
trap 'rm -rf "$TMPDIR_RUN"' EXIT

# Capacité de verdict : sans nightly, pas de rustdoc JSON, donc pas de gate.
# lint-toolchain-pin: allow nightly — rustdoc JSON indisponible hors nightly
if ! cargo +nightly --version >/dev/null 2>&1; then
  echo "ERREUR : toolchain nightly absente — \`cargo public-api\` indisponible (exit 2, jamais un PASS)" >&2
  exit 2
fi
[ -f "$BEFORE/Cargo.toml" ] && [ -f "$AFTER/Cargo.toml" ] || {
  echo "ERREUR : fixtures introuvables ($BEFORE / $AFTER)" >&2
  exit 2
}

# Surface publique AVANT (consommateur épinglé sur la version précédente).
# lint-toolchain-pin: allow nightly — rustdoc JSON indisponible hors nightly
cargo +nightly public-api --manifest-path "$BEFORE/Cargo.toml" 2>/dev/null > "$TMPDIR_RUN/before.txt"
# Surface publique APRÈS.
# lint-toolchain-pin: allow nightly — rustdoc JSON indisponible hors nightly
cargo +nightly public-api --manifest-path "$AFTER/Cargo.toml" 2>/dev/null > "$TMPDIR_RUN/after.txt"

# Extraction NON VIDE prouvée AVANT de lire un verdict : les deux surfaces doivent
# exister et être non vides (sinon le diff vide ne prouve rien).
[ -s "$TMPDIR_RUN/before.txt" ] || { echo "ERREUR : surface AVANT vide" >&2; exit 2; }
[ -s "$TMPDIR_RUN/after.txt" ]  || { echo "ERREUR : surface APRÈS vide" >&2; exit 2; }

# Le rapport : diff entre les deux surfaces. Un consommateur épinglé sur AVANT reçoit
# ce diff comme rapport de rupture.
if diff "$TMPDIR_RUN/before.txt" "$TMPDIR_RUN/after.txt" > "$TMPDIR_RUN/report.txt"; then
  echo "FAIL : rapport de rupture VIDE — le dispositif public-api n'a rien vu"
  exit 1
fi

echo "OK : rapport de rupture NON VIDE ($(wc -l < "$TMPDIR_RUN/report.txt") lignes de diff)"
echo "Rapport (ce que reçoit un consommateur épinglé sur la version précédente) :"
cat "$TMPDIR_RUN/report.txt"
exit 0
